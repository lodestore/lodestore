use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::client::Descriptor;
use crate::wire::{
    BITMAP_CONTEXT_PREFIX, CMD_BLOCK_STATUS, CMD_CACHE, CMD_DISC, CMD_FLUSH, CMD_READ, CMD_TRIM,
    CMD_WRITE, CMD_WRITE_ZEROES, CONTEXT_ALLOCATION, EINVAL, EIO, EOVERFLOW, EPERM,
    FLAG_FIXED_NEWSTYLE, FLAG_HAS_FLAGS, FLAG_NO_ZEROES, FLAG_READ_ONLY, IHAVEOPT, INFO_BLOCK_SIZE,
    INFO_EXPORT, NBDMAGIC, OPT_ABORT, OPT_GO, OPT_SET_META_CONTEXT, OPT_STRUCTURED_REPLY, REP_ACK,
    REP_ERR_INVALID, REP_ERR_UNKNOWN, REP_ERR_UNSUP, REP_INFO, REP_META_CONTEXT,
    REPLY_BLOCK_STATUS, REPLY_ERROR, REPLY_FLAG_DONE, REPLY_OFFSET_DATA, REQUEST_MAGIC,
    STATE_DIRTY, STATE_HOLE, STATE_ZERO, Take, discard, option_reply, read_exact,
    read_exact_bounded, read_u32, read_u64, structured_chunk, within,
};
use crate::{Error, Limits, Result};

const ALLOCATION_BLOCK: u64 = 64 << 10;

pub trait Export {
    fn size(&self) -> u64;
    fn name(&self) -> &str;
    fn bitmaps(&self) -> Vec<&str>;
    fn min_block(&self) -> u32 {
        1
    }
    fn read(&mut self, offset: u64, len: u32) -> std::result::Result<Vec<u8>, u32>;
    fn allocation(&mut self, offset: u64, len: u32) -> Vec<Descriptor>;
    fn dirty(&mut self, name: &str, offset: u64, len: u32) -> Vec<Descriptor>;
}

pub struct MemExport {
    name: String,
    data: Vec<u8>,
    bitmaps: Vec<(String, Vec<Range<u64>>)>,
    dirty_only: Arc<AtomicBool>,
    min: u32,
    fail_after: Option<u64>,
    reads: u64,
    bytes_read: u64,
    reads_outside_dirty: u64,
}

impl MemExport {
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            name: String::new(),
            data,
            bitmaps: Vec::new(),
            dirty_only: Arc::new(AtomicBool::new(false)),
            min: 1,
            fail_after: None,
            reads: 0,
            bytes_read: 0,
            reads_outside_dirty: 0,
        }
    }

    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.to_owned();
        self
    }

    pub fn with_bitmap(mut self, name: &str, mut ranges: Vec<Range<u64>>) -> Self {
        ranges.sort_by_key(|r| r.start);
        self.bitmaps.push((name.to_owned(), ranges));
        self
    }

    pub fn dirty_only(self, on: bool) -> Self {
        self.dirty_only.store(on, Ordering::SeqCst);
        self
    }

    pub fn dirty_only_switch(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.dirty_only)
    }

    pub fn min_block(mut self, n: u32) -> Self {
        self.min = n;
        self
    }

    pub fn fail_reads_after(mut self, k: u64) -> Self {
        self.fail_after = Some(k);
        self
    }

    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    pub fn reads_outside_dirty(&self) -> u64 {
        self.reads_outside_dirty
    }

    fn inside_first_bitmap(&self, start: u64, end: u64) -> bool {
        let Some((_, ranges)) = self.bitmaps.first() else {
            return false;
        };
        let mut pos = start;
        for r in ranges {
            if r.end <= pos {
                continue;
            }
            if r.start > pos {
                break;
            }
            pos = r.end;
            if pos >= end {
                return true;
            }
        }
        false
    }
}

fn push_merged(out: &mut Vec<Descriptor>, len: u64, flags: u32) {
    let Ok(len) = u32::try_from(len) else { return };
    match out.last_mut() {
        Some(last) if last.flags == flags && last.len.checked_add(len).is_some() => last.len += len,
        _ => out.push(Descriptor { len, flags }),
    }
}

impl Export for MemExport {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn bitmaps(&self) -> Vec<&str> {
        self.bitmaps.iter().map(|(n, _)| n.as_str()).collect()
    }

    fn min_block(&self) -> u32 {
        self.min
    }

    fn read(&mut self, offset: u64, len: u32) -> std::result::Result<Vec<u8>, u32> {
        self.reads += 1;
        let end = offset.checked_add(u64::from(len)).ok_or(EINVAL)?;
        if !self.inside_first_bitmap(offset, end) {
            self.reads_outside_dirty += 1;
            if self.dirty_only.load(Ordering::SeqCst) {
                return Err(EIO);
            }
        }
        if self.fail_after.is_some_and(|k| self.reads > k) {
            return Err(EIO);
        }
        let range = usize::try_from(offset).ok().zip(usize::try_from(end).ok());
        let data = range
            .and_then(|(s, e)| self.data.get(s..e))
            .ok_or(EINVAL)?
            .to_vec();
        self.bytes_read += u64::from(len);
        Ok(data)
    }

    fn allocation(&mut self, offset: u64, len: u32) -> Vec<Descriptor> {
        let end = offset + u64::from(len);
        let mut out = Vec::new();
        let mut pos = offset;
        while pos < end {
            let next = ((pos / ALLOCATION_BLOCK + 1) * ALLOCATION_BLOCK).min(end);
            let zero = usize::try_from(pos)
                .ok()
                .zip(usize::try_from(next).ok())
                .and_then(|(s, e)| self.data.get(s..e))
                .is_none_or(|b| b.iter().all(|x| *x == 0));
            let flags = if zero { STATE_HOLE | STATE_ZERO } else { 0 };
            push_merged(&mut out, next - pos, flags);
            pos = next;
        }
        out
    }

    fn dirty(&mut self, name: &str, offset: u64, len: u32) -> Vec<Descriptor> {
        let end = offset + u64::from(len);
        let Some((_, ranges)) = self.bitmaps.iter().find(|(n, _)| n == name) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut pos = offset;
        for r in ranges {
            let s = r.start.max(offset).max(pos);
            let e = r.end.min(end);
            if s >= e {
                continue;
            }
            if s > pos {
                push_merged(&mut out, s - pos, 0);
            }
            push_merged(&mut out, e - s, STATE_DIRTY);
            pos = e;
        }
        if pos < end {
            push_merged(&mut out, end - pos, 0);
        }
        out
    }
}

enum Context {
    Allocation,
    Dirty(String),
}

#[derive(Default)]
struct Session {
    structured: bool,
    contexts: Vec<(u32, Context)>,
}

enum Flow {
    Continue,
    Go,
    Abort,
}

fn parse_name_and_queries(data: &[u8]) -> Option<(&str, Vec<&str>)> {
    let mut t = Take(data);
    let n = usize::try_from(t.u32()?).ok()?;
    let name = std::str::from_utf8(t.bytes(n)?).ok()?;
    let count = t.u32()?;
    let mut queries = Vec::new();
    for _ in 0..count {
        let n = usize::try_from(t.u32()?).ok()?;
        queries.push(std::str::from_utf8(t.bytes(n)?).ok()?);
    }
    Some((name, queries))
}

fn parse_go(data: &[u8]) -> Option<(&str, Vec<u16>)> {
    let mut t = Take(data);
    let n = usize::try_from(t.u32()?).ok()?;
    let name = std::str::from_utf8(t.bytes(n)?).ok()?;
    let count = t.u16()?;
    let mut infos = Vec::new();
    for _ in 0..count {
        infos.push(t.u16()?);
    }
    Some((name, infos))
}

async fn option<S, E>(
    stream: &mut S,
    export: &E,
    limits: &Limits,
    session: &mut Session,
) -> Result<Flow>
where
    S: AsyncRead + AsyncWrite + Unpin,
    E: Export,
{
    if read_u64(stream).await? != IHAVEOPT {
        return Err(Error::Protocol("bad option magic".into()));
    }
    let opt = read_u32(stream).await?;
    let len = read_u32(stream).await?;
    let data = read_exact_bounded(stream, u64::from(len), limits.max_option_payload).await?;
    let mut out = Vec::new();
    let mut flow = Flow::Continue;
    match opt {
        OPT_ABORT => {
            out = option_reply(opt, REP_ACK, &[]);
            flow = Flow::Abort;
        }
        OPT_STRUCTURED_REPLY if data.is_empty() => {
            session.structured = true;
            out = option_reply(opt, REP_ACK, &[]);
        }
        OPT_STRUCTURED_REPLY => out = option_reply(opt, REP_ERR_INVALID, &[]),
        OPT_SET_META_CONTEXT => match parse_name_and_queries(&data) {
            Some((name, queries)) if name == export.name() => {
                session.contexts.clear();
                let bitmaps = export.bitmaps();
                for q in queries {
                    let ctx = if q == CONTEXT_ALLOCATION {
                        Some(Context::Allocation)
                    } else {
                        q.strip_prefix(BITMAP_CONTEXT_PREFIX)
                            .filter(|b| bitmaps.contains(b))
                            .map(|b| Context::Dirty(b.to_owned()))
                    };
                    let duplicate = session.contexts.iter().any(|(_, c)| match (c, &ctx) {
                        (Context::Allocation, Some(Context::Allocation)) => true,
                        (Context::Dirty(a), Some(Context::Dirty(b))) => a == b,
                        _ => false,
                    });
                    if let (Some(ctx), false) = (ctx, duplicate) {
                        let id = session.contexts.len() as u32 + 1;
                        let mut payload = id.to_be_bytes().to_vec();
                        payload.extend_from_slice(q.as_bytes());
                        out.extend(option_reply(opt, REP_META_CONTEXT, &payload));
                        session.contexts.push((id, ctx));
                    }
                }
                out.extend(option_reply(opt, REP_ACK, &[]));
            }
            Some(_) => out = option_reply(opt, REP_ERR_UNKNOWN, &[]),
            None => out = option_reply(opt, REP_ERR_INVALID, &[]),
        },
        OPT_GO => match parse_go(&data) {
            Some((name, _)) if name != export.name() => {
                out = option_reply(opt, REP_ERR_UNKNOWN, &[]);
            }
            Some(_) if !session.structured => out = option_reply(opt, REP_ERR_UNSUP, &[]),
            Some((_, infos)) => {
                let mut info = INFO_EXPORT.to_be_bytes().to_vec();
                info.extend_from_slice(&export.size().to_be_bytes());
                info.extend_from_slice(&(FLAG_HAS_FLAGS | FLAG_READ_ONLY).to_be_bytes());
                out.extend(option_reply(opt, REP_INFO, &info));
                if infos.contains(&INFO_BLOCK_SIZE) {
                    let min = export.min_block();
                    let mut info = INFO_BLOCK_SIZE.to_be_bytes().to_vec();
                    info.extend_from_slice(&min.to_be_bytes());
                    info.extend_from_slice(&4096.max(min).to_be_bytes());
                    info.extend_from_slice(&limits.max_read.to_be_bytes());
                    out.extend(option_reply(opt, REP_INFO, &info));
                }
                out.extend(option_reply(opt, REP_ACK, &[]));
                flow = Flow::Go;
            }
            None => out = option_reply(opt, REP_ERR_INVALID, &[]),
        },
        _ => out = option_reply(opt, REP_ERR_UNSUP, &[]),
    }
    stream.write_all(&out).await?;
    stream.flush().await?;
    Ok(flow)
}

fn error_chunk(cookie: u64, errno: u32) -> Vec<u8> {
    let mut payload = errno.to_be_bytes().to_vec();
    payload.extend_from_slice(&0u16.to_be_bytes());
    structured_chunk(REPLY_FLAG_DONE, REPLY_ERROR, cookie, &payload)
}

fn range_errno<E: Export>(export: &E, offset: u64, length: u32) -> Option<u32> {
    let in_range = offset
        .checked_add(u64::from(length))
        .is_some_and(|end| length != 0 && end <= export.size());
    if !in_range {
        return Some(EINVAL);
    }
    let m = u64::from(export.min_block());
    if m > 1 && (!offset.is_multiple_of(m) || !u64::from(length).is_multiple_of(m)) {
        return Some(EINVAL);
    }
    None
}

pub async fn serve<S, E>(mut stream: S, export: &mut E, limits: &Limits) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
    E: Export,
{
    let mut greeting = Vec::with_capacity(18);
    greeting.extend_from_slice(&NBDMAGIC.to_be_bytes());
    greeting.extend_from_slice(&IHAVEOPT.to_be_bytes());
    greeting.extend_from_slice(&(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES).to_be_bytes());
    within(limits, async {
        stream.write_all(&greeting).await?;
        stream.flush().await?;
        Ok(())
    })
    .await?;
    let client_flags = within(limits, read_u32(&mut stream)).await?;
    let known = u32::from(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES);
    if client_flags & !known != 0 || client_flags & u32::from(FLAG_FIXED_NEWSTYLE) == 0 {
        return Err(Error::Protocol("unsupported client flags".into()));
    }

    let mut session = Session::default();
    let mut seen = 0;
    loop {
        seen += 1;
        if seen > limits.max_options {
            return Err(Error::Protocol("too many options".into()));
        }
        match within(limits, option(&mut stream, &*export, limits, &mut session)).await? {
            Flow::Continue => {}
            Flow::Go => break,
            Flow::Abort => return Ok(()),
        }
    }

    loop {
        let mut magic = [0u8; 4];
        match read_exact(&mut stream, &mut magic).await {
            Ok(()) => {}
            Err(Error::Closed) => return Ok(()),
            Err(e) => return Err(e),
        }
        if u32::from_be_bytes(magic) != REQUEST_MAGIC {
            return Err(Error::Protocol("bad request magic".into()));
        }
        let mut rest = [0u8; 24];
        within(limits, read_exact(&mut stream, &mut rest)).await?;
        let mut t = Take(&rest);
        let (Some(_flags), Some(ty), Some(cookie), Some(offset), Some(length)) =
            (t.u16(), t.u16(), t.u64(), t.u64(), t.u32())
        else {
            return Err(Error::Protocol("short request".into()));
        };
        let reply = match ty {
            CMD_DISC => return Ok(()),
            CMD_READ => read_reply(export, limits, cookie, offset, length),
            CMD_BLOCK_STATUS => status_reply(export, limits, &session, cookie, offset, length),
            CMD_WRITE => {
                if u64::from(length) > u64::from(limits.max_read) {
                    return Err(Error::Protocol("a write larger than a read may be".into()));
                }
                within(limits, discard(&mut stream, u64::from(length))).await?;
                error_chunk(cookie, EPERM)
            }
            CMD_FLUSH | CMD_TRIM | CMD_CACHE | CMD_WRITE_ZEROES => error_chunk(cookie, EPERM),
            _ => error_chunk(cookie, EINVAL),
        };
        within(limits, async {
            stream.write_all(&reply).await?;
            stream.flush().await?;
            Ok(())
        })
        .await?;
    }
}

fn read_reply<E: Export>(
    export: &mut E,
    limits: &Limits,
    cookie: u64,
    offset: u64,
    length: u32,
) -> Vec<u8> {
    if length > limits.max_read {
        return error_chunk(cookie, EOVERFLOW);
    }
    if let Some(errno) = range_errno(export, offset, length) {
        return error_chunk(cookie, errno);
    }
    match export.read(offset, length) {
        Ok(data) if data.len() as u64 == u64::from(length) => {
            let mut payload = offset.to_be_bytes().to_vec();
            payload.extend_from_slice(&data);
            structured_chunk(REPLY_FLAG_DONE, REPLY_OFFSET_DATA, cookie, &payload)
        }
        Ok(_) => error_chunk(cookie, EIO),
        Err(errno) => error_chunk(cookie, errno),
    }
}

fn status_reply<E: Export>(
    export: &mut E,
    limits: &Limits,
    session: &Session,
    cookie: u64,
    offset: u64,
    length: u32,
) -> Vec<u8> {
    if let Some(errno) = range_errno(export, offset, length) {
        return error_chunk(cookie, errno);
    }
    if session.contexts.is_empty() {
        return error_chunk(cookie, EINVAL);
    }
    let mut out = Vec::new();
    for (i, (id, ctx)) in session.contexts.iter().enumerate() {
        let mut descriptors = match ctx {
            Context::Allocation => export.allocation(offset, length),
            Context::Dirty(name) => export.dirty(name, offset, length),
        };
        descriptors.truncate(limits.max_extents);
        let mut payload = id.to_be_bytes().to_vec();
        for d in descriptors {
            payload.extend_from_slice(&d.len.to_be_bytes());
            payload.extend_from_slice(&d.flags.to_be_bytes());
        }
        let flags = if i + 1 == session.contexts.len() {
            REPLY_FLAG_DONE
        } else {
            0
        };
        out.extend(structured_chunk(
            flags,
            REPLY_BLOCK_STATUS,
            cookie,
            &payload,
        ));
    }
    out
}
