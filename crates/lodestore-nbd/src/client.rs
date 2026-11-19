use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::wire::{
    BITMAP_CONTEXT_PREFIX, CMD_BLOCK_STATUS, CMD_DISC, CMD_READ, CONTEXT_ALLOCATION,
    FLAG_FIXED_NEWSTYLE, FLAG_HAS_FLAGS, FLAG_NO_ZEROES, FLAG_READ_ONLY, IHAVEOPT, INFO_BLOCK_SIZE,
    INFO_EXPORT, NBDMAGIC, OPT_GO, OPT_REPLY_MAGIC, OPT_SET_META_CONTEXT, OPT_STRUCTURED_REPLY,
    REP_ACK, REP_ERROR_BIT, REP_INFO, REP_META_CONTEXT, REPLY_BLOCK_STATUS, REPLY_ERROR_BIT,
    REPLY_ERROR_OFFSET, REPLY_FLAG_DONE, REPLY_NONE, REPLY_OFFSET_DATA, REPLY_OFFSET_HOLE,
    SIMPLE_REPLY_MAGIC, STATE_DIRTY, STATE_HOLE, STATE_ZERO, STRUCTURED_REPLY_MAGIC, Take,
    option_request, read_exact, read_exact_bounded, read_u16, read_u32, read_u64, request,
    to_usize, within,
};
use crate::{Error, Limits, NbdUri, Result};

const MAX_INFO_REPLIES: usize = 16;
const MAX_BITMAPS: usize = 2;

fn protocol<T>(what: &str) -> Result<T> {
    Err(Error::Protocol(what.to_owned()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSize {
    pub min: u32,
    pub preferred: u32,
    pub max: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub len: u32,
    pub flags: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockStatus {
    pub allocation: Vec<Descriptor>,
    pub dirty: Option<Vec<Descriptor>>,
}

pub struct Client<S> {
    stream: S,
    limits: Limits,
    size: u64,
    read_only: bool,
    block: BlockSize,
    alloc_id: u32,
    bitmaps: Vec<(String, Option<u32>)>,
    cookie: u64,
    broken: bool,
}

struct Negotiated {
    size: u64,
    read_only: bool,
    block: BlockSize,
    alloc_id: u32,
    ids: Vec<Option<u32>>,
}

enum Header {
    Simple { errno: u32 },
    Chunk { flags: u16, ty: u16, len: u32 },
}

fn validate_block(min: u32, preferred: u32, max: u32) -> Result<BlockSize> {
    let ok = min.is_power_of_two()
        && min <= 65_536
        && preferred.is_power_of_two()
        && preferred >= min.max(512)
        && max >= preferred;
    if ok {
        Ok(BlockSize {
            min,
            preferred,
            max,
        })
    } else {
        protocol("the server announced an inconsistent block size")
    }
}

struct OptReply {
    ty: u32,
    payload: Vec<u8>,
}

async fn read_option_reply<S: AsyncRead + Unpin>(
    stream: &mut S,
    option: u32,
    limits: &Limits,
) -> Result<OptReply> {
    if read_u64(stream).await? != OPT_REPLY_MAGIC {
        return protocol("bad option reply magic");
    }
    if read_u32(stream).await? != option {
        return protocol("reply to an option that was not sent");
    }
    let ty = read_u32(stream).await?;
    let len = read_u32(stream).await?;
    let payload = read_exact_bounded(stream, u64::from(len), limits.max_option_payload).await?;
    if ty & REP_ERROR_BIT != 0 {
        return Err(Error::Refused { option, reply: ty });
    }
    match ty {
        REP_ACK if !payload.is_empty() => protocol("acknowledgement with a payload"),
        REP_ACK | REP_INFO | REP_META_CONTEXT => Ok(OptReply { ty, payload }),
        _ => protocol("unknown option reply type"),
    }
}

async fn expect_ack<S: AsyncRead + Unpin>(
    stream: &mut S,
    option: u32,
    limits: &Limits,
) -> Result<()> {
    match read_option_reply(stream, option, limits).await?.ty {
        REP_ACK => Ok(()),
        _ => protocol("expected an acknowledgement"),
    }
}

fn name_data(export: &str, extra: &[u8]) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&(export.len() as u32).to_be_bytes());
    data.extend_from_slice(export.as_bytes());
    data.extend_from_slice(extra);
    data
}

async fn negotiate<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    export: &str,
    bitmaps: &[&str],
    limits: &Limits,
) -> Result<Negotiated> {
    if read_u64(stream).await? != NBDMAGIC || read_u64(stream).await? != IHAVEOPT {
        return protocol("bad greeting");
    }
    let server_flags = read_u16(stream).await?;
    if server_flags & FLAG_FIXED_NEWSTYLE == 0 {
        return protocol("the server does not speak fixed newstyle");
    }
    let mut client_flags = u32::from(FLAG_FIXED_NEWSTYLE);
    if server_flags & FLAG_NO_ZEROES != 0 {
        client_flags |= u32::from(FLAG_NO_ZEROES);
    }
    stream.write_all(&client_flags.to_be_bytes()).await?;

    stream
        .write_all(&option_request(OPT_STRUCTURED_REPLY, &[]))
        .await?;
    expect_ack(stream, OPT_STRUCTURED_REPLY, limits).await?;

    // The first bitmap is the one read; the second proves the next one already exists (qemu:dirty-bitmap:NAME).
    let mut names = vec![CONTEXT_ALLOCATION.to_owned()];
    names.extend(
        bitmaps
            .iter()
            .map(|b| format!("{BITMAP_CONTEXT_PREFIX}{b}")),
    );
    let mut queries = (names.len() as u32).to_be_bytes().to_vec();
    for n in &names {
        queries.extend_from_slice(&(n.len() as u32).to_be_bytes());
        queries.extend_from_slice(n.as_bytes());
    }
    let data = name_data(export, &queries);
    if data.len() > limits.max_option_payload {
        return protocol("the context request is larger than an option may be");
    }
    stream
        .write_all(&option_request(OPT_SET_META_CONTEXT, &data))
        .await?;
    let mut confirmed: Vec<Option<u32>> = vec![None; names.len()];
    let mut count = 0;
    loop {
        let reply = read_option_reply(stream, OPT_SET_META_CONTEXT, limits).await?;
        match reply.ty {
            REP_ACK => break,
            REP_META_CONTEXT => {
                count += 1;
                if count > names.len() {
                    return protocol("more contexts than were requested");
                }
                let mut t = Take(&reply.payload);
                let Some(id) = t.u32() else {
                    return protocol("short context reply");
                };
                let name = std::str::from_utf8(t.0).ok();
                let slot = name
                    .and_then(|n| names.iter().position(|q| q == n))
                    .and_then(|i| confirmed.get_mut(i));
                match slot {
                    Some(s @ None) => *s = Some(id),
                    _ => return protocol("a context that was not requested, or repeated"),
                }
                let mut seen = confirmed.iter().flatten().collect::<Vec<_>>();
                seen.sort();
                seen.dedup();
                if seen.len() != confirmed.iter().flatten().count() {
                    return protocol("two contexts share an id");
                }
            }
            _ => return protocol("unexpected reply to the context request"),
        }
    }
    let mut confirmed = confirmed.into_iter();
    let Some(alloc_id) = confirmed.next().flatten() else {
        return protocol("the server does not offer base:allocation");
    };
    let ids: Vec<Option<u32>> = confirmed.collect();

    let mut go = name_data(export, &1u16.to_be_bytes());
    go.extend_from_slice(&INFO_BLOCK_SIZE.to_be_bytes());
    stream.write_all(&option_request(OPT_GO, &go)).await?;
    let mut export_info: Option<(u64, u16)> = None;
    let mut block: Option<BlockSize> = None;
    let mut replies = 0;
    loop {
        let reply = read_option_reply(stream, OPT_GO, limits).await?;
        match reply.ty {
            REP_ACK => break,
            REP_INFO => {
                replies += 1;
                if replies > MAX_INFO_REPLIES {
                    return protocol("too many information replies");
                }
                let total = reply.payload.len();
                let mut t = Take(&reply.payload);
                match t.u16() {
                    Some(INFO_EXPORT) => {
                        let (Some(size), Some(flags)) = (t.u64(), t.u16()) else {
                            return protocol("short export information");
                        };
                        if total != 12 || export_info.is_some() {
                            return protocol("malformed export information");
                        }
                        export_info = Some((size, flags));
                    }
                    Some(INFO_BLOCK_SIZE) => {
                        let (Some(min), Some(preferred), Some(max)) = (t.u32(), t.u32(), t.u32())
                        else {
                            return protocol("short block size information");
                        };
                        if total != 14 || block.is_some() {
                            return protocol("malformed block size information");
                        }
                        block = Some(validate_block(min, preferred, max)?);
                    }
                    Some(_) => {}
                    None => return protocol("short information reply"),
                }
            }
            _ => return protocol("unexpected reply to the export request"),
        }
    }
    let Some((size, flags)) = export_info else {
        return protocol("the server sent no export information");
    };
    if size > limits.max_export_size {
        return protocol(&format!(
            "the export is {size} bytes, more than the limit of {}",
            limits.max_export_size
        ));
    }
    let block = block.unwrap_or(BlockSize {
        min: 1,
        preferred: 4096,
        max: limits.max_read,
    });
    if !size.is_multiple_of(u64::from(block.min)) {
        return protocol(&format!(
            "the export size {size} is not a multiple of the server's minimum block size {}",
            block.min
        ));
    }
    Ok(Negotiated {
        size,
        read_only: flags & FLAG_HAS_FLAGS != 0 && flags & FLAG_READ_ONLY != 0,
        block,
        alloc_id,
        ids,
    })
}

fn sanitize(raw: &[u8], max: usize) -> String {
    let mut out = String::new();
    for c in String::from_utf8_lossy(raw)
        .chars()
        .filter(|c| !c.is_control())
    {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

fn place(
    covered: &mut Vec<(u64, u64)>,
    window: (u64, u64),
    offset: u64,
    len: u64,
    max_pieces: usize,
) -> Result<usize> {
    let end = offset.checked_add(len);
    let Some(end) = end.filter(|e| offset >= window.0 && *e <= window.1) else {
        return protocol("a data range outside the request");
    };
    if covered.len() >= max_pieces {
        return protocol("too many data pieces in one reply");
    }
    let i = covered.partition_point(|&(s, _)| s < offset);
    let overlaps_prev = i
        .checked_sub(1)
        .and_then(|p| covered.get(p))
        .is_some_and(|&(_, e)| e > offset);
    let overlaps_next = covered.get(i).is_some_and(|&(s, _)| s < end);
    if overlaps_prev || overlaps_next {
        return protocol("overlapping data ranges");
    }
    covered.insert(i, (offset, end));
    usize::try_from(offset - window.0).or_else(|_| protocol("data offset"))
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> Client<S> {
    pub async fn handshake(
        mut stream: S,
        export: &str,
        bitmaps: &[&str],
        limits: Limits,
    ) -> Result<Self> {
        if bitmaps.len() > MAX_BITMAPS {
            return protocol("at most two bitmaps may be requested");
        }
        if bitmaps.len() == 2 && bitmaps.first() == bitmaps.get(1) {
            return protocol("the same bitmap was requested twice");
        }
        let n = within(&limits, negotiate(&mut stream, export, bitmaps, &limits)).await?;
        let client = Self {
            stream,
            limits,
            size: n.size,
            read_only: n.read_only,
            block: n.block,
            alloc_id: n.alloc_id,
            bitmaps: bitmaps.iter().map(|b| (*b).to_owned()).zip(n.ids).collect(),
            cookie: 0,
            broken: false,
        };
        if client.max_read() == 0 {
            return protocol("the server's block size leaves no usable read length");
        }
        Ok(client)
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn block_size(&self) -> BlockSize {
        self.block
    }

    pub fn has_dirty(&self) -> bool {
        self.bitmaps.first().is_some_and(|(_, id)| id.is_some())
    }

    pub fn has_context(&self, name: &str) -> bool {
        self.bitmaps.iter().any(|(n, id)| n == name && id.is_some())
    }

    pub fn max_read(&self) -> u32 {
        let m = self.block.max.min(self.limits.max_read);
        m - m % self.block.min
    }

    fn guard(&self) -> Result<()> {
        if self.broken {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }

    fn next_cookie(&mut self) -> u64 {
        self.cookie = self.cookie.wrapping_add(1);
        self.cookie
    }

    async fn read_header(&mut self, cookie: u64) -> Result<Header> {
        match read_u32(&mut self.stream).await? {
            SIMPLE_REPLY_MAGIC => {
                let errno = read_u32(&mut self.stream).await?;
                if read_u64(&mut self.stream).await? != cookie {
                    return protocol("a reply to a request that is not in flight");
                }
                Ok(Header::Simple { errno })
            }
            STRUCTURED_REPLY_MAGIC => {
                let flags = read_u16(&mut self.stream).await?;
                let ty = read_u16(&mut self.stream).await?;
                if read_u64(&mut self.stream).await? != cookie {
                    return protocol("a reply to a request that is not in flight");
                }
                let len = read_u32(&mut self.stream).await?;
                Ok(Header::Chunk { flags, ty, len })
            }
            _ => protocol("bad reply magic"),
        }
    }

    async fn read_error_chunk(&mut self, ty: u16, len: u32) -> Result<Error> {
        let header = if ty == REPLY_ERROR_OFFSET { 14 } else { 6 };
        let max = self.limits.max_error_message + 14;
        let len = to_usize(len);
        if len < header || len > max {
            return protocol("an error chunk of an impossible size");
        }
        let payload = read_exact_bounded(&mut self.stream, len as u64, max).await?;
        let mut t = Take(&payload);
        let (Some(errno), Some(mlen)) = (t.u32(), t.u16()) else {
            return protocol("short error chunk");
        };
        let mlen = usize::from(mlen);
        if mlen > len - header {
            return protocol("an error message longer than its chunk");
        }
        let message = t.bytes(mlen).unwrap_or_default();
        Ok(Error::Remote {
            errno,
            message: sanitize(message, self.limits.max_error_message),
        })
    }

    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.write_all(bytes).await?;
        self.stream.flush().await?;
        Ok(())
    }

    pub async fn read(&mut self, offset: u64, len: u32) -> Result<Vec<u8>> {
        self.guard()?;
        if len == 0 || len > self.max_read() {
            return protocol("a read length outside what the server accepts");
        }
        let Some(end) = offset
            .checked_add(u64::from(len))
            .filter(|e| *e <= self.size)
        else {
            return protocol("a read beyond the end of the export");
        };
        let limits = self.limits;
        within(&limits, self.read_inner(offset, len, end)).await
    }

    async fn read_inner(&mut self, offset: u64, len: u32, end: u64) -> Result<Vec<u8>> {
        self.broken = true;
        let cookie = self.next_cookie();
        self.send(&request(CMD_READ, cookie, offset, len)).await?;
        let mut buf = vec![0u8; to_usize(len)];
        let mut covered: Vec<(u64, u64)> = Vec::new();
        let mut total = 0u64;
        loop {
            let (flags, ty, clen) = match self.read_header(cookie).await? {
                Header::Simple { errno: 0 } => {
                    return protocol("a simple reply that reports success");
                }
                Header::Simple { errno } => {
                    self.broken = false;
                    return Err(Error::Remote {
                        errno,
                        message: String::new(),
                    });
                }
                Header::Chunk { flags, ty, len } => (flags, ty, len),
            };
            let done = flags & REPLY_FLAG_DONE != 0;
            let window = (offset, end);
            let max_pieces = self.limits.max_extents;
            match ty {
                REPLY_NONE => {
                    if clen != 0 || !done {
                        return protocol("a malformed terminating chunk");
                    }
                }
                REPLY_OFFSET_DATA => {
                    if clen < 9 {
                        return protocol("a data chunk without data");
                    }
                    let at = read_u64(&mut self.stream).await?;
                    let n = u64::from(clen) - 8;
                    let rel = place(&mut covered, window, at, n, max_pieces)?;
                    let Some(slot) = buf.get_mut(rel..rel + to_usize(clen - 8)) else {
                        return protocol("a data range outside the request");
                    };
                    read_exact(&mut self.stream, slot).await?;
                    total += n;
                }
                REPLY_OFFSET_HOLE => {
                    if clen != 12 {
                        return protocol("a hole chunk of the wrong size");
                    }
                    let at = read_u64(&mut self.stream).await?;
                    let n = u64::from(read_u32(&mut self.stream).await?);
                    if n == 0 {
                        return protocol("an empty hole");
                    }
                    place(&mut covered, window, at, n, max_pieces)?;
                    total += n;
                }
                t if t & REPLY_ERROR_BIT != 0 => {
                    let err = self.read_error_chunk(t, clen).await?;
                    self.broken = !done;
                    return Err(err);
                }
                _ => return protocol("an unknown reply chunk type"),
            }
            if done {
                if total != u64::from(len) {
                    return protocol("the reply does not cover the request");
                }
                self.broken = false;
                return Ok(buf);
            }
        }
    }

    pub async fn block_status(&mut self, offset: u64, len: u32) -> Result<BlockStatus> {
        self.guard()?;
        if len == 0
            || offset
                .checked_add(u64::from(len))
                .is_none_or(|e| e > self.size)
        {
            return protocol("a status request outside the export");
        }
        let limits = self.limits;
        within(&limits, self.status_inner(offset, len)).await
    }

    async fn status_inner(&mut self, offset: u64, len: u32) -> Result<BlockStatus> {
        self.broken = true;
        let cookie = self.next_cookie();
        self.send(&request(CMD_BLOCK_STATUS, cookie, offset, len))
            .await?;
        let negotiated = 1 + self.bitmaps.iter().flat_map(|(_, id)| id).count();
        let mut seen: Vec<(u32, Vec<Descriptor>)> = Vec::new();
        let max_payload = 4 + 8 * self.limits.max_extents;
        loop {
            let (flags, ty, clen) = match self.read_header(cookie).await? {
                Header::Simple { errno: 0 } => {
                    return protocol("a simple reply that reports success");
                }
                Header::Simple { errno } => {
                    self.broken = false;
                    return Err(Error::Remote {
                        errno,
                        message: String::new(),
                    });
                }
                Header::Chunk { flags, ty, len } => (flags, ty, len),
            };
            let done = flags & REPLY_FLAG_DONE != 0;
            match ty {
                REPLY_BLOCK_STATUS => {
                    let n = to_usize(clen);
                    if n < 12 || !(n - 4).is_multiple_of(8) || n > max_payload {
                        return protocol("a status chunk of an impossible size");
                    }
                    let payload =
                        read_exact_bounded(&mut self.stream, u64::from(clen), max_payload).await?;
                    let mut t = Take(&payload);
                    let Some(id) = t.u32() else {
                        return protocol("short status chunk");
                    };
                    let known =
                        id == self.alloc_id || self.bitmaps.iter().any(|(_, b)| *b == Some(id));
                    if !known || seen.iter().any(|(i, _)| *i == id) {
                        return protocol("a status chunk for an unknown or repeated context");
                    }
                    let defined = if id == self.alloc_id {
                        STATE_HOLE | STATE_ZERO
                    } else {
                        STATE_DIRTY
                    };
                    let mut descriptors = Vec::with_capacity((n - 4) / 8);
                    while !t.is_empty() {
                        let (Some(dlen), Some(dflags)) = (t.u32(), t.u32()) else {
                            return protocol("short status descriptor");
                        };
                        if dlen == 0 {
                            return protocol("an empty status descriptor");
                        }
                        if dflags & !defined != 0 {
                            return protocol("a status descriptor with flags its context lacks");
                        }
                        descriptors.push(Descriptor {
                            len: dlen,
                            flags: dflags,
                        });
                    }
                    seen.push((id, descriptors));
                }
                REPLY_NONE => {
                    if clen != 0 || !done {
                        return protocol("a malformed terminating chunk");
                    }
                }
                t if t & REPLY_ERROR_BIT != 0 => {
                    let err = self.read_error_chunk(t, clen).await?;
                    self.broken = !done;
                    return Err(err);
                }
                _ => return protocol("an unexpected chunk in a status reply"),
            }
            if done {
                if seen.len() != negotiated {
                    return protocol("a status reply missing a negotiated context");
                }
                let take = |id: u32| seen.iter().find(|(i, _)| *i == id).map(|(_, d)| d.clone());
                let allocation = take(self.alloc_id).unwrap_or_default();
                let dirty = self.bitmaps.first().and_then(|(_, id)| *id).and_then(take);
                self.broken = false;
                return Ok(BlockStatus { allocation, dirty });
            }
        }
    }

    pub async fn disconnect(mut self) -> Result<()> {
        let limits = self.limits;
        if !self.broken {
            within(&limits, async {
                self.send(&request(CMD_DISC, self.cookie.wrapping_add(1), 0, 0))
                    .await
            })
            .await?;
        }
        within(&limits, async { Ok(self.stream.shutdown().await?) }).await
    }
}

#[cfg(unix)]
impl Client<tokio::net::UnixStream> {
    pub async fn connect(uri: &NbdUri, bitmaps: &[&str], limits: Limits) -> Result<Self> {
        let stream = within(&limits, async {
            Ok(tokio::net::UnixStream::connect(&uri.socket).await?)
        })
        .await?;
        Self::handshake(stream, &uri.export, bitmaps, limits).await
    }
}
