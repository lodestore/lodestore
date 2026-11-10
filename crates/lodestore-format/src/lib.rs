//! Lodestore repository format v1.
#![forbid(unsafe_code)]

pub mod attest;
pub mod check;
pub mod chunker;
pub mod config;
pub mod crypto;
pub mod error;
pub mod fs;
pub mod gc;
pub mod index;
pub mod keys;
pub mod locks;
pub mod pack;
pub mod provenance;
pub mod reader;
pub mod repo;
pub mod selection;
pub mod snapshot;
pub mod store;
pub mod tree;
pub mod tree_cache;
pub mod verify_snapshot;
pub mod writer;

#[allow(clippy::all, clippy::pedantic)]
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/lodestore.format.v1.rs"));
}

pub use error::{Error, Result};

pub(crate) type Fut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + 'a>>;
