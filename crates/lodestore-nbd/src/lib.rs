#![forbid(unsafe_code)]

pub mod client;
pub mod limits;
pub mod server;
pub mod source;
pub mod uri;
pub mod wire;

pub use client::{BlockSize, BlockStatus, Client, Descriptor};
pub use limits::Limits;
pub use server::{Export, MemExport, serve};
pub use source::NbdSource;
pub use uri::{NbdUri, is_nbd_uri};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("NBD I/O error: {0}")]
    Io(std::io::Error),
    #[error("NBD protocol violation: {0}")]
    Protocol(String),
    #[error("the NBD server refused option {option} (reply {reply:#x})")]
    Refused { option: u32, reply: u32 },
    #[error("the NBD server reported error {errno}: {message}")]
    Remote { errno: u32, message: String },
    #[error("the NBD peer did not answer in time")]
    Timeout,
    #[error("the NBD connection is closed")]
    Closed,
    #[error("invalid NBD URI: {0}")]
    Uri(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
        match e.kind() {
            UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted => Error::Closed,
            _ => Error::Io(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
