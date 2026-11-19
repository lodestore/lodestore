use std::fmt::Debug;
use std::time::Duration;

use lodestore_nbd::wire::*;
use lodestore_nbd::{Client, Error, Limits, MemExport, serve};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

const D: u16 = REPLY_FLAG_DONE;
const M: u64 = 1 << 20;

fn limits() -> Limits {
    Limits {
        op_timeout: Duration::from_secs(5),
        ..Limits::default()
    }
}

fn pattern(len: u64) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + (i >> 12)) % 251 + 1) as u8)
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Protocol,
    Refused,
    Closed,
    Remote,
    Timeout,
}

fn expect<T: Debug>(name: &str, want: Kind, got: Result<T, Error>) {
    let ok = matches!(
        (want, &got),
        (Kind::Protocol, Err(Error::Protocol(_)))
            | (Kind::Refused, Err(Error::Refused { .. }))
            | (Kind::Closed, Err(Error::Closed))
            | (Kind::Remote, Err(Error::Remote { .. }))
            | (Kind::Timeout, Err(Error::Timeout))
    );
    assert!(ok, "{name}: wanted {want:?}, got {got:?}");
}

mod script {
    use super::*;

    pub fn greeting_with(flags: u16) -> Vec<u8> {
        let mut v = NBDMAGIC.to_be_bytes().to_vec();
        v.extend_from_slice(&IHAVEOPT.to_be_bytes());
        v.extend_from_slice(&flags.to_be_bytes());
        v
    }

    pub fn greeting() -> Vec<u8> {
        greeting_with(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES)
    }

    pub fn ack(option: u32) -> Vec<u8> {
        option_reply(option, REP_ACK, &[])
    }

    pub fn context(id: u32, name: &str) -> Vec<u8> {
        let mut p = id.to_be_bytes().to_vec();
        p.extend_from_slice(name.as_bytes());
        option_reply(OPT_SET_META_CONTEXT, REP_META_CONTEXT, &p)
    }

    pub fn info(payload: &[u8]) -> Vec<u8> {
        option_reply(OPT_GO, REP_INFO, payload)
    }

    pub fn info_export(size: u64) -> Vec<u8> {
        let mut p = INFO_EXPORT.to_be_bytes().to_vec();
        p.extend_from_slice(&size.to_be_bytes());
        p.extend_from_slice(&(FLAG_HAS_FLAGS | FLAG_READ_ONLY).to_be_bytes());
        info(&p)
    }

    pub fn info_block(min: u32, preferred: u32, max: u32) -> Vec<u8> {
        let mut p = INFO_BLOCK_SIZE.to_be_bytes().to_vec();
        for v in [min, preferred, max] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        info(&p)
    }

    pub fn through_contexts(bitmaps: &[(u32, &str)]) -> Vec<u8> {
        let mut v = greeting();
        v.extend(ack(OPT_STRUCTURED_REPLY));
        v.extend(context(1, CONTEXT_ALLOCATION));
        for (id, name) in bitmaps {
            v.extend(context(*id, &format!("{BITMAP_CONTEXT_PREFIX}{name}")));
        }
        v.extend(ack(OPT_SET_META_CONTEXT));
        v
    }

    pub fn handshake(
        size: u64,
        block: Option<(u32, u32, u32)>,
        bitmaps: &[(u32, &str)],
    ) -> Vec<u8> {
        let mut v = through_contexts(bitmaps);
        v.extend(info_export(size));
        if let Some((min, preferred, max)) = block {
            v.extend(info_block(min, preferred, max));
        }
        v.extend(ack(OPT_GO));
        v
    }

    pub fn header_only(flags: u16, ty: u16, cookie: u64, len: u32) -> Vec<u8> {
        let mut v = STRUCTURED_REPLY_MAGIC.to_be_bytes().to_vec();
        v.extend_from_slice(&flags.to_be_bytes());
        v.extend_from_slice(&ty.to_be_bytes());
        v.extend_from_slice(&cookie.to_be_bytes());
        v.extend_from_slice(&len.to_be_bytes());
        v
    }

    pub fn data(flags: u16, cookie: u64, offset: u64, bytes: &[u8]) -> Vec<u8> {
        let mut p = offset.to_be_bytes().to_vec();
        p.extend_from_slice(bytes);
        structured_chunk(flags, REPLY_OFFSET_DATA, cookie, &p)
    }

    pub fn hole(flags: u16, cookie: u64, offset: u64, len: u32) -> Vec<u8> {
        let mut p = offset.to_be_bytes().to_vec();
        p.extend_from_slice(&len.to_be_bytes());
        structured_chunk(flags, REPLY_OFFSET_HOLE, cookie, &p)
    }

    pub fn error(flags: u16, ty: u16, cookie: u64, errno: u32, message: &[u8]) -> Vec<u8> {
        let mut p = errno.to_be_bytes().to_vec();
        p.extend_from_slice(&(message.len() as u16).to_be_bytes());
        p.extend_from_slice(message);
        structured_chunk(flags, ty, cookie, &p)
    }

    pub fn status(flags: u16, cookie: u64, id: u32, descriptors: &[(u32, u32)]) -> Vec<u8> {
        let mut p = id.to_be_bytes().to_vec();
        for (len, f) in descriptors {
            p.extend_from_slice(&len.to_be_bytes());
            p.extend_from_slice(&f.to_be_bytes());
        }
        structured_chunk(flags, REPLY_BLOCK_STATUS, cookie, &p)
    }
}

async fn scripted(bytes: &[u8]) -> (DuplexStream, DuplexStream) {
    let (client, mut server) = duplex(2 << 20);
    server.write_all(bytes).await.unwrap();
    (client, server)
}

async fn connect(
    bytes: &[u8],
    bitmaps: &[&str],
    limits: Limits,
) -> (Result<Client<DuplexStream>, Error>, DuplexStream) {
    let (client, server) = scripted(bytes).await;
    (Client::handshake(client, "", bitmaps, limits).await, server)
}

struct Handshake {
    name: &'static str,
    bytes: Vec<u8>,
    bitmaps: Vec<&'static str>,
    limits: Limits,
    close: bool,
    want: Kind,
    message: Vec<&'static str>,
}

fn row(name: &'static str, want: Kind, bytes: Vec<u8>) -> Handshake {
    Handshake {
        name,
        bytes,
        bitmaps: Vec::new(),
        limits: limits(),
        close: false,
        want,
        message: Vec::new(),
    }
}

fn handshake_rows() -> Vec<Handshake> {
    use script::*;
    let prefix = || through_contexts(&[]);
    let mut rows = Vec::new();
    let mut bad_magic = greeting();
    bad_magic[0] ^= 1;
    rows.push(row("bad NBDMAGIC", Kind::Protocol, bad_magic));
    rows.push(row("no fixed newstyle", Kind::Protocol, greeting_with(0)));
    let mut v = greeting();
    v.extend(ack(OPT_GO));
    rows.push(row("reply to a foreign option", Kind::Protocol, v));
    let mut v = greeting();
    v.extend(option_reply(OPT_STRUCTURED_REPLY, 77, &[]));
    rows.push(row("unknown reply type", Kind::Protocol, v));
    let mut v = greeting();
    v.extend_from_slice(&OPT_REPLY_MAGIC.to_be_bytes());
    v.extend_from_slice(&OPT_STRUCTURED_REPLY.to_be_bytes());
    v.extend_from_slice(&REP_ACK.to_be_bytes());
    v.extend_from_slice(&(1u32 << 30).to_be_bytes());
    rows.push(row("1 GiB option reply", Kind::Protocol, v));
    let mut v = greeting();
    v.extend(option_reply(OPT_STRUCTURED_REPLY, REP_ACK, &[1]));
    rows.push(row("acknowledgement with a payload", Kind::Protocol, v));

    let mut r = row(
        "more contexts than requested",
        Kind::Protocol,
        through_contexts(&[(2, "b0"), (3, "zz")]),
    );
    r.bitmaps = vec!["b0"];
    rows.push(r);
    rows.push(row(
        "a context that was not requested",
        Kind::Protocol,
        through_contexts(&[(2, "zz")]),
    ));
    let mut v = greeting();
    v.extend(ack(OPT_STRUCTURED_REPLY));
    v.extend(context(1, CONTEXT_ALLOCATION));
    v.extend(context(2, CONTEXT_ALLOCATION));
    rows.push(row("a repeated context", Kind::Protocol, v));
    let mut r = row(
        "two contexts with one id",
        Kind::Protocol,
        through_contexts(&[(1, "b0")]),
    );
    r.bitmaps = vec!["b0"];
    rows.push(r);
    let mut v = greeting();
    v.extend(ack(OPT_STRUCTURED_REPLY));
    v.extend(ack(OPT_SET_META_CONTEXT));
    rows.push(row("no base:allocation", Kind::Protocol, v));

    for (name, len) in [
        ("INFO_EXPORT of 11 bytes", 9),
        ("INFO_EXPORT of 13 bytes", 11),
    ] {
        let mut p = INFO_EXPORT.to_be_bytes().to_vec();
        p.extend(vec![0; len]);
        let mut v = prefix();
        v.extend(info(&p));
        rows.push(row(name, Kind::Protocol, v));
    }
    let mut r = row(
        "export larger than the limit",
        Kind::Protocol,
        handshake(2 * M, None, &[]),
    );
    r.limits.max_export_size = M;
    rows.push(r);
    let mut r = row(
        "export size not a multiple of the minimum block",
        Kind::Protocol,
        handshake(M + 512, Some((4096, 4096, 4 << 20)), &[]),
    );
    r.message = vec!["1049088", "4096"];
    rows.push(r);
    for (name, block) in [
        ("min block of 3", (3, 4096, 4 << 20)),
        ("preferred below min", (4096, 512, 4 << 20)),
        ("preferred not a power of two", (4096, 5000, 4 << 20)),
        ("max below preferred", (4096, 4096, 2048)),
        ("min block of 128 KiB", (131_072, 131_072, 4 << 20)),
    ] {
        rows.push(row(name, Kind::Protocol, handshake(M, Some(block), &[])));
    }
    let mut v = prefix();
    v.extend(option_reply(OPT_GO, REP_ERR_UNSUP, b"no"));
    rows.push(row("GO refused", Kind::Refused, v));
    let mut v = prefix();
    v.extend(ack(OPT_GO));
    rows.push(row("no export information", Kind::Protocol, v));
    let mut v = prefix();
    v.extend(info_export(M));
    v.extend(info_export(M));
    rows.push(row("export information twice", Kind::Protocol, v));
    let mut v = prefix();
    for _ in 0..20 {
        let mut p = 9u16.to_be_bytes().to_vec();
        p.push(0);
        v.extend(info(&p));
    }
    rows.push(row("endless information", Kind::Protocol, v));
    let mut r = row("closed mid handshake", Kind::Closed, greeting());
    r.close = true;
    rows.push(r);
    rows
}

#[tokio::test(start_paused = true)]
async fn client_rejects_hostile_handshakes() {
    let (control, _server) = connect(&script::handshake(M, None, &[]), &[], limits()).await;
    assert!(control.is_ok(), "the control handshake must succeed");
    for r in handshake_rows() {
        let (client, server) = scripted(&r.bytes).await;
        let mut server = server;
        if r.close {
            server.shutdown().await.unwrap();
        }
        let got = Client::handshake(client, "", &r.bitmaps, r.limits)
            .await
            .map(|_| ());
        for needle in &r.message {
            let text = got
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_default();
            assert!(text.contains(needle), "{}: {text:?} lacks {needle}", r.name);
        }
        expect(r.name, r.want, got);
        drop(server);
    }
}

const REQ_OFF: u64 = 4096;
const REQ_LEN: usize = 4096;

struct Reply {
    name: &'static str,
    bytes: Vec<u8>,
    limits: Limits,
    close: bool,
    two_reads: bool,
    want: Kind,
}

fn reply(name: &'static str, want: Kind, bytes: Vec<u8>) -> Reply {
    Reply {
        name,
        bytes,
        limits: limits(),
        close: false,
        two_reads: false,
        want,
    }
}

fn read_rows() -> Vec<Reply> {
    use script::*;
    let full = vec![7u8; REQ_LEN];
    let cat = |parts: Vec<Vec<u8>>| parts.concat();
    let mut rows = vec![
        reply("wrong cookie", Kind::Protocol, data(D, 2, REQ_OFF, &full)),
        reply("bad reply magic", Kind::Protocol, vec![0; 20]),
        reply(
            "data outside the request",
            Kind::Protocol,
            data(D, 1, 0, &full),
        ),
        reply(
            "data past the end of the request",
            Kind::Protocol,
            data(D, 1, REQ_OFF + 1, &full),
        ),
        reply(
            "overlapping data",
            Kind::Protocol,
            cat(vec![
                data(0, 1, REQ_OFF, &full[..2048]),
                data(D, 1, REQ_OFF + 1024, &full[..3072]),
            ]),
        ),
        reply(
            "the whole window sent again and again without done",
            Kind::Protocol,
            cat((0..400).map(|_| data(0, 1, REQ_OFF, &full)).collect()),
        ),
        reply(
            "data chunk of 8 bytes",
            Kind::Protocol,
            structured_chunk(D, REPLY_OFFSET_DATA, 1, &[0; 8]),
        ),
        reply(
            "data chunk of 5 bytes",
            Kind::Protocol,
            structured_chunk(D, REPLY_OFFSET_DATA, 1, &[0; 5]),
        ),
        reply(
            "hole of 13 bytes",
            Kind::Protocol,
            structured_chunk(D, REPLY_OFFSET_HOLE, 1, &[0; 13]),
        ),
        reply("empty hole", Kind::Protocol, hole(D, 1, REQ_OFF, 0)),
        reply(
            "hole outside the request",
            Kind::Protocol,
            hole(D, 1, REQ_OFF, REQ_LEN as u32 + 1),
        ),
        reply(
            "DONE without full coverage",
            Kind::Protocol,
            data(D, 1, REQ_OFF, &full[..2048]),
        ),
        reply(
            "NONE without DONE",
            Kind::Protocol,
            structured_chunk(0, REPLY_NONE, 1, &[]),
        ),
        reply(
            "NONE with a payload",
            Kind::Protocol,
            structured_chunk(D, REPLY_NONE, 1, &[0]),
        ),
        reply(
            "unknown type without the error bit",
            Kind::Protocol,
            structured_chunk(D, 77, 1, &[]),
        ),
        reply(
            "ERROR of 1 GiB",
            Kind::Protocol,
            header_only(D, REPLY_ERROR, 1, 1 << 30),
        ),
        reply(
            "ERROR_OFFSET of 1 GiB",
            Kind::Protocol,
            header_only(D, REPLY_ERROR_OFFSET, 1, 1 << 30),
        ),
        reply(
            "unknown error type of 1 GiB",
            Kind::Protocol,
            header_only(D, 0x8009, 1, 1 << 30),
        ),
        reply(
            "ERROR of 4 bytes",
            Kind::Protocol,
            structured_chunk(D, REPLY_ERROR, 1, &[0; 4]),
        ),
        reply(
            "ERROR_OFFSET of 10 bytes",
            Kind::Protocol,
            structured_chunk(D, REPLY_ERROR_OFFSET, 1, &[0; 10]),
        ),
        reply("error message longer than its chunk", Kind::Protocol, {
            let mut p = 5u32.to_be_bytes().to_vec();
            p.extend_from_slice(&100u16.to_be_bytes());
            p.extend_from_slice(b"abc");
            structured_chunk(D, REPLY_ERROR, 1, &p)
        }),
        reply("simple reply that reports success", Kind::Protocol, {
            let mut v = SIMPLE_REPLY_MAGIC.to_be_bytes().to_vec();
            v.extend_from_slice(&0u32.to_be_bytes());
            v.extend_from_slice(&1u64.to_be_bytes());
            v
        }),
    ];
    let mut r = reply(
        "data after DONE",
        Kind::Protocol,
        cat(vec![
            data(D, 1, REQ_OFF, &full),
            data(0, 1, REQ_OFF, &full[..16]),
        ]),
    );
    r.two_reads = true;
    rows.push(r);
    let mut r = reply(
        "more pieces than allowed",
        Kind::Protocol,
        cat(vec![
            data(0, 1, REQ_OFF, &full[..1024]),
            data(0, 1, REQ_OFF + 1024, &full[..1024]),
            data(0, 1, REQ_OFF + 2048, &full[..1024]),
        ]),
    );
    r.limits.max_extents = 2;
    rows.push(r);
    let mut r = reply(
        "EOF in the middle of data",
        Kind::Closed,
        cat(vec![
            header_only(D, REPLY_OFFSET_DATA, 1, REQ_LEN as u32 + 8),
            REQ_OFF.to_be_bytes().to_vec(),
            full[..100].to_vec(),
        ]),
    );
    r.close = true;
    rows.push(r);
    rows.push(reply(
        "remote error is a remote error",
        Kind::Remote,
        error(D, REPLY_ERROR, 1, EIO, b"disk on fire"),
    ));
    rows
}

#[tokio::test(start_paused = true)]
async fn client_rejects_hostile_read_replies() {
    let hs = script::handshake(M, None, &[]);
    let full = vec![7u8; REQ_LEN];

    let ok = [
        script::data(0, 1, REQ_OFF + 2048, &full[..2048]),
        script::data(0, 1, REQ_OFF, &full[..1024]),
        script::hole(0, 1, REQ_OFF + 1024, 1024),
        structured_chunk(D, REPLY_NONE, 1, &[]),
    ]
    .concat();
    let (client, _server) = scripted(&[hs.clone(), ok].concat()).await;
    let mut client = Client::handshake(client, "", &[], limits()).await.unwrap();
    let got = client.read(REQ_OFF, REQ_LEN as u32).await.unwrap();
    let mut want = full.clone();
    want[1024..2048].fill(0);
    assert_eq!(got, want, "the control read must succeed");

    for r in read_rows() {
        let (client, server) = scripted(&[hs.clone(), r.bytes].concat()).await;
        let mut server = server;
        if r.close {
            server.shutdown().await.unwrap();
        }
        let mut client = Client::handshake(client, "", &[], r.limits).await.unwrap();
        let mut got = client.read(REQ_OFF, REQ_LEN as u32).await.map(|_| ());
        if r.two_reads {
            assert!(got.is_ok(), "{}: the first read is complete", r.name);
            got = client.read(REQ_OFF, REQ_LEN as u32).await.map(|_| ());
        }
        expect(r.name, r.want, got);
        drop(server);
    }
}

struct Status {
    name: &'static str,
    bytes: Vec<u8>,
}

fn status_rows() -> Vec<Status> {
    use script::*;
    let alloc = [(4096, 0)];
    let s = |name, bytes| Status { name, bytes };
    vec![
        s(
            "status chunk of 10 bytes",
            structured_chunk(D, REPLY_BLOCK_STATUS, 1, &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0]),
        ),
        s(
            "status chunk of 13 bytes",
            structured_chunk(
                D,
                REPLY_BLOCK_STATUS,
                1,
                &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            ),
        ),
        s(
            "status chunk over the extent limit",
            header_only(D, REPLY_BLOCK_STATUS, 1, 4 + 8 * 5),
        ),
        s("status for an unknown context", status(D, 1, 9, &alloc)),
        s(
            "two chunks for one context",
            [status(0, 1, 1, &alloc), status(D, 1, 1, &alloc)].concat(),
        ),
        s(
            "DONE with only the allocation chunk",
            status(D, 1, 1, &alloc),
        ),
        s(
            "DONE without the second bitmap",
            [status(0, 1, 1, &alloc), status(D, 1, 2, &alloc)].concat(),
        ),
        s(
            "DONE without the dirty chunk",
            [status(0, 1, 1, &alloc), status(D, 1, 3, &alloc)].concat(),
        ),
        s("descriptor of length 0", status(D, 1, 1, &[(0, 0)])),
        s("empty descriptor list", status(D, 1, 1, &[])),
        s(
            "allocation descriptor with unknown bit",
            [
                status(0, 1, 1, &[(4096, 4)]),
                status(0, 1, 3, &alloc),
                status(D, 1, 2, &alloc),
            ]
            .concat(),
        ),
        s(
            "dirty descriptor with unknown bit",
            [
                status(0, 1, 1, &alloc),
                status(0, 1, 3, &alloc),
                status(D, 1, 2, &[(4096, 2)]),
            ]
            .concat(),
        ),
        s("NONE without DONE", structured_chunk(0, REPLY_NONE, 1, &[])),
        s("data in a status reply", data(D, 1, 0, &[0; 16])),
    ]
}

#[tokio::test(start_paused = true)]
async fn client_rejects_hostile_status_replies() {
    use script::*;
    let hs = handshake(M, None, &[(2, "b0"), (3, "b1")]);
    let mut small = limits();
    small.max_extents = 4;

    let ok = [
        status(0, 1, 1, &[(4096, 0)]),
        status(0, 1, 3, &[(4096, 0)]),
        status(D, 1, 2, &[(100, 1), (3996, 0)]),
    ]
    .concat();
    let (client, _server) = scripted(&[hs.clone(), ok].concat()).await;
    let mut client = Client::handshake(client, "", &["b0", "b1"], small)
        .await
        .unwrap();
    let got = client.block_status(0, 4096).await.unwrap();
    assert_eq!(got.allocation.len(), 1);
    assert_eq!(
        got.dirty
            .unwrap()
            .iter()
            .map(|d| (d.len, d.flags))
            .collect::<Vec<_>>(),
        vec![(100, 1), (3996, 0)]
    );

    for r in status_rows() {
        let (client, _server) = scripted(&[hs.clone(), r.bytes].concat()).await;
        let mut client = Client::handshake(client, "", &["b0", "b1"], small)
            .await
            .unwrap();
        let got = client.block_status(0, 4096).await.map(|_| ());
        expect(r.name, Kind::Protocol, got);
    }
}

#[tokio::test(start_paused = true)]
async fn client_times_out_on_a_silent_server() {
    let (client, _server) = duplex(1 << 20);
    let got = Client::handshake(client, "", &[], limits())
        .await
        .map(|_| ());
    expect("silent handshake", Kind::Timeout, got);

    let (client, _server) = scripted(&script::handshake(M, None, &[])).await;
    let mut client = Client::handshake(client, "", &[], limits()).await.unwrap();
    expect("silent read", Kind::Timeout, client.read(0, 4096).await);
    expect("poisoned read", Kind::Closed, client.read(0, 4096).await);
    expect(
        "poisoned status",
        Kind::Closed,
        client.block_status(0, 4096).await,
    );
}

#[tokio::test(start_paused = true)]
async fn client_sanitizes_server_error_text() {
    use script::*;
    let long = vec![b'a'; 4100];
    let replies = [
        error(D, REPLY_ERROR, 1, EIO, b"bad\x1b[31m\nred\x07"),
        error(D, REPLY_ERROR, 2, EIO, &long),
        data(D, 3, 0, &[1; 4096]),
    ]
    .concat();
    let (client, _server) = scripted(&[handshake(M, None, &[]), replies].concat()).await;
    let mut client = Client::handshake(client, "", &[], limits()).await.unwrap();
    let Err(err @ Error::Remote { errno: EIO, .. }) = client.read(0, 4096).await else {
        panic!("expected a remote error");
    };
    let Error::Remote { message, .. } = &err else {
        unreachable!()
    };
    assert_eq!(message, "bad[31mred");
    assert!(!err.to_string().chars().any(char::is_control));
    let Err(Error::Remote { message, .. }) = client.read(0, 4096).await else {
        panic!("expected a remote error");
    };
    assert_eq!(message.len(), 4096);
    assert_eq!(client.read(0, 4096).await.unwrap(), vec![1; 4096]);

    let (client, _server) = scripted(
        &[
            handshake(M, None, &[]),
            error(D, REPLY_ERROR, 1, EIO, &vec![b'a'; 5000]),
        ]
        .concat(),
    )
    .await;
    let mut client = Client::handshake(client, "", &[], limits()).await.unwrap();
    expect(
        "message beyond the chunk limit",
        Kind::Protocol,
        client.read(0, 4096).await,
    );
}

fn client_flags(flags: u32) -> Vec<u8> {
    flags.to_be_bytes().to_vec()
}

fn name_and_queries(name: &str, queries: &[&str]) -> Vec<u8> {
    let mut v = (name.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(name.as_bytes());
    v.extend_from_slice(&(queries.len() as u32).to_be_bytes());
    for q in queries {
        v.extend_from_slice(&(q.len() as u32).to_be_bytes());
        v.extend_from_slice(q.as_bytes());
    }
    v
}

fn go_data(name: &str) -> Vec<u8> {
    let mut v = (name.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(name.as_bytes());
    v.extend_from_slice(&1u16.to_be_bytes());
    v.extend_from_slice(&INFO_BLOCK_SIZE.to_be_bytes());
    v
}

fn session_prefix() -> Vec<u8> {
    [
        client_flags(3),
        option_request(OPT_STRUCTURED_REPLY, &[]),
        option_request(
            OPT_SET_META_CONTEXT,
            &name_and_queries("", &[CONTEXT_ALLOCATION]),
        ),
        option_request(OPT_GO, &go_data("")),
    ]
    .concat()
}

async fn run_server(
    script: Vec<u8>,
    export: &mut MemExport,
    limits: &Limits,
) -> (Result<(), Error>, Vec<u8>) {
    let (client, server) = duplex(64 << 10);
    let (mut read, mut write) = tokio::io::split(client);
    let writer = async move {
        let _ = write.write_all(&script).await;
        let _ = write.shutdown().await;
    };
    let reader = async move {
        let mut out = Vec::new();
        let _ = read.read_to_end(&mut out).await;
        out
    };
    let (served, out, ()) = tokio::join!(serve(server, export, limits), reader, writer);
    (served, out)
}

struct Out<'a>(lodestore_nbd::wire::Take<'a>);

impl Out<'_> {
    fn option_reply(&mut self) -> (u32, u32, Vec<u8>) {
        assert_eq!(self.0.u64(), Some(OPT_REPLY_MAGIC));
        let option = self.0.u32().unwrap();
        let ty = self.0.u32().unwrap();
        let len = self.0.u32().unwrap() as usize;
        (option, ty, self.0.bytes(len).unwrap().to_vec())
    }

    fn handshake(&mut self) {
        assert_eq!(self.0.bytes(18).unwrap().len(), 18);
        loop {
            let (option, ty, _) = self.option_reply();
            if option == OPT_GO && ty == REP_ACK {
                return;
            }
        }
    }

    fn chunk(&mut self) -> (u16, u16, u64, Vec<u8>) {
        assert_eq!(self.0.u32(), Some(STRUCTURED_REPLY_MAGIC));
        let flags = self.0.u16().unwrap();
        let ty = self.0.u16().unwrap();
        let cookie = self.0.u64().unwrap();
        let len = self.0.u32().unwrap() as usize;
        (flags, ty, cookie, self.0.bytes(len).unwrap().to_vec())
    }

    fn expect_errno(&mut self, cookie: u64, errno: u32) {
        let (flags, ty, got_cookie, payload) = self.chunk();
        assert_eq!((flags, ty, got_cookie), (D, REPLY_ERROR, cookie));
        assert_eq!(
            payload.get(..4),
            Some(&errno.to_be_bytes()[..]),
            "cookie {cookie}"
        );
    }
}

#[tokio::test]
async fn server_rejects_hostile_options() {
    let l = Limits::default();
    let big_option = [
        client_flags(3),
        IHAVEOPT.to_be_bytes().to_vec(),
        OPT_GO.to_be_bytes().to_vec(),
        (1u32 << 30).to_be_bytes().to_vec(),
    ]
    .concat();
    let mut wrong_magic = [client_flags(3), option_request(OPT_GO, &go_data(""))].concat();
    wrong_magic[4] ^= 1;
    let forty = [
        client_flags(3),
        (0..40).flat_map(|_| option_request(99, &[])).collect(),
    ]
    .concat();
    for (name, script) in [
        ("bad IHAVEOPT magic", wrong_magic),
        ("option of 1 GiB", big_option),
        ("40 options", forty.clone()),
        ("unknown client flag", client_flags(0x80)),
        ("no fixed newstyle", client_flags(0)),
    ] {
        let mut export = MemExport::new(pattern(M));
        let (served, out) = run_server(script, &mut export, &l).await;
        expect(name, Kind::Protocol, served);
        if name == "40 options" {
            let mut o = Out(Take(&out));
            o.0.bytes(18).unwrap();
            for _ in 0..32 {
                assert_eq!(o.option_reply().1, REP_ERR_UNSUP);
            }
            assert!(o.0.is_empty(), "no reply after the 32nd option");
        }
    }

    let go_first = [
        client_flags(3),
        option_request(OPT_GO, &go_data("")),
        option_request(OPT_STRUCTURED_REPLY, &[1]),
        option_request(OPT_SET_META_CONTEXT, &name_and_queries("other", &[])),
        option_request(OPT_SET_META_CONTEXT, &[1, 2]),
        option_request(OPT_STRUCTURED_REPLY, &[]),
        option_request(OPT_GO, &go_data("other")),
    ]
    .concat();
    let mut export = MemExport::new(pattern(M));
    let (served, out) = run_server(go_first, &mut export, &l).await;
    expect("script ends mid-session", Kind::Closed, served);
    let mut o = Out(Take(&out));
    o.0.bytes(18).unwrap();
    let replies: Vec<u32> = (0..6).map(|_| o.option_reply().1).collect();
    assert_eq!(
        replies,
        [
            REP_ERR_UNSUP,
            REP_ERR_INVALID,
            REP_ERR_UNKNOWN,
            REP_ERR_INVALID,
            REP_ACK,
            REP_ERR_UNKNOWN
        ]
    );
}

#[tokio::test]
async fn server_rejects_hostile_requests() {
    let data = pattern(M);
    let mut export = MemExport::new(data.clone());
    let l = Limits::default();
    let script = [
        session_prefix(),
        request(CMD_READ, 1, u64::MAX - 5, 4096),
        request(CMD_READ, 2, M - 100, 4096),
        request(CMD_READ, 3, 0, 1 << 30),
        request(99, 4, 0, 0),
        request(CMD_READ, 5, 0, 0),
        request(CMD_READ, 6, 0, 4096),
        request(CMD_BLOCK_STATUS, 7, 0, 0),
        request(CMD_BLOCK_STATUS, 8, M, 4096),
        request(CMD_READ, 9, 4096, 4096),
        {
            let mut bad = request(CMD_READ, 10, 0, 4096);
            bad[0] ^= 1;
            bad
        },
    ]
    .concat();
    let (served, out) = run_server(script, &mut export, &l).await;
    expect("bad request magic ends the session", Kind::Protocol, served);
    let mut o = Out(Take(&out));
    o.handshake();
    o.expect_errno(1, EINVAL);
    o.expect_errno(2, EINVAL);
    o.expect_errno(3, EOVERFLOW);
    o.expect_errno(4, EINVAL);
    o.expect_errno(5, EINVAL);
    let (flags, ty, cookie, payload) = o.chunk();
    assert_eq!((flags, ty, cookie), (D, REPLY_OFFSET_DATA, 6));
    assert_eq!(payload.get(8..), Some(&data[..4096]));
    o.expect_errno(7, EINVAL);
    o.expect_errno(8, EINVAL);
    let (_, ty, cookie, payload) = o.chunk();
    assert_eq!((ty, cookie), (REPLY_OFFSET_DATA, 9));
    assert_eq!(payload.get(8..), Some(&data[4096..8192]));
    assert!(o.0.is_empty());
}

#[tokio::test]
async fn server_is_read_only() {
    let data = pattern(M);
    let mut export = MemExport::new(data.clone());
    let l = Limits::default();
    let script = [
        session_prefix(),
        request(CMD_WRITE, 1, 0, 4096),
        vec![0xee; 4096],
        request(CMD_TRIM, 2, 0, 4096),
        request(CMD_WRITE_ZEROES, 3, 0, 4096),
        request(CMD_FLUSH, 4, 0, 0),
        request(CMD_CACHE, 5, 0, 4096),
        request(CMD_READ, 6, 0, 4096),
        request(CMD_DISC, 7, 0, 0),
    ]
    .concat();
    let (served, out) = run_server(script, &mut export, &l).await;
    served.unwrap();
    let mut o = Out(Take(&out));
    o.handshake();
    for cookie in 1..=5 {
        o.expect_errno(cookie, EPERM);
    }
    let (_, ty, cookie, payload) = o.chunk();
    assert_eq!((ty, cookie), (REPLY_OFFSET_DATA, 6));
    assert_eq!(payload.get(8..), Some(&data[..4096]));
    assert!(o.0.is_empty());

    let script = [session_prefix(), request(CMD_WRITE, 1, 0, (4 << 20) + 1)].concat();
    let (served, out) = run_server(script, &mut export, &l).await;
    expect("oversized write", Kind::Protocol, served);
    let mut o = Out(Take(&out));
    o.handshake();
    assert!(o.0.is_empty(), "an oversized write gets no reply");
}

/// A reply of one-byte holes in descending order is checked in time linear in its size.
#[tokio::test]
async fn client_reads_many_descending_holes_fast() {
    const N: usize = 262_144;
    let hs = script::handshake(M, None, &[]);
    let mut bytes = hs;
    for i in (0..N).rev() {
        let flags = if i == 0 { D } else { 0 };
        bytes.extend_from_slice(&script::hole(flags, 1, REQ_OFF + i as u64, 1));
    }
    let (client, mut server) = duplex(16 << 20);
    server.write_all(&bytes).await.unwrap();
    let limits = Limits {
        max_extents: N,
        op_timeout: Duration::from_secs(10),
        ..limits()
    };
    let mut client = Client::handshake(client, "", &[], limits).await.unwrap();
    let started = std::time::Instant::now();
    let got = client.read(REQ_OFF, N as u32).await.unwrap();
    let elapsed = started.elapsed();
    assert_eq!(got.len(), N);
    assert!(got.iter().all(|&b| b == 0));
    assert!(
        elapsed < Duration::from_secs(5),
        "{N} descending holes took {elapsed:?}"
    );
    drop(server);
}
