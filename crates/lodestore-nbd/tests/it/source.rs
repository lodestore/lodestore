use lodestore_format::image::{ImageExtent, ImageSource};
use lodestore_nbd::source::{MAX_PIECES, MAX_STATUS_STEPS};
use lodestore_nbd::wire::{STATE_DIRTY, STATE_ZERO};
use lodestore_nbd::{Client, Descriptor, Export, Limits, MemExport, NbdSource, serve};
use tokio::io::duplex;

const K: u64 = 1024;
const M: u64 = 1 << 20;

fn pattern(len: u64) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 31 + (i >> 12)) % 251 + 1) as u8)
        .collect()
}

fn d(len: u64, flags: u32) -> Descriptor {
    Descriptor {
        len: u32::try_from(len).unwrap(),
        flags,
    }
}

fn e(len: u64, zero: bool, dirty: bool) -> ImageExtent {
    ImageExtent { len, zero, dirty }
}

async fn with_source<E: Export, T>(
    export: &mut E,
    bitmaps: &[&str],
    limits: Limits,
    f: impl AsyncFnOnce(&mut NbdSource<tokio::io::DuplexStream>) -> T,
) -> T {
    let (c, s) = duplex(64 << 10);
    let server = serve(s, export, &limits);
    let client = async {
        let client = Client::handshake(c, "", bitmaps, limits).await.unwrap();
        let mut source = NbdSource::new(client);
        let out = f(&mut source).await;
        source.into_client().disconnect().await.unwrap();
        out
    };
    let (served, out) = tokio::join!(server, client);
    served.unwrap();
    out
}

type DirtyFn = Box<dyn FnMut(&str, u64, u32) -> Vec<Descriptor>>;

struct ScriptedExport {
    size: u64,
    bitmaps: Vec<&'static str>,
    allocation: Box<dyn FnMut(u64, u32) -> Vec<Descriptor>>,
    dirty: DirtyFn,
    requests: usize,
}

impl ScriptedExport {
    fn new(
        size: u64,
        bitmaps: &[&'static str],
        allocation: impl FnMut(u64, u32) -> Vec<Descriptor> + 'static,
        dirty: impl FnMut(&str, u64, u32) -> Vec<Descriptor> + 'static,
    ) -> Self {
        Self {
            size,
            bitmaps: bitmaps.to_vec(),
            allocation: Box::new(allocation),
            dirty: Box::new(dirty),
            requests: 0,
        }
    }
}

impl Export for ScriptedExport {
    fn size(&self) -> u64 {
        self.size
    }

    fn name(&self) -> &str {
        ""
    }

    fn bitmaps(&self) -> Vec<&str> {
        self.bitmaps.clone()
    }

    fn read(&mut self, _offset: u64, len: u32) -> Result<Vec<u8>, u32> {
        Ok(vec![0; len as usize])
    }

    fn allocation(&mut self, offset: u64, len: u32) -> Vec<Descriptor> {
        self.requests += 1;
        (self.allocation)(offset, len)
    }

    fn dirty(&mut self, name: &str, offset: u64, len: u32) -> Vec<Descriptor> {
        (self.dirty)(name, offset, len)
    }
}

fn runs(offset: u64, len: u64, f: impl Fn(u64) -> u32) -> Vec<Descriptor> {
    let mut out: Vec<Descriptor> = Vec::new();
    let mut pos = offset;
    while pos < offset + len {
        let next = ((pos / 4096 + 1) * 4096).min(offset + len);
        let flags = f(pos / 4096);
        match out.last_mut() {
            Some(last) if last.flags == flags => last.len += (next - pos) as u32,
            _ => out.push(d(next - pos, flags)),
        }
        pos = next;
    }
    out
}

fn full(len: u32) -> Vec<Descriptor> {
    vec![d(u64::from(len), 0)]
}

fn dirty_never(_: &str, _: u64, _: u32) -> Vec<Descriptor> {
    panic!("no dirty context was negotiated")
}

fn total(extents: &[ImageExtent]) -> u64 {
    extents.iter().map(|x| x.len).sum()
}

#[tokio::test]
async fn nbd_source_merges_allocation_and_dirty() {
    let mut data = pattern(8 * M);
    data[(64 * K) as usize..M as usize].fill(0);
    let mut export = MemExport::new(data).with_bitmap("b0", vec![4 * K..8 * K, 2 * M..3 * M]);
    with_source(&mut export, &["b0"], Limits::default(), async |s| {
        assert!(s.has_dirty());
        assert!(s.has_context("b0"));
        assert_eq!(s.size(), 8 * M);
        let got = s.extents(0, 4 * M).await.unwrap();
        assert_eq!(
            got,
            vec![
                e(4 * K, false, false),
                e(4 * K, false, true),
                e(56 * K, false, false),
                e(M - 64 * K, true, false),
                e(M, false, false),
                e(M, false, true),
                e(M, false, false),
            ]
        );
    })
    .await;
}

#[tokio::test]
async fn nbd_source_reads_across_the_payload_limit() {
    let data = pattern(8 * M);
    let mut export = MemExport::new(data.clone());
    let limits = Limits {
        max_read: 64 * K as u32,
        ..Limits::default()
    };
    with_source(&mut export, &[], limits, async |s| {
        assert_eq!(
            s.read(0, 4 * M as usize).await.unwrap(),
            data[..(4 * M) as usize]
        );
    })
    .await;
    assert_eq!(export.bytes_read(), 4 * M);
}

#[tokio::test]
async fn nbd_source_caps_pieces() {
    let ranges = (0..MAX_PIECES as u64 * 4)
        .step_by(2)
        .map(|i| i..i + 1)
        .collect();
    let mut export = MemExport::new(pattern(4 * M)).with_bitmap("b0", ranges);
    with_source(&mut export, &["b0"], Limits::default(), async |s| {
        let err = s.extents(0, 4 * M).await.unwrap_err();
        assert!(err.to_string().contains("too many pieces"), "{err}");
    })
    .await;
}

#[tokio::test]
async fn nbd_source_without_bitmap_is_never_dirty() {
    let mut export = MemExport::new(pattern(M)).with_bitmap("b0", vec![0..4 * K]);
    with_source(&mut export, &[], Limits::default(), async |s| {
        assert!(!s.has_dirty());
        let got = s.extents(0, M).await.unwrap();
        assert_eq!(got, vec![e(M, false, false)]);
    })
    .await;
}

#[tokio::test]
async fn nbd_source_aligns_to_the_minimum_block() {
    let size = M + 8 * K;
    let data = pattern(size);
    let mut export = MemExport::new(data.clone()).min_block(4096);
    with_source(&mut export, &[], Limits::default(), async |s| {
        assert_eq!(total(&s.extents(1000, 5000).await.unwrap()), 5000);
        assert_eq!(s.read(1000, 5000).await.unwrap(), data[1000..6000]);
        let tail = size - 300;
        assert_eq!(total(&s.extents(tail, 300).await.unwrap()), 300);
        assert_eq!(s.read(tail, 300).await.unwrap(), data[tail as usize..]);
    })
    .await;
}

#[tokio::test]
async fn nbd_source_survives_hostile_status() {
    let zero_at = |b: u64| (128..256).contains(&b);
    let dirty_at = |b: u64| (b / 16) % 2 == 1;
    let flag = |zero: bool| if zero { STATE_ZERO } else { 0 };
    let dirty_flag = |dirty: bool| if dirty { STATE_DIRTY } else { 0 };

    // a bitmap that the server does not know is never dirty
    let mut export = ScriptedExport::new(M, &[], |_, len| full(len), dirty_never);
    with_source(&mut export, &["b0"], Limits::default(), async |s| {
        assert!(!s.has_dirty());
        assert!(!s.has_context("b0"));
        assert_eq!(s.extents(0, M).await.unwrap(), vec![e(M, false, false)]);
    })
    .await;

    // the last descriptor of every context reaches past the request
    let mut export = ScriptedExport::new(
        M,
        &["b0"],
        |_, len| vec![d(4096, 0), d(u64::from(len) - 4096 + 8192, STATE_ZERO)],
        |_, _, len| vec![d(4096, STATE_DIRTY), d(u64::from(len) - 4096 + 4096, 0)],
    );
    with_source(&mut export, &["b0"], Limits::default(), async |s| {
        let got = s.extents(0, 16 * K).await.unwrap();
        assert_eq!(got, vec![e(4096, false, true), e(12 * K, true, false)]);
    })
    .await;

    // different coverage per context leaves no gap and mixes no flags
    let mut export = ScriptedExport::new(
        2 * M,
        &["b0"],
        move |offset, len| runs(offset, u64::from(len).min(M), |b| flag(zero_at(b))),
        move |_, offset, len| {
            runs(offset, u64::from(len).min(64 * K), |b| {
                dirty_flag(dirty_at(b))
            })
        },
    );
    with_source(&mut export, &["b0"], Limits::default(), async |s| {
        let got = s.extents(0, 2 * M).await.unwrap();
        let mut want: Vec<ImageExtent> = Vec::new();
        for b in 0..2 * M / 4096 {
            let (zero, dirty) = (zero_at(b), dirty_at(b));
            match want.last_mut() {
                Some(l) if l.zero == zero && l.dirty == dirty => l.len += 4096,
                _ => want.push(e(4096, zero, dirty)),
            }
        }
        assert_eq!(got, want);
    })
    .await;

    // unknown flag bits mean nothing
    let mut export = ScriptedExport::new(
        M,
        &["b0"],
        |_, len| vec![d(u64::from(len), !STATE_ZERO)],
        |_, _, len| {
            vec![
                d(4096, !STATE_DIRTY),
                d(4096, STATE_DIRTY | 0xff00),
                d(u64::from(len) - 8192, !STATE_DIRTY),
            ]
        },
    );
    with_source(&mut export, &["b0"], Limits::default(), async |s| {
        let got = s.extents(0, 16 * K).await.unwrap();
        assert_eq!(
            got,
            vec![
                e(4096, false, false),
                e(4096, false, true),
                e(8 * K, false, false)
            ]
        );
    })
    .await;

    // an empty descriptor list is an error, not a loop
    let mut export = ScriptedExport::new(M, &["b0"], |_, len| full(len), |_, _, _| Vec::new());
    with_source(&mut export, &["b0"], Limits::default(), async |s| {
        assert!(s.extents(0, 16 * K).await.is_err());
    })
    .await;
    let mut export = ScriptedExport::new(
        M,
        &["b0"],
        |_, _| Vec::new(),
        |_, _, len| vec![d(u64::from(len), 0)],
    );
    with_source(&mut export, &["b0"], Limits::default(), async |s| {
        assert!(s.extents(0, 16 * K).await.is_err());
    })
    .await;

    // only the first bitmap decides what is dirty
    let mut export = ScriptedExport::new(
        M,
        &["g1", "g2"],
        |_, len| full(len),
        |name, _, len| {
            if name == "g1" {
                vec![d(4096, STATE_DIRTY), d(u64::from(len) - 4096, 0)]
            } else {
                vec![d(u64::from(len), STATE_DIRTY)]
            }
        },
    );
    with_source(&mut export, &["g1", "g2"], Limits::default(), async |s| {
        assert!(s.has_context("g2"));
        let got = s.extents(0, 16 * K).await.unwrap();
        assert_eq!(got, vec![e(4096, false, true), e(12 * K, false, false)]);
    })
    .await;
}

#[tokio::test]
async fn nbd_source_caps_status_steps() {
    let mut export = ScriptedExport::new(4 * M, &[], |_, _| vec![d(1, 0)], dirty_never);
    with_source(&mut export, &[], Limits::default(), async |s| {
        let err = s.extents(0, 4 * M).await.unwrap_err();
        assert!(err.to_string().contains("too little progress"), "{err}");
    })
    .await;
    assert_eq!(export.requests, MAX_STATUS_STEPS);
}
