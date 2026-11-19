use lodestore_format::image::{ImageExtent, ImageSource};
use lodestore_format::{Error as FormatError, Result as FormatResult};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::wire::{STATE_DIRTY, STATE_ZERO};
use crate::{Client, Descriptor, Error};

pub const MAX_PIECES: usize = 65_536;
pub const MAX_STATUS_STEPS: usize = 1024;

pub struct NbdSource<S> {
    client: Client<S>,
}

fn fail(what: &str) -> FormatError {
    FormatError::Io(std::io::Error::other(what.to_owned()))
}

fn io(e: Error) -> FormatError {
    FormatError::Io(std::io::Error::other(e))
}

fn covered(descriptors: &[Descriptor]) -> u64 {
    descriptors.iter().map(|d| u64::from(d.len)).sum()
}

struct Cursor<'a> {
    descriptors: &'a [Descriptor],
    index: usize,
    left: u64,
}

impl<'a> Cursor<'a> {
    fn new(descriptors: &'a [Descriptor]) -> Self {
        let left = descriptors.first().map_or(0, |d| u64::from(d.len));
        Self {
            descriptors,
            index: 0,
            left,
        }
    }

    fn flags(&self) -> u32 {
        self.descriptors.get(self.index).map_or(0, |d| d.flags)
    }

    fn advance(&mut self, n: u64) {
        self.left -= n;
        if self.left == 0 {
            self.index += 1;
            self.left = self
                .descriptors
                .get(self.index)
                .map_or(0, |d| u64::from(d.len));
        }
    }
}

fn push(pieces: &mut Vec<ImageExtent>, len: u64, zero: bool, dirty: bool) -> FormatResult<()> {
    if let Some(last) = pieces
        .last_mut()
        .filter(|l| l.zero == zero && l.dirty == dirty)
    {
        last.len += len;
    } else if pieces.len() >= MAX_PIECES {
        return Err(fail("the NBD export has too many pieces"));
    } else {
        pieces.push(ImageExtent { len, zero, dirty });
    }
    Ok(())
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> NbdSource<S> {
    pub fn new(client: Client<S>) -> Self {
        Self { client }
    }

    pub fn into_client(self) -> Client<S> {
        self.client
    }

    pub fn has_context(&self, name: &str) -> bool {
        self.client.has_context(name)
    }

    fn window(&self, offset: u64, len: u64) -> FormatResult<(u64, u64)> {
        let end = offset
            .checked_add(len)
            .filter(|e| *e <= self.client.size())
            .ok_or_else(|| fail("a range beyond the end of the NBD export"))?;
        let min = u64::from(self.client.block_size().min);
        let start = offset - offset % min;
        let end = end.next_multiple_of(min).min(self.client.size());
        Ok((start, end))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> ImageSource for NbdSource<S> {
    fn size(&self) -> u64 {
        self.client.size()
    }

    fn has_dirty(&self) -> bool {
        self.client.has_dirty()
    }

    async fn extents(&mut self, offset: u64, len: u64) -> FormatResult<Vec<ImageExtent>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let (start, end) = self.window(offset, len)?;
        let min = u64::from(self.client.block_size().min);
        let max = u64::from(self.client.max_read());
        let mut pieces: Vec<ImageExtent> = Vec::new();
        let mut pos = start;
        let mut steps = 0;
        while pos < end {
            steps += 1;
            if steps > MAX_STATUS_STEPS {
                return Err(fail("the NBD server made too little progress"));
            }
            let ask = (end - pos).min(max);
            let status = self
                .client
                .block_status(
                    pos,
                    u32::try_from(ask).map_err(|_| fail("the NBD request length"))?,
                )
                .await
                .map_err(io)?;
            let mut step = ask.min(covered(&status.allocation));
            if let Some(dirty) = &status.dirty {
                step = step.min(covered(dirty));
            }
            step -= step % min;
            if step == 0 {
                return Err(fail("the NBD server made no progress"));
            }
            let mut allocation = Cursor::new(&status.allocation);
            let mut dirty = status.dirty.as_deref().map(Cursor::new);
            let mut left = step;
            while left > 0 {
                let mut n = left.min(allocation.left);
                if let Some(d) = &dirty {
                    n = n.min(d.left);
                }
                let zero = allocation.flags() & STATE_ZERO != 0;
                let is_dirty = dirty.as_ref().is_some_and(|d| d.flags() & STATE_DIRTY != 0);
                push(&mut pieces, n, zero, is_dirty)?;
                allocation.advance(n);
                if let Some(d) = &mut dirty {
                    d.advance(n);
                }
                left -= n;
            }
            pos += step;
        }
        let mut skip = offset - start;
        let mut keep = len;
        let mut out = Vec::with_capacity(pieces.len());
        for p in pieces {
            let cut = skip.min(p.len);
            skip -= cut;
            let take = (p.len - cut).min(keep);
            keep -= take;
            if take > 0 {
                out.push(ImageExtent { len: take, ..p });
            }
        }
        Ok(out)
    }

    async fn read(&mut self, offset: u64, len: usize) -> FormatResult<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let (start, end) = self.window(offset, len as u64)?;
        let max = u64::from(self.client.max_read());
        let mut buf = Vec::with_capacity(
            usize::try_from(end - start).map_err(|_| fail("the NBD read length"))?,
        );
        let mut pos = start;
        while pos < end {
            let n = (end - pos).min(max);
            let part = self
                .client
                .read(
                    pos,
                    u32::try_from(n).map_err(|_| fail("the NBD read length"))?,
                )
                .await
                .map_err(io)?;
            buf.extend_from_slice(&part);
            pos += n;
        }
        let head = usize::try_from(offset - start).map_err(|_| fail("the NBD read offset"))?;
        buf.drain(..head);
        buf.truncate(len);
        Ok(buf)
    }
}
