use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NbdUri {
    pub socket: PathBuf,
    pub export: String,
}
