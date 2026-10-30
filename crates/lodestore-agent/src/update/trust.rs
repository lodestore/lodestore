use std::path::Path;

use crate::Paths;
use crate::hooks::unsafe_reason;
use crate::update::minisign::PublicKey;

pub const PROJECT_KEY: &str = include_str!("../../keys/project.pub");

const MAX_KEY_FILE: u64 = 4096;

pub fn trusted_keys(paths: &Paths) -> Vec<PublicKey> {
    let mut keys = Vec::new();
    match PublicKey::parse(PROJECT_KEY) {
        Ok(key) => keys.push(key),
        Err(e) => tracing::error!("embedded project key is unusable: {e}"),
    }
    keys.extend(host_keys(&paths.trusted_keys()));
    keys
}

fn host_keys(dir: &Path) -> Vec<PublicKey> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!("ignoring {}: {e}", dir.display());
            return Vec::new();
        }
    };
    if !meta.is_dir() {
        tracing::warn!("ignoring {}: not a directory", dir.display());
        return Vec::new();
    }
    if let Some(reason) = unsafe_reason(&meta) {
        tracing::warn!("ignoring {}: {reason}", dir.display());
        return Vec::new();
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!("ignoring {}: {e}", dir.display());
            return Vec::new();
        }
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "pub"))
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|path| match load_key(path) {
            Ok(key) => Some(key),
            Err(reason) => {
                tracing::warn!("ignoring {}: {reason}", path.display());
                None
            }
        })
        .collect()
}

fn load_key(path: &Path) -> Result<PublicKey, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    if let Some(reason) = unsafe_reason(&meta) {
        return Err(reason.into());
    }
    if meta.len() > MAX_KEY_FILE {
        return Err("file is too large".into());
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    PublicKey::parse(&text).map_err(|e| e.to_string())
}
