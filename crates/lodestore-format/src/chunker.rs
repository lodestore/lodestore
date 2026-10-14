//! Content-defined chunking: FastCDC v2020 with a per-repository seed, so cut
//! points cannot be guessed from public data, and keyed BLAKE3 chunk ids.

use std::io::Read;

use fastcdc::v2020::{Normalization, StreamCDC};

use crate::config::{
    CHUNK_AVG_SIZE, CHUNK_MAX_SIZE, CHUNK_MIN_SIZE, CHUNK_NORMALIZATION, CHUNKER_FASTCDC_V2020,
};
use crate::crypto::BlobId;
use crate::error::{Error, Result};
use crate::pb;

/// Chunk or tree id: keyed BLAKE3 over the plaintext.
pub fn blob_id(k_id: &[u8; 32], data: &[u8]) -> BlobId {
    *blake3::keyed_hash(k_id, data).as_bytes()
}

/// The chunker parameters this build implements. A repository declaring
/// anything else cannot be written to safely: different cut points would
/// silently destroy deduplication.
pub fn check_params(params: Option<&pb::ChunkerParams>) -> Result<()> {
    let ok = params.is_some_and(|p| {
        p.algorithm == CHUNKER_FASTCDC_V2020
            && p.min_size == CHUNK_MIN_SIZE
            && p.avg_size == CHUNK_AVG_SIZE
            && p.max_size == CHUNK_MAX_SIZE
            && p.normalization == CHUNK_NORMALIZATION
    });
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidInput(
            "repository uses chunker parameters this build does not support".into(),
        ))
    }
}

pub struct Chunk {
    pub id: BlobId,
    pub data: Vec<u8>,
}

pub struct Chunker<R: Read> {
    inner: StreamCDC<R>,
    k_id: [u8; 32],
}

impl<R: Read> Chunker<R> {
    pub fn new(reader: R, k_id: &[u8; 32], seed: u64) -> Self {
        Chunker {
            inner: StreamCDC::with_level_and_seed(
                reader,
                CHUNK_MIN_SIZE as usize,
                CHUNK_AVG_SIZE as usize,
                CHUNK_MAX_SIZE as usize,
                Normalization::Level1,
                seed,
            ),
            k_id: *k_id,
        }
    }
}

impl<R: Read> Iterator for Chunker<R> {
    type Item = Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        let chunk = match self.inner.next()? {
            Ok(c) => c,
            Err(fastcdc::v2020::Error::IoError(e)) => return Some(Err(Error::Io(e))),
            Err(e) => return Some(Err(Error::InvalidInput(format!("chunker: {e}")))),
        };
        Some(Ok(Chunk {
            id: blob_id(&self.k_id, &chunk.data),
            data: chunk.data,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    fn cuts(data: &[u8], k: &[u8; 32], seed: u64) -> Vec<usize> {
        Chunker::new(Cursor::new(data), k, seed)
            .map(|c| c.unwrap().data.len())
            .collect()
    }

    #[test]
    fn chunker_is_deterministic_bounded_and_seeded() {
        let data = noise(6 << 20);
        let k = [5u8; 32];
        let a = cuts(&data, &k, 1);
        assert_eq!(a, cuts(&data, &k, 1));
        assert_ne!(a, cuts(&data, &k, 3));
        assert_eq!(a.iter().sum::<usize>(), data.len());
        let (last, rest) = a.split_last().unwrap();
        assert!(
            rest.iter()
                .all(|&n| (CHUNK_MIN_SIZE as usize..=CHUNK_MAX_SIZE as usize).contains(&n))
        );
        assert!(*last <= CHUNK_MAX_SIZE as usize);

        assert!(cuts(&[], &k, 1).is_empty());
        assert_eq!(cuts(&[1, 2, 3], &k, 1), vec![3]);
    }

    #[test]
    fn chunk_ids_are_keyed() {
        assert_ne!(blob_id(&[1; 32], b"x"), blob_id(&[2; 32], b"x"));
        assert_eq!(blob_id(&[1; 32], b"x"), blob_id(&[1; 32], b"x"));
    }
}
