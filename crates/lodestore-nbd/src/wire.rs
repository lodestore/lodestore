use std::future::Future;
use std::io::ErrorKind;

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{Error, Limits, Result};

pub const NBDMAGIC: u64 = 0x4e42_444d_4147_4943;
pub const IHAVEOPT: u64 = 0x4948_4156_454f_5054;
pub const OPT_REPLY_MAGIC: u64 = 0x0003_e889_0455_65a9;
pub const REQUEST_MAGIC: u32 = 0x2560_9513;
pub const SIMPLE_REPLY_MAGIC: u32 = 0x6744_6698;
pub const STRUCTURED_REPLY_MAGIC: u32 = 0x668e_33ef;

pub const OPT_ABORT: u32 = 2;
pub const OPT_GO: u32 = 7;
pub const OPT_STRUCTURED_REPLY: u32 = 8;
pub const OPT_SET_META_CONTEXT: u32 = 10;

pub const REP_ERROR_BIT: u32 = 1 << 31;
pub const REP_ACK: u32 = 1;
pub const REP_INFO: u32 = 3;
pub const REP_META_CONTEXT: u32 = 4;
pub const REP_ERR_UNSUP: u32 = REP_ERROR_BIT | 1;
pub const REP_ERR_INVALID: u32 = REP_ERROR_BIT | 3;
pub const REP_ERR_UNKNOWN: u32 = REP_ERROR_BIT | 6;

pub const INFO_EXPORT: u16 = 0;
pub const INFO_BLOCK_SIZE: u16 = 3;

pub const CMD_READ: u16 = 0;
pub const CMD_WRITE: u16 = 1;
pub const CMD_DISC: u16 = 2;
pub const CMD_FLUSH: u16 = 3;
pub const CMD_TRIM: u16 = 4;
pub const CMD_CACHE: u16 = 5;
pub const CMD_WRITE_ZEROES: u16 = 6;
pub const CMD_BLOCK_STATUS: u16 = 7;

pub const REPLY_FLAG_DONE: u16 = 1;
pub const REPLY_NONE: u16 = 0;
pub const REPLY_OFFSET_DATA: u16 = 1;
pub const REPLY_OFFSET_HOLE: u16 = 2;
pub const REPLY_BLOCK_STATUS: u16 = 5;
pub const REPLY_ERROR: u16 = 0x8001;
pub const REPLY_ERROR_OFFSET: u16 = 0x8002;
pub const REPLY_ERROR_BIT: u16 = 0x8000;

pub const EPERM: u32 = 1;
pub const EIO: u32 = 5;
pub const EINVAL: u32 = 22;
pub const EOVERFLOW: u32 = 75;

pub const STATE_HOLE: u32 = 1;
pub const STATE_ZERO: u32 = 2;
pub const STATE_DIRTY: u32 = 1;

pub const FLAG_FIXED_NEWSTYLE: u16 = 1;
pub const FLAG_NO_ZEROES: u16 = 2;
pub const FLAG_HAS_FLAGS: u16 = 1;
pub const FLAG_READ_ONLY: u16 = 2;

pub const CONTEXT_ALLOCATION: &str = "base:allocation";
pub const BITMAP_CONTEXT_PREFIX: &str = "qemu:dirty-bitmap:";

pub async fn within<T>(limits: &Limits, fut: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(limits.op_timeout, fut)
        .await
        .map_err(|_| Error::Timeout)?
}

pub async fn read_exact<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut [u8]) -> Result<()> {
    match stream.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Err(Error::Closed),
        Err(e) => Err(e.into()),
    }
}

pub async fn read_u16<S: AsyncRead + Unpin>(stream: &mut S) -> Result<u16> {
    let mut b = [0; 2];
    read_exact(stream, &mut b).await?;
    Ok(u16::from_be_bytes(b))
}

pub async fn read_u32<S: AsyncRead + Unpin>(stream: &mut S) -> Result<u32> {
    let mut b = [0; 4];
    read_exact(stream, &mut b).await?;
    Ok(u32::from_be_bytes(b))
}

pub async fn read_u64<S: AsyncRead + Unpin>(stream: &mut S) -> Result<u64> {
    let mut b = [0; 8];
    read_exact(stream, &mut b).await?;
    Ok(u64::from_be_bytes(b))
}

pub async fn read_exact_bounded<S: AsyncRead + Unpin>(
    stream: &mut S,
    len: u64,
    limit: usize,
) -> Result<Vec<u8>> {
    let len = usize::try_from(len)
        .ok()
        .filter(|l| *l <= limit)
        .ok_or_else(|| {
            Error::Protocol(format!(
                "a field of {len} bytes exceeds the limit of {limit}"
            ))
        })?;
    let mut buf = vec![0; len];
    read_exact(stream, &mut buf).await?;
    Ok(buf)
}

pub async fn discard<S: AsyncRead + Unpin>(stream: &mut S, mut len: u64) -> Result<()> {
    let mut buf = [0u8; 8192];
    while len > 0 {
        let n = len.min(buf.len() as u64) as usize;
        let Some(part) = buf.get_mut(..n) else {
            return Err(Error::Protocol("discard window".into()));
        };
        read_exact(stream, part).await?;
        len -= n as u64;
    }
    Ok(())
}

pub struct Take<'a>(pub &'a [u8]);

impl<'a> Take<'a> {
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.bytes(2)?.try_into().ok()?))
    }

    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.bytes(4)?.try_into().ok()?))
    }

    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.bytes(8)?.try_into().ok()?))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub fn to_usize(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

pub fn option_request(option: u32, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + data.len());
    v.extend_from_slice(&IHAVEOPT.to_be_bytes());
    v.extend_from_slice(&option.to_be_bytes());
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    v
}

pub fn option_reply(option: u32, reply: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(20 + payload.len());
    v.extend_from_slice(&OPT_REPLY_MAGIC.to_be_bytes());
    v.extend_from_slice(&option.to_be_bytes());
    v.extend_from_slice(&reply.to_be_bytes());
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn request(ty: u16, cookie: u64, offset: u64, length: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(28);
    v.extend_from_slice(&REQUEST_MAGIC.to_be_bytes());
    v.extend_from_slice(&0u16.to_be_bytes());
    v.extend_from_slice(&ty.to_be_bytes());
    v.extend_from_slice(&cookie.to_be_bytes());
    v.extend_from_slice(&offset.to_be_bytes());
    v.extend_from_slice(&length.to_be_bytes());
    v
}

pub fn structured_chunk(flags: u16, ty: u16, cookie: u64, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(20 + payload.len());
    v.extend_from_slice(&STRUCTURED_REPLY_MAGIC.to_be_bytes());
    v.extend_from_slice(&flags.to_be_bytes());
    v.extend_from_slice(&ty.to_be_bytes());
    v.extend_from_slice(&cookie.to_be_bytes());
    v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    v.extend_from_slice(payload);
    v
}
