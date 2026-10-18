use std::ops::Range;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use lodestore_format::store::{BlobError, BlobStore, ListEntry};
use tokio::time::Instant;

const NANOS: u128 = 1_000_000_000;
const CREDIT_STEP: Duration = Duration::from_millis(250);

/// Caps how fast objects are written. Every `put_if_absent` reserves a slot of
/// `len / rate` after the previous one and waits for the slot to end before
/// writing, so the bytes are paid for ahead and the average never exceeds the
/// rate. While a put waits for its slot, its bytes are counted as uploaded bit
/// by bit, so the upload speed sampled from the counter stays at the rate
/// instead of jumping by a whole pack; a failed put takes them back. Only
/// [`BlobStore`] is implemented: the wrapper adds no way to delete.
pub struct Throttled<S> {
    inner: S,
    rate: Option<u64>,
    next_free: Mutex<Instant>,
    uploaded: Arc<AtomicU64>,
}

impl<S> Throttled<S> {
    pub fn new(inner: S, rate: Option<u64>, uploaded: Arc<AtomicU64>) -> Self {
        Throttled {
            inner,
            rate: rate.filter(|r| *r > 0),
            next_free: Mutex::new(Instant::now()),
            uploaded,
        }
    }

    fn reserve(&self, len: u64, rate: u64) -> (Instant, Instant) {
        let nanos = (u128::from(len) * NANOS).div_ceil(u128::from(rate));
        let slot = Duration::new(
            u64::try_from(nanos / NANOS).unwrap_or(u64::MAX),
            (nanos % NANOS) as u32,
        );
        let mut next_free = self.next_free.lock().unwrap_or_else(|e| e.into_inner());
        let start = (*next_free).max(Instant::now());
        let end = start.checked_add(slot).unwrap_or(start);
        *next_free = end;
        (start, end)
    }

    /// Sleeps until the slot ends and counts `len` bytes as uploaded in
    /// proportion to how much of the slot has passed.
    async fn pay(&self, len: u64, start: Instant, end: Instant) {
        let slot = end.duration_since(start).as_nanos().max(1);
        let mut credited = 0;
        loop {
            let now = Instant::now();
            let passed = now.saturating_duration_since(start).as_nanos().min(slot);
            let due = u64::try_from(u128::from(len) * passed / slot).unwrap_or(len);
            self.uploaded.fetch_add(due - credited, Ordering::Relaxed);
            credited = due;
            if now >= end {
                break;
            }
            tokio::time::sleep_until(end.min(now.max(start) + CREDIT_STEP)).await;
        }
    }
}

impl<S: BlobStore> BlobStore for Throttled<S> {
    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
        let len = data.len() as u64;
        let Some(rate) = self.rate else {
            self.inner.put_if_absent(key, data).await?;
            self.uploaded.fetch_add(len, Ordering::Relaxed);
            return Ok(());
        };
        let (start, end) = self.reserve(len, rate);
        self.pay(len, start, end).await;
        let result = self.inner.put_if_absent(key, data).await;
        if result.is_err() {
            self.uploaded.fetch_sub(len, Ordering::Relaxed);
        }
        result
    }

    async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
        self.inner.get(key).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
        self.inner.get_range(key, range).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
        self.inner.list(prefix).await
    }

    async fn ensure_prefixes(&self, prefixes: &[&str]) -> Result<(), BlobError> {
        self.inner.ensure_prefixes(prefixes).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const MIB: usize = 1 << 20;

    #[derive(Default)]
    struct Mem(Mutex<HashMap<String, Bytes>>);

    impl BlobStore for Mem {
        async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<(), BlobError> {
            let mut map = self.0.lock().unwrap();
            if map.contains_key(key) {
                return Err(BlobError::AlreadyExists);
            }
            if key.starts_with("bad") {
                return Err(BlobError::backend("refused"));
            }
            map.insert(key.to_string(), data);
            Ok(())
        }

        async fn get(&self, key: &str) -> Result<Bytes, BlobError> {
            self.0
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or(BlobError::NotFound)
        }

        async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes, BlobError> {
            Ok(self
                .get(key)
                .await?
                .slice(range.start as usize..range.end as usize))
        }

        async fn list(&self, _prefix: &str) -> Result<Vec<ListEntry>, BlobError> {
            Ok(Vec::new())
        }
    }

    fn counter() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_average_within_limit() {
        let uploaded = counter();
        let store = Throttled::new(Mem::default(), Some(2 * MIB as u64), uploaded.clone());
        let start = Instant::now();
        for i in 0..10 {
            store
                .put_if_absent(&format!("k{i}"), Bytes::from(vec![0u8; MIB]))
                .await
                .unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(4500) && elapsed <= Duration::from_secs(5),
            "{elapsed:?}"
        );
        assert_eq!(uploaded.load(Ordering::Relaxed), 10 * MIB as u64);
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_no_rate_passes_through() {
        let uploaded = counter();
        let store = Throttled::new(Mem::default(), None, uploaded.clone());
        let start = Instant::now();
        for i in 0..5 {
            store
                .put_if_absent(&format!("k{i}"), Bytes::from(vec![0u8; MIB]))
                .await
                .unwrap();
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(uploaded.load(Ordering::Relaxed), 5 * MIB as u64);
        assert_eq!(store.get("k3").await.unwrap().len(), MIB);
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_no_overflow_tiny_rate() {
        let store = Throttled::new(Mem::default(), Some(1024), counter());
        let start = Instant::now();
        store
            .put_if_absent("a", Bytes::from(vec![0u8; 32 * MIB]))
            .await
            .unwrap();
        store
            .put_if_absent("b", Bytes::from_static(b"x"))
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_secs(32 * 1024)
                && elapsed < Duration::from_secs(32 * 1024 + 1),
            "{elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_counts_only_success() {
        let uploaded = counter();
        let store = Throttled::new(Mem::default(), Some(MIB as u64), uploaded.clone());
        store
            .put_if_absent("a", Bytes::from_static(b"1234"))
            .await
            .unwrap();
        assert!(matches!(
            store.put_if_absent("a", Bytes::from_static(b"1234")).await,
            Err(BlobError::AlreadyExists)
        ));
        assert!(
            store
                .put_if_absent("bad", Bytes::from_static(b"1234"))
                .await
                .is_err()
        );
        assert_eq!(uploaded.load(Ordering::Relaxed), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn throttle_counts_bytes_while_waiting() {
        let uploaded = counter();
        let store = Arc::new(Throttled::new(
            Mem::default(),
            Some(2 * MIB as u64),
            uploaded.clone(),
        ));
        let put = tokio::spawn({
            let store = Arc::clone(&store);
            async move {
                store
                    .put_if_absent("pack", Bytes::from(vec![0u8; 32 * MIB]))
                    .await
            }
        });
        let mut last = 0;
        for second in 1..=15u64 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let now = uploaded.load(Ordering::Relaxed);
            let step = now - last;
            assert!(
                step >= 3 * MIB as u64 / 2 && step <= 5 * MIB as u64 / 2,
                "second {second}: {step}"
            );
            last = now;
        }
        put.await.unwrap().unwrap();
        assert_eq!(uploaded.load(Ordering::Relaxed), 32 * MIB as u64);
    }
}
