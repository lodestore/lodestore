use std::convert::Infallible;
use std::time::Duration;

use axum::extract::ConnectInfo;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower::Service;

pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Serves `router` over HTTP/1.1 and closes any connection that does not send a
/// complete request head within `HEADER_READ_TIMEOUT`. The timer covers a silent
/// connection and the pause between keep-alive requests; bodies and responses are
/// not timed, so long downloads and ndjson streams are unaffected.
pub(crate) async fn serve_http(
    listener: TcpListener,
    router: axum::Router,
    stop: CancellationToken,
) -> anyhow::Result<()> {
    let graceful = GracefulShutdown::new();
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(%error, "accepting an HTTP connection failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            () = stop.cancelled() => break,
        };
        let router = router.clone();
        let service = service_fn(move |mut request: hyper::Request<Incoming>| {
            request.extensions_mut().insert(ConnectInfo(peer));
            let mut router = router.clone();
            async move {
                match router.call(request).await {
                    Ok(response) => Ok::<_, Infallible>(response),
                    Err(never) => match never {},
                }
            }
        });
        let connection = http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .serve_connection(TokioIo::new(stream), service);
        let connection = graceful.watch(connection);
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!(%error, %peer, "HTTP connection ended with an error");
            }
        });
    }
    graceful.shutdown().await;
    Ok(())
}
