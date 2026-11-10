mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::{Backend, TestServer};
use lodestore_server::serve::HEADER_READ_TIMEOUT;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn rest_addr(server: &TestServer) -> SocketAddr {
    server.config.listen
}

async fn assert_closed_within_timeout(mut stream: TcpStream) {
    let started = Instant::now();
    let mut buf = [0u8; 64];
    let read = tokio::time::timeout(
        HEADER_READ_TIMEOUT + Duration::from_secs(3),
        stream.read(&mut buf),
    )
    .await
    .expect("the server did not close the connection in time");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "unexpected data from the server: {read:?}"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "closed too early: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn rest_closes_silent_connection() {
    let server = TestServer::start(&Backend::Sqlite).await;
    let stream = TcpStream::connect(rest_addr(&server)).await.unwrap();
    assert_closed_within_timeout(stream).await;
    server.stop().await;
}

#[tokio::test]
async fn rest_closes_connection_with_unfinished_headers() {
    let server = TestServer::start(&Backend::Sqlite).await;
    let mut stream = TcpStream::connect(rest_addr(&server)).await.unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    assert_closed_within_timeout(stream).await;
    server.stop().await;
}

#[tokio::test]
async fn rest_serves_normal_requests() {
    let server = TestServer::start(&Backend::Sqlite).await;
    let response = reqwest::get(format!("{}/healthz", server.rest_url()))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    server.api().await;
    server.stop().await;
}

#[tokio::test]
async fn grpc_closes_stalled_tls_handshake() {
    let server = TestServer::start(&Backend::Sqlite).await;
    let stream = TcpStream::connect(server.config.grpc.listen).await.unwrap();
    assert_closed_within_timeout(stream).await;
    server.stop().await;
}
