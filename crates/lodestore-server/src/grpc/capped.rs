use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::server::{Connected, TcpConnectInfo};

use crate::auth::limits::bucket;

const WARN_EVERY: Duration = Duration::from_secs(60);

/// Counts open connections per client address (IPv6 by its /56), so one
/// address cannot hold every slot of the global cap.
pub struct PerAddr {
    cap: usize,
    held: Mutex<HashMap<IpAddr, usize>>,
}

impl PerAddr {
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(PerAddr {
            cap,
            held: Mutex::default(),
        })
    }

    fn try_hold(self: &Arc<Self>, ip: IpAddr) -> Option<AddrHold> {
        let key = bucket(ip);
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        let count = held.entry(key).or_insert(0);
        if *count >= self.cap {
            return None;
        }
        *count += 1;
        Some(AddrHold {
            owner: self.clone(),
            key,
        })
    }
}

struct AddrHold {
    owner: Arc<PerAddr>,
    key: IpAddr,
}

impl Drop for AddrHold {
    fn drop(&mut self) {
        let mut held = self.owner.held.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(count) = held.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                held.remove(&self.key);
            }
        }
    }
}

/// A TCP connection that holds one slot of the connection cap, and one of its
/// address's share of it, until it is dropped.
pub struct Capped {
    inner: TcpStream,
    _permit: OwnedSemaphorePermit,
    _addr: AddrHold,
}

impl AsyncRead for Capped {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Capped {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

impl Connected for Capped {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        self.inner.connect_info()
    }
}

/// Accepts connections while a slot is free and the client address is under
/// its share; a connection that finds none is closed at once. The accept task
/// ends when the returned stream is dropped.
pub fn incoming(
    listener: TcpListener,
    slots: Arc<Semaphore>,
    per_addr: Arc<PerAddr>,
) -> ReceiverStream<io::Result<Capped>> {
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let mut last_warn: Option<Instant> = None;
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = tx.closed() => return,
            };
            let (stream, peer) = match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%error, "accepting a gRPC connection failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let held = per_addr.try_hold(peer.ip());
            let permit = held
                .as_ref()
                .and_then(|_| slots.clone().try_acquire_owned().ok());
            let (Some(addr), Some(permit)) = (held, permit) else {
                if last_warn.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
                    last_warn = Some(Instant::now());
                    tracing::warn!(%peer, "gRPC connection cap reached; closing new connections");
                }
                continue;
            };
            let capped = Capped {
                inner: stream,
                _permit: permit,
                _addr: addr,
            };
            if tx.send(Ok(capped)).await.is_err() {
                return;
            }
        }
    });
    ReceiverStream::new(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio_stream::StreamExt;

    async fn pair() -> (TcpListener, std::net::SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    #[tokio::test]
    async fn capped_closes_connections_over_the_cap() {
        let (listener, addr) = pair().await;
        let mut incoming = incoming(listener, Arc::new(Semaphore::new(1)), PerAddr::new(8));

        let _first_client = TcpStream::connect(addr).await.unwrap();
        let first = incoming.next().await.unwrap().unwrap();

        let mut second_client = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(1), second_client.read(&mut buf))
            .await
            .expect("the over-cap connection was not closed");
        assert!(matches!(read, Ok(0) | Err(_)));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), incoming.next())
                .await
                .is_err(),
            "the over-cap connection was handed out"
        );

        drop(first);
        let _third_client = TcpStream::connect(addr).await.unwrap();
        let third = tokio::time::timeout(Duration::from_secs(1), incoming.next())
            .await
            .expect("no connection after a slot was freed");
        assert!(third.unwrap().is_ok());
    }

    #[tokio::test]
    async fn one_address_cannot_take_every_slot() {
        let (listener, addr) = pair().await;
        let mut incoming = incoming(listener, Arc::new(Semaphore::new(8)), PerAddr::new(2));

        let mut clients = Vec::new();
        let mut held = Vec::new();
        for _ in 0..2 {
            clients.push(TcpStream::connect(addr).await.unwrap());
            held.push(incoming.next().await.unwrap().unwrap());
        }

        let mut third_client = TcpStream::connect(addr).await.unwrap();
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(1), third_client.read(&mut buf))
            .await
            .expect("the connection over the address share was not closed");
        assert!(matches!(read, Ok(0) | Err(_)));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), incoming.next())
                .await
                .is_err(),
            "a connection over the address share was handed out"
        );

        held.pop();
        let _fourth_client = TcpStream::connect(addr).await.unwrap();
        let fourth = tokio::time::timeout(Duration::from_secs(1), incoming.next())
            .await
            .expect("no connection after the address freed a slot");
        assert!(fourth.unwrap().is_ok());
    }

    #[test]
    fn addresses_share_by_prefix_not_by_host() {
        let per_addr = PerAddr::new(1);
        let a: IpAddr = "2001:db8:0:ff::1".parse().unwrap();
        let same_block: IpAddr = "2001:db8:0:a::2".parse().unwrap();
        let other: IpAddr = "2001:db8:0:100::1".parse().unwrap();
        let first = per_addr.try_hold(a).unwrap();
        assert!(per_addr.try_hold(same_block).is_none());
        assert!(per_addr.try_hold(other).is_some());
        drop(first);
        assert!(per_addr.try_hold(same_block).is_some());
    }

    #[tokio::test]
    async fn capped_keeps_peer_address() {
        let (listener, addr) = pair().await;
        let mut incoming = incoming(listener, Arc::new(Semaphore::new(1)), PerAddr::new(8));
        let client = TcpStream::connect(addr).await.unwrap();
        let capped = incoming.next().await.unwrap().unwrap();
        assert_eq!(
            capped.connect_info().remote_addr(),
            Some(client.local_addr().unwrap())
        );
    }
}
