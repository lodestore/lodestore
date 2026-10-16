//! Lodestore repository format v1.

pub mod chunker;
pub mod config;
pub mod crypto;
pub mod error;
pub mod fs;
pub mod index;
pub mod keys;
pub mod pack;
pub mod reader;
pub mod repo;
pub mod selection;
pub mod snapshot;
pub mod store;
pub mod tree;
pub mod tree_cache;
pub mod writer;

#[allow(clippy::all, clippy::pedantic)]
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/lodestore.format.v1.rs"));
}

pub use error::{Error, Result};

pub(crate) type Fut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + 'a>>;
