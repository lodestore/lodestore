//! Seed inputs for the fuzz corpora, cut from the golden fixtures. Fixture
//! files are only read.

use std::fs;
use std::path::Path;

use lodestore_format::pack::{ENTRY_LEN, PACK_MAGIC, PACK_PREFIX_LEN, read_pack_header};
use lodestore_format::repo::Repository;
use lodestore_format::store::LocalFsStore;

type Seeds = Vec<(&'static str, Vec<u8>)>;

const TRAILER_LEN: usize = 4 + PACK_MAGIC.len();

pub fn seeds(fixtures: &Path) -> Seeds {
    try_seeds(fixtures).unwrap_or_else(|e| panic!("seed inputs: {e}"))
}

pub fn try_seeds(fixtures: &Path) -> Result<Seeds, String> {
    let io = |what: &str, e: std::io::Error| format!("{what}: {e}");
    let password = fs::read_to_string(fixtures.join("PASSWORD")).map_err(|e| io("PASSWORD", e))?;
    let repo_dir = fixtures.join("repo");
    let store = LocalFsStore::new(repo_dir.clone());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| io("runtime", e))?;
    let repo = rt
        .block_on(Repository::open(&store, password.trim_end().as_bytes()))
        .map_err(|e| format!("open the fixture repository: {e}"))?;

    let mut out: Seeds = Vec::new();
    for shard in fs::read_dir(repo_dir.join("packs")).map_err(|e| io("packs", e))? {
        let shard = shard.map_err(|e| io("packs", e))?;
        for pack in fs::read_dir(shard.path()).map_err(|e| io("packs", e))? {
            let path = pack.map_err(|e| io("packs", e))?.path();
            let bytes = fs::read(&path).map_err(|e| io("pack", e))?;
            let size = bytes.len();
            let prefix = bytes
                .get(..PACK_PREFIX_LEN)
                .ok_or("pack shorter than a prefix")?;
            let trailer = bytes
                .get(size.saturating_sub(TRAILER_LEN)..)
                .ok_or("pack shorter than a trailer")?;
            out.push(("pack_prefix", prefix.to_vec()));
            out.push((
                "pack_trailer",
                [&(size as u64).to_le_bytes()[..], trailer].concat(),
            ));

            let key = format!(
                "packs/{}/{}",
                shard.file_name().to_string_lossy(),
                path.file_name().unwrap_or_default().to_string_lossy()
            );
            let header = rt
                .block_on(read_pack_header(
                    &store,
                    &key,
                    size as u64,
                    repo.read_keys(),
                ))
                .map_err(|e| format!("read the header of {key}: {e}"))?;
            let mut records = Vec::with_capacity(header.entries.len() * ENTRY_LEN);
            for e in &header.entries {
                records.extend_from_slice(&e.blob_id);
                records.push(e.blob_type);
                records.extend_from_slice(&e.offset.to_le_bytes());
                records.extend_from_slice(&e.enc_len.to_le_bytes());
                records.extend_from_slice(&e.ulen.to_le_bytes());
            }
            out.push(("pack_entries", records));
        }
    }
    Ok(out)
}
