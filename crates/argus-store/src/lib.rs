//! Recording storage.
//!
//! Chunks are written to local disk using the same key layout they will have
//! in S3, so uploading (phase 2) is a straight copy of closed files:
//!
//! ```text
//! rec/{camera}/{stream}/{yyyy}/{mm}/{dd}/{hh}/{start_ms}.m4s
//! init/{camera}/{stream}/{fingerprint}.mp4
//! ```

pub mod gop_index;
pub mod index;
pub mod keys;
pub mod summaries;
pub mod tracks;
pub mod upload;
pub mod writer;

pub use gop_index::GopEntry;
pub use index::{ChunkRecord, Index, ReplayChunk, SeekTarget, StoredChunk};
pub use upload::{Policy, StreamPolicy, Uploader};
pub use writer::ChunkWriter;
