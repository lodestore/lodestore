use crate::store::BlobError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] BlobError),
    #[error("cryptographic operation failed")]
    Crypto,
    #[error("wrong password or recovery key")]
    WrongSecret,
    #[error(
        "wrong password or recovery key, or the key slot it opens was not tried: {untried} more key slots ask for more key-derivation work than one opening may spend ({budget}); if the secret is right, remove the key slots that are not needed"
    )]
    KdfBudget { untried: usize, budget: u64 },
    #[error("corrupt {object}: {reason}")]
    Corrupt { object: String, reason: String },
    #[error(
        "unsupported {what}: the repository requires version {required}, this build supports up to {supported}"
    )]
    UnsupportedVersion {
        what: &'static str,
        required: u32,
        supported: u32,
    },
    #[error("repository is already initialized")]
    AlreadyInitialized,
    #[error("repository is not initialized")]
    NotInitialized,
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("interrupted")]
    Interrupted,
    #[error("the backup ran longer than its time limit of {limit_secs}s; no snapshot was written")]
    DeadlineExceeded { limit_secs: u64 },
    #[error("the snapshot's directory tree has more than {limit} {what}; the walk was stopped")]
    WalkLimit { what: &'static str, limit: u64 },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn corrupt(object: impl Into<String>, reason: impl Into<String>) -> Self {
        Error::Corrupt {
            object: object.into(),
            reason: reason.into(),
        }
    }
}

/// Refuses an object the listing shows larger than a `what` can be, so that
/// it is never downloaded.
pub(crate) fn within_size(key: &str, size: u64, max: u64, what: &str) -> Result<()> {
    if size > max {
        return Err(Error::corrupt(
            key,
            format!("{size} bytes, more than a {what} can be ({max})"),
        ));
    }
    Ok(())
}

impl From<rustix::io::Errno> for Error {
    fn from(e: rustix::io::Errno) -> Self {
        Error::Io(e.into())
    }
}
