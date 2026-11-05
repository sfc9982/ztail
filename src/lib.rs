//! Incremental `tail` for gzip, plain text, and (optionally) zstd files.
//!
//! The original Crystal program re-opened and fully re-decompressed the file on
//! every 500 ms poll. This crate keeps the file descriptor open, remembers the
//! byte offset, and feeds only newly appended bytes into a persistent streaming
//! decoder, so each poll is O(new bytes).

mod app;
mod cli;
mod decode;
mod gzip;
mod lines;
mod tailfile;
mod watch;

pub use app::run;
pub use tailfile::TailFile;
