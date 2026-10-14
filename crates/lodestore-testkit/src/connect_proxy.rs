//! A minimal HTTP CONNECT proxy for tests. It tunnels bytes without looking
//! at them, so TLS inside the tunnel stays end to end, and counts the tunnels
//! it opened.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

pub struct ConnectProxy {
    pub addr: SocketAddr,
    pub connects: Arc<AtomicU64>,
}

impl ConnectProxy {
    pub fn start() -> ConnectProxy {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding the proxy");
        let addr = listener.local_addr().expect("proxy address");
        let connects = Arc::new(AtomicU64::new(0));
        let counter = connects.clone();
        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                let counter = counter.clone();
                thread::spawn(move || {
                    let _ = handle(client, &counter);
                });
            }
        });
        ConnectProxy { addr, connects }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn connects(&self) -> u64 {
        self.connects.load(Ordering::SeqCst)
    }
}

fn read_head(client: &mut TcpStream) -> std::io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 || client.read(&mut byte)? == 0 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        head.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

fn handle(mut client: TcpStream, connects: &AtomicU64) -> std::io::Result<()> {
    let head = read_head(&mut client)?;
    let mut line = head.lines().next().unwrap_or_default().split_whitespace();
    let (Some("CONNECT"), Some(target)) = (line.next(), line.next()) else {
        return client.write_all(b"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n");
    };
    let upstream = TcpStream::connect(target)?;
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
    connects.fetch_add(1, Ordering::SeqCst);

    let (mut client_read, mut upstream_write) = (client.try_clone()?, upstream.try_clone()?);
    let up = thread::spawn(move || {
        let _ = std::io::copy(&mut client_read, &mut upstream_write);
        let _ = upstream_write.shutdown(Shutdown::Write);
    });
    let (mut upstream_read, mut client_write) = (upstream, client);
    let _ = std::io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}
