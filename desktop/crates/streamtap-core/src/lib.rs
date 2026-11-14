//! streamtap-core: what streamtap decides, without doing any I/O.
//!
//! Everything here works on bytes and rows it is handed and returns what it found; it opens no file, no socket and
//! no process, reads no clock and prints nothing (`tests/no_io.rs` holds that). The sink does the I/O around it:
//!
//! - [`wire`]: the headers of a WebSocket message and the lines of `index.jsonl`.
//! - [`fmp4`]: what the bytes are (container, init, media head, encryption) and the boxes the mux reads or rewrites.
//! - [`dedupe`]: has this media segment been written before.
//! - [`parts`]: which `MediaSource` and which part of a `SourceBuffer` is muxed.
//! - [`timeline`]: offsets, the sequence-mode layout, the order of a part, coverage and gaps.

#![allow(
    clippy::must_use_candidate,
    clippy::missing_panics_doc,
    clippy::implicit_hasher,
    reason = "the inner library of one workspace, not a published API: every caller is in this repository"
)]

pub mod dedupe;
pub mod fmp4;
pub mod parts;
pub mod timeline;
pub mod wire;
