use lodestore_nbd::wire::{EINVAL, EIO, STATE_DIRTY, STATE_HOLE, STATE_ZERO};
use lodestore_nbd::{Client, Descriptor, Error, Limits, MemExport, NbdUri, serve};
use tokio::io::{DuplexStream, duplex};

const K: u64 = 1024;
const M: u64 = 1 << 20;

fn d(len: u64, flags: u32) -> Descriptor {
    Descriptor {
        len: len as u32,
        flags,
    }
}

fn pattern(len: u64) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + (i >> 12)) % 251 + 1) as u8)
        .collect()
}

async fn with_client<T>(
    export: &mut MemExport,
    bitmaps: &[&str],
    f: impl AsyncFnOnce(&mut Client<DuplexStream>) -> T,
) -> T {
    let (c, s) = duplex(64 << 10);
    let limits = Limits::default();
    let server = serve(s, export, &limits);
    let client = async {
        let mut client = Client::handshake(c, "", bitmaps, limits).await.unwrap();
        let out = f(&mut client).await;
        client.disconnect().await.unwrap();
        out
    };
    let (served, out) = tokio::join!(server, client);
    served.unwrap();
    out
}

fn remote_errno<T: std::fmt::Debug>(r: Result<T, Error>) -> u32 {
    match r {
        Err(Error::Remote { errno, .. }) => errno,
        other => panic!("expected a remote error, got {other:?}"),
    }
}

#[tokio::test]
async fn nbd_reads_and_status_round_trip() {
    let mut data = pattern(8 * M);
    data[(2 * M) as usize..(3 * M) as usize].fill(0);
    let mut export = MemExport::new(data.clone()).with_bitmap(
        "b0",
        vec![3 * M..3 * M + 4096, 64 * K..192 * K, M..M + 64 * K],
    );
    with_client(&mut export, &["b0"], async |c| {
        assert_eq!(c.size(), 8 * M);
        assert!(c.read_only());
        assert!(c.has_dirty());
        let got = c.read(M, 64 * K as u32).await.unwrap();
        assert_eq!(got, data[M as usize..(M + 64 * K) as usize]);
        let status = c.block_status(0, 4 * M as u32).await.unwrap();
        assert_eq!(
            status.allocation,
            vec![d(2 * M, 0), d(M, STATE_HOLE | STATE_ZERO), d(M, 0)]
        );
        assert_eq!(
            status.dirty.unwrap(),
            vec![
                d(64 * K, 0),
                d(128 * K, STATE_DIRTY),
                d(M - 192 * K, 0),
                d(64 * K, STATE_DIRTY),
                d(2 * M - 64 * K, 0),
                d(4096, STATE_DIRTY),
                d(M - 4096, 0),
            ]
        );
    })
    .await;
    assert_eq!(export.bytes_read(), 64 * K);
}

#[tokio::test]
async fn nbd_bitmap_context_only_when_offered() {
    let mut export = MemExport::new(pattern(M)).with_bitmap("b0", vec![0..4096]);
    with_client(&mut export, &["other"], async |c| {
        assert!(!c.has_dirty());
        assert!(!c.has_context("b0"));
        let status = c.block_status(0, 4096).await.unwrap();
        assert_eq!(status.dirty, None);
        assert_eq!(status.allocation, vec![d(4096, 0)]);
    })
    .await;
}

#[tokio::test]
async fn nbd_serves_two_bitmap_contexts() {
    let mut export = MemExport::new(pattern(M))
        .with_bitmap("g1", vec![0..4096])
        .with_bitmap("g2", vec![8192..12288]);
    with_client(&mut export, &["g1", "g2"], async |c| {
        assert!(c.has_dirty());
        assert!(c.has_context("g2"));
        let status = c.block_status(0, 16384).await.unwrap();
        assert_eq!(
            status.dirty.unwrap(),
            vec![d(4096, STATE_DIRTY), d(12288, 0)]
        );
    })
    .await;
    with_client(&mut export, &["g1", "g3"], async |c| {
        assert!(c.has_dirty());
        assert!(c.has_context("g1"));
        assert!(!c.has_context("g3"));
        c.block_status(0, 4096).await.unwrap();
    })
    .await;
    let (client_end, _server_end) = duplex(1024);
    let err = Client::handshake(client_end, "", &["a", "b", "c"], Limits::default()).await;
    assert!(matches!(err, Err(Error::Protocol(_))));
}

#[tokio::test]
async fn nbd_dirty_only_refuses_reads_outside_dirty() {
    let mut export = MemExport::new(pattern(M)).with_bitmap("b0", vec![64 * K..128 * K]);
    let switch = export.dirty_only_switch();
    with_client(&mut export, &["b0"], async |c| {
        assert_eq!(c.read(0, 4096).await.unwrap().len(), 4096);
        switch.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(c.read(64 * K, 4096).await.unwrap().len(), 4096);
        assert_eq!(remote_errno(c.read(0, 4096).await), EIO);
        assert_eq!(c.read(64 * K, 4096).await.unwrap().len(), 4096);
    })
    .await;
    assert_eq!(export.reads_outside_dirty(), 2);
}

#[tokio::test]
async fn nbd_min_block_refuses_unaligned_requests() {
    let mut export = MemExport::new(pattern(M)).min_block(4096);
    with_client(&mut export, &[], async |c| {
        assert_eq!(c.block_size().min, 4096);
        assert_eq!(c.read(4096, 4096).await.unwrap().len(), 4096);
        assert_eq!(remote_errno(c.read(1000, 4096).await), EINVAL);
        assert_eq!(remote_errno(c.block_status(100, 4096).await), EINVAL);
        assert_eq!(c.read(8192, 4096).await.unwrap().len(), 4096);
    })
    .await;
}

#[tokio::test]
async fn nbd_fail_reads_after() {
    let mut export = MemExport::new(pattern(M)).fail_reads_after(2);
    with_client(&mut export, &[], async |c| {
        c.read(0, 4096).await.unwrap();
        c.read(4096, 4096).await.unwrap();
        assert_eq!(remote_errno(c.read(8192, 4096).await), EIO);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn nbd_over_a_unix_socket() {
    let dir = tempfile::Builder::new()
        .prefix("nbd")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("s");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let data = pattern(M);
    let mut export = MemExport::new(data.clone())
        .with_name("disk0")
        .with_bitmap("b0", vec![0..4096]);
    let limits = Limits::default();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve(stream, &mut export, &limits).await
    });
    let uri = NbdUri {
        socket,
        export: "disk0".into(),
    };
    let mut client = Client::connect(&uri, &["b0"], limits).await.unwrap();
    assert!(client.has_dirty());
    assert_eq!(client.read(4096, 4096).await.unwrap(), data[4096..8192]);
    client.disconnect().await.unwrap();
    server.await.unwrap().unwrap();
}
