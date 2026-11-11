//! Reading pack frames for a restore: straight from the bucket through the
//! temporary link the server issued, or through the server when it cannot
//! hand out links. The agent never reads the bucket with its own key.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use lodestore_format::crypto::Key32;
use lodestore_proto::agent::v1 as pb;
use lodestore_proto::agent::v1::agent_client::AgentClient;
use tonic::{Code, Status};
use zeroize::Zeroizing;

use crate::NetOptions;
use crate::shared_channel::SharedChannel;

pub const MAX_RANGE: u64 = 8 << 20;
pub const MAX_GAP: u64 = 64 << 10;
pub const REFRESH_MARGIN_MS: i64 = 60_000;
const ATTEMPTS: u32 = 3;
const PAUSE: Duration = Duration::from_millis(250);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// How to read one pack: its key and, when the bucket can sign links, a
/// temporary URL. Both are secrets, so neither reaches `Debug` or the logs.
pub struct PackAccess {
    pub pack: u32,
    pub pack_id: Vec<u8>,
    pub key: Key32,
    pub url: Option<Zeroizing<String>>,
    pub expires_at_ms: i64,
}

impl PackAccess {
    /// Takes the secrets out of the message and wipes the copies it held.
    pub fn from_proto(mut p: pb::PackAccess) -> Result<Self, String> {
        let mut raw = std::mem::take(&mut p.key);
        let key = <[u8; 32]>::try_from(raw.as_slice())
            .map(Zeroizing::new)
            .map_err(|_| format!("the key of pack {} is not 32 bytes", p.pack));
        zeroize::Zeroize::zeroize(&mut raw);
        let url = Zeroizing::new(std::mem::take(&mut p.url));
        Ok(PackAccess {
            pack: p.pack,
            pack_id: p.pack_id,
            key: key?,
            url: (!url.is_empty()).then_some(url),
            expires_at_ms: p.expires_at_ms,
        })
    }

    /// The link is gone or about to be.
    pub fn expiring(&self, now_ms: i64) -> bool {
        self.url.is_some()
            && self.expires_at_ms != 0
            && self.expires_at_ms - now_ms < REFRESH_MARGIN_MS
    }
}

impl fmt::Debug for PackAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackAccess")
            .field("pack", &self.pack)
            .field("has_url", &self.url.is_some())
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("access to the pack was refused")]
    Forbidden,
    #[error("the pack is missing from storage")]
    NotFound,
    #[error("{0}")]
    Failed(String),
}

#[allow(async_fn_in_trait)]
pub trait PackFetcher {
    /// Exactly `len` bytes of the pack from `offset`.
    async fn fetch(
        &self,
        access: &PackAccess,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FetchError>;

    /// New access for packs whose links expired or were refused.
    async fn refresh(&self, packs: &[u32]) -> Result<Vec<PackAccess>, FetchError>;
}

/// A run of frames of one pack read with one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub pack: u32,
    pub start: u64,
    pub len: u64,
    /// The chunks of the file, by position, that lie in this span.
    pub chunks: Range<usize>,
}

/// Groups consecutive chunks of one pack into ranges of at most
/// [`MAX_RANGE`], reading over gaps of up to [`MAX_GAP`] bytes.
pub fn coalesce(locs: &[pb::ChunkLoc]) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    for (i, c) in locs.iter().enumerate() {
        let start = u64::from(c.offset);
        let end = start + u64::from(c.enc_len);
        if let Some(cur) = out.last_mut() {
            let cur_end = cur.start + cur.len;
            if cur.pack == c.pack
                && start >= cur_end
                && start - cur_end <= MAX_GAP
                && end - cur.start <= MAX_RANGE
            {
                cur.len = end - cur.start;
                cur.chunks.end = i + 1;
                continue;
            }
        }
        out.push(Span {
            pack: c.pack,
            start,
            len: end - start,
            chunks: i..i + 1,
        });
    }
    out
}

fn jittered(base: Duration) -> Duration {
    let mut byte = [0u8; 1];
    let _ = getrandom::fill(&mut byte);
    base.mul_f64(0.5 + f64::from(byte[0]) / 255.0)
}

enum Attempt {
    Retry(String),
    Stop(FetchError),
}

async fn retrying<T, Fut>(pause: Duration, mut once: impl FnMut() -> Fut) -> Result<T, FetchError>
where
    Fut: Future<Output = Result<T, Attempt>>,
{
    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match once().await {
            Ok(v) => return Ok(v),
            Err(Attempt::Stop(e)) => return Err(e),
            Err(Attempt::Retry(why)) => last = why,
        }
        if attempt < ATTEMPTS {
            tokio::time::sleep(jittered(pause * attempt)).await;
        }
    }
    Err(FetchError::Failed(format!(
        "{last} (gave up after {ATTEMPTS} attempts)"
    )))
}

/// Reads ranges from the temporary links with plain HTTP `Range` requests.
pub struct HttpFetcher {
    client: reqwest::Client,
    pause: Duration,
}

impl HttpFetcher {
    pub fn new(net: &NetOptions) -> Result<Self, FetchError> {
        let bad = |e: reqwest::Error| FetchError::Failed(format!("cannot set up HTTP: {e}"));
        let no_proxy = net
            .no_proxy
            .as_deref()
            .and_then(reqwest::NoProxy::from_string);
        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .no_proxy();
        if let Some(url) = net.https_proxy.as_ref().or(net.http_proxy.as_ref()) {
            builder = builder.proxy(
                reqwest::Proxy::https(url.as_str())
                    .map_err(bad)?
                    .no_proxy(no_proxy.clone()),
            );
        }
        if let Some(url) = &net.http_proxy {
            builder = builder.proxy(
                reqwest::Proxy::http(url.as_str())
                    .map_err(bad)?
                    .no_proxy(no_proxy),
            );
        }
        Ok(HttpFetcher {
            client: builder.build().map_err(bad)?,
            pause: PAUSE,
        })
    }

    pub fn with_pause(mut self, pause: Duration) -> Self {
        self.pause = pause;
        self
    }

    async fn once(&self, url: &str, offset: u64, len: u64) -> Result<Vec<u8>, Attempt> {
        let last = offset + len - 1;
        let response = self
            .client
            .get(url)
            .header(reqwest::header::RANGE, format!("bytes={offset}-{last}"))
            .send()
            .await
            .map_err(|e| Attempt::Retry(e.without_url().to_string()))?;
        let status = response.status();
        match status {
            reqwest::StatusCode::PARTIAL_CONTENT => {}
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::UNAUTHORIZED => {
                return Err(Attempt::Stop(FetchError::Forbidden));
            }
            reqwest::StatusCode::NOT_FOUND => return Err(Attempt::Stop(FetchError::NotFound)),
            s if s.is_server_error() || s == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                return Err(Attempt::Retry(format!("storage answered {s}")));
            }
            s => {
                return Err(Attempt::Stop(FetchError::Failed(format!(
                    "storage answered {s} to a range request"
                ))));
            }
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| Attempt::Retry(e.without_url().to_string()))?;
        if body.len() as u64 != len {
            return Err(Attempt::Retry(format!(
                "storage returned {} bytes, expected {len}",
                body.len()
            )));
        }
        Ok(body.to_vec())
    }
}

impl PackFetcher for HttpFetcher {
    async fn fetch(
        &self,
        access: &PackAccess,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FetchError> {
        let Some(url) = access.url.as_deref() else {
            return Err(FetchError::Failed("the pack has no link".into()));
        };
        if len == 0 {
            return Ok(Vec::new());
        }
        retrying(self.pause, || self.once(url, offset, len)).await
    }

    async fn refresh(&self, _: &[u32]) -> Result<Vec<PackAccess>, FetchError> {
        Err(FetchError::Failed("links cannot be refreshed here".into()))
    }
}

fn status_error(status: &Status) -> Attempt {
    match status.code() {
        Code::NotFound => Attempt::Stop(FetchError::NotFound),
        Code::PermissionDenied | Code::Unauthenticated => Attempt::Stop(FetchError::Forbidden),
        Code::Unavailable
        | Code::Unknown
        | Code::Internal
        | Code::Aborted
        | Code::DeadlineExceeded => Attempt::Retry(status.message().to_string()),
        _ => Attempt::Stop(FetchError::Failed(status.message().to_string())),
    }
}

/// Asks the server for the bytes; used when the bucket cannot sign links.
pub struct GrpcFetcher {
    channel: Arc<SharedChannel>,
    job_id: String,
}

impl GrpcFetcher {
    pub fn new(channel: Arc<SharedChannel>, job_id: &str) -> Self {
        GrpcFetcher {
            channel,
            job_id: job_id.to_string(),
        }
    }

    async fn once(&self, pack: u32, offset: u64, len: u64) -> Result<Vec<u8>, Attempt> {
        let length = u32::try_from(len)
            .map_err(|_| Attempt::Stop(FetchError::Failed("range is too long".into())))?;
        let mut stream = self
            .channel
            .call(|channel| {
                let request = pb::ReadPackRangeRequest {
                    job_id: self.job_id.clone(),
                    pack,
                    offset,
                    length,
                };
                async move { AgentClient::new(channel).read_pack_range(request).await }
            })
            .await
            .map_err(|s| status_error(&s))?
            .into_inner();
        let mut out = Vec::with_capacity(len as usize);
        while let Some(part) = stream.message().await.map_err(|s| status_error(&s))? {
            out.extend_from_slice(&part.data);
            if out.len() as u64 > len {
                break;
            }
        }
        if out.len() as u64 != len {
            return Err(Attempt::Retry(format!(
                "the server returned {} bytes, expected {len}",
                out.len()
            )));
        }
        Ok(out)
    }
}

impl PackFetcher for GrpcFetcher {
    async fn fetch(
        &self,
        access: &PackAccess,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FetchError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        retrying(PAUSE, || self.once(access.pack, offset, len)).await
    }

    async fn refresh(&self, packs: &[u32]) -> Result<Vec<PackAccess>, FetchError> {
        let response = self
            .channel
            .call(|channel| {
                let request = pb::RefreshPackAccessRequest {
                    job_id: self.job_id.clone(),
                    packs: packs.to_vec(),
                };
                async move {
                    AgentClient::new(channel)
                        .max_decoding_message_size(super::restore::MAX_PLAN_MESSAGE)
                        .refresh_pack_access(request)
                        .await
                }
            })
            .await
            .map_err(|s| match status_error(&s) {
                Attempt::Stop(e) => e,
                Attempt::Retry(why) => FetchError::Failed(why),
            })?
            .into_inner();
        response
            .packs
            .into_iter()
            .map(|p| PackAccess::from_proto(p).map_err(FetchError::Failed))
            .collect()
    }
}

/// What a job uses: links when the server issued one, the server otherwise.
pub struct Fetchers {
    http: HttpFetcher,
    grpc: GrpcFetcher,
}

impl Fetchers {
    pub fn new(
        channel: Arc<SharedChannel>,
        job_id: &str,
        net: &NetOptions,
    ) -> Result<Self, FetchError> {
        Ok(Fetchers {
            http: HttpFetcher::new(net)?,
            grpc: GrpcFetcher::new(channel, job_id),
        })
    }
}

impl PackFetcher for Fetchers {
    async fn fetch(
        &self,
        access: &PackAccess,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FetchError> {
        if access.url.is_some() {
            self.http.fetch(access, offset, len).await
        } else {
            self.grpc.fetch(access, offset, len).await
        }
    }

    async fn refresh(&self, packs: &[u32]) -> Result<Vec<PackAccess>, FetchError> {
        self.grpc.refresh(packs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(pack: u32, offset: u32, enc_len: u32) -> pb::ChunkLoc {
        pb::ChunkLoc {
            pack,
            offset,
            enc_len,
            ulen: enc_len,
            id: vec![0; 32],
        }
    }

    #[test]
    fn fetch_coalesces_within_8mib() {
        let spans = coalesce(&[loc(1, 0, 100), loc(1, 100, 200), loc(1, 400, 50)]);
        assert_eq!(
            spans,
            vec![Span {
                pack: 1,
                start: 0,
                len: 450,
                chunks: 0..3
            }]
        );

        let spans = coalesce(&[loc(1, 0, 100), loc(2, 100, 100), loc(2, 200, 100)]);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].chunks, 1..3);

        let big = 5 << 20;
        let spans = coalesce(&[loc(1, 0, big), loc(1, big, big), loc(1, 2 * big, 100)]);
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(|s| s.len <= MAX_RANGE));
        assert_eq!(spans[0].chunks, 0..1);

        let far = coalesce(&[loc(1, 0, 100), loc(1, 1 << 20, 100)]);
        assert_eq!(far.len(), 2);

        let back = coalesce(&[loc(1, 5000, 100), loc(1, 0, 100)]);
        assert_eq!(back.len(), 2);
    }

    #[test]
    fn fetch_debug_hides_url_and_key() {
        let access = PackAccess::from_proto(pb::PackAccess {
            pack: 3,
            pack_id: vec![1; 32],
            key: vec![0xAB; 32],
            url: "https://bucket.example/packs/secret?X-Amz-Signature=deadbeef".into(),
            expires_at_ms: 42,
        })
        .unwrap();
        let text = format!("{access:?}");
        assert!(!text.contains("bucket.example"), "{text}");
        assert!(!text.contains("deadbeef"), "{text}");
        assert!(!text.contains("171"), "{text}");
        assert!(text.contains("pack: 3"));
    }

    #[test]
    fn fetch_rejects_short_keys_and_flags_expiry() {
        let short = pb::PackAccess {
            key: vec![1; 31],
            ..Default::default()
        };
        assert!(PackAccess::from_proto(short).is_err());

        let access = PackAccess::from_proto(pb::PackAccess {
            key: vec![1; 32],
            url: "http://x".into(),
            expires_at_ms: 100_000,
            ..Default::default()
        })
        .unwrap();
        assert!(access.expiring(50_000));
        assert!(!access.expiring(10_000));
    }
}
