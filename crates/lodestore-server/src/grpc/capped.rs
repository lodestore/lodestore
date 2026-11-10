use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::server::{Connected, TcpConnectInfo};

const WARN_EVERY: Duration = Duration::from_secs(60);

/// A TCP connection that holds one slot of the connection cap until it is dropped.
pub struct Capped {
    inner: TcpStream,
    _permit: OwnedSemaphorePermit,
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

/// Accepts connections while a slot is free; a connection that finds none is
/// closed at once. The accept task ends when the returned stream is dropped.
pub fn incoming(
    listener: TcpListener,
    slots: Arc<Semaphore>,
) -> ReceiverStream<io::Result<Capped>> {
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let mut last_warn: Option<Instant> = None;
        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                () = tx.closed() => return,
            };
            let stream = match accepted {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::warn!(%error, "accepting a gRPC connection failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                if last_warn.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
                    last_warn = Some(Instant::now());
                    tracing::warn!("gRPC connection cap reached; closing new connections");
                }
                continue;
            };
            let capped = Capped {
                inner: stream,
                _permit: permit,
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
        let mut incoming = incoming(listener, Arc::new(Semaphore::new(1)));

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
    async fn capped_keeps_peer_address() {
        let (listener, addr) = pair().await;
        let mut incoming = incoming(listener, Arc::new(Semaphore::new(1)));
        let client = TcpStream::connect(addr).await.unwrap();
        let capped = incoming.next().await.unwrap().unwrap();
        assert_eq!(
            capped.connect_info().remote_addr(),
            Some(client.local_addr().unwrap())
        );
    }
}
