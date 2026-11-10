use std::time::Duration;

use lodestore_server::config::{NotificationsConfig, Secret};
use lodestore_server::notify::net;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

async fn listener() -> (String, mpsc::UnboundedReceiver<String>) {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = socket.accept().await else {
                return;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let _ = tx.send(head.lines().next().unwrap_or_default().to_string());
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    (format!("127.0.0.1:{}", addr.port()), rx)
}

async fn quiet(rx: &mut mpsc::UnboundedReceiver<String>) -> bool {
    tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .is_err()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_ignores_environment_proxy() {
    let (target, mut target_rx) = listener().await;
    let (proxy, mut proxy_rx) = listener().await;
    let proxy_url = format!("http://{proxy}");
    for name in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        // SAFETY: nextest runs each test in its own process and nothing reads the environment yet.
        unsafe { std::env::set_var(name, &proxy_url) };
    }
    let config = NotificationsConfig {
        allow_private_targets: true,
        ..Default::default()
    };
    let response = net::post_json(&config, &format!("http://{target}/x"), b"{}".to_vec(), None)
        .await
        .unwrap();
    assert!(response.status().is_success());
    assert_eq!(target_rx.recv().await.unwrap(), "POST /x HTTP/1.1");
    assert!(quiet(&mut proxy_rx).await, "the environment proxy was used");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_uses_configured_proxy() {
    let (proxy, mut proxy_rx) = listener().await;
    let config = NotificationsConfig {
        proxy: Some(Secret::new(format!("http://{proxy}"))),
        ..Default::default()
    };
    let response = net::post_json(
        &config,
        "http://hooks.example.invalid/x",
        b"{}".to_vec(),
        None,
    )
    .await
    .unwrap();
    assert!(response.status().is_success());
    assert_eq!(
        proxy_rx.recv().await.unwrap(),
        "POST http://hooks.example.invalid/x HTTP/1.1"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_proxy_still_rejects_blocked_literals() {
    let (proxy, mut proxy_rx) = listener().await;
    let config = NotificationsConfig {
        proxy: Some(Secret::new(format!("http://{proxy}"))),
        ..Default::default()
    };
    let error = net::post_json(&config, "http://169.254.169.254/x", b"{}".to_vec(), None)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "The address 169.254.169.254 is not allowed."
    );
    assert!(quiet(&mut proxy_rx).await, "the proxy got a connection");
}
