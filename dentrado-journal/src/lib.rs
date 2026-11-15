//! A deliberately dumb, append-only log.
//!
//! The journal knows nothing about events, users or groups: it stores schema
//! **declarations** and records of `schema_id | payload | crc32`, in order,
//! forever. Whoever writes the payloads gives them meaning; the journal only
//! guarantees that a byte it accepted once is still there, unchanged, in the
//! same place.
//!
//! ```
//! # use std::ops::ControlFlow;
//! # use dentrado_journal::{Cursor, Decl, Frame, Journal};
//! # fn run<F: Future>(f: F) -> F::Output {
//! #     compio::runtime::Runtime::new().unwrap().block_on(f)
//! # }
//! # run(async {
//! # let dir = tempfile::tempdir().unwrap();
//! let mut log = Journal::open(dir.path()).await.unwrap();
//! let schema = log
//!     .intern(Decl { name: "example/counter", shape: "u64" })
//!     .await
//!     .unwrap();
//! let head = log.append(&[Frame::new(schema, &7u64.to_le_bytes())]).await.unwrap();
//! log.sync().await.unwrap();
//!
//! let mut seen = Vec::new();
//! let resume = log
//!     .replay_from(Cursor::START, async |_, frame: Frame<'_>| {
//!         seen.push(frame.payload.to_vec());
//!         ControlFlow::Continue(())
//!     })
//!     .await
//!     .unwrap();
//! # assert_eq!(seen.len(), 2); // the declaration, then the record
//! # assert_eq!(resume, head);
//! # });
//! ```
//!
//! # What is durable, and what is not
//!
//! [`Journal::sync`] is `fdatasync`: after it returns, the bytes are on the
//! drive as far as the drive and the filesystem are willing to say so. Nothing
//! below the head is ever written again — no record is updated in place, no
//! page is rewritten — so an append can only *extend* the log. That is what
//! makes a torn write harmless: a partially applied extend leaves either a
//! prefix of the new bytes or garbage past the old head, and opening the log
//! truncates exactly that. Bytes that were already durable are not in the
//! written range at all, so no partial write can reach them. (A block shared
//! with the tail is rewritten with its own old bytes as a prefix, so even a
//! torn *block* write leaves that prefix intact.)
//!
//! This is not immunity to the hardware:
//!
//! - **Bit rot and media errors** happen, and nothing a log does prevents them.
//!   They are *detected*, never repaired — repair takes redundancy, which is
//!   not in this format. Every frame carries a crc32 and every closed segment
//!   carries a blake3 digest over all of its frames, verified on every full
//!   replay and by [`Journal::verify`]. But a checksum alone cannot tell a
//!   rotted tail from an append that was interrupted, and getting that wrong is
//!   exactly how a log loses data it promised: so [`Journal::sync`] also writes
//!   down *how far* it got (a two-slot, crc'd `acked` file, fsynced after the
//!   segments). Opening then knows the difference — damage at or below the
//!   acked head is rot and refuses to open, damage past it is an unfinished
//!   append and is truncated away.
//! - **Losing that file** costs only the distinction: the log falls back to
//!   reading its tail as an interrupted append, exactly as it did before.
//! - **A drive that lies about a flush** loses the most recent appends, and the
//!   log then ends earlier than the writer believed. That is why an
//!   acknowledgement waits for [`Journal::sync`]: the acked file can lag the
//!   data, never lead it.
//! - **A second writer** is refused at open by the directory's lock. **An
//!   adversary** with write access to the files is out of scope.
//!
//! There is no read-only mode: `open` needs write access and takes the lock,
//! because one handle serves both replay and append, and a replay is what may
//! have to truncate a torn tail. A read-only replica would need a second entry
//! point, and there is no consumer for one yet.
//!
//! When redundancy arrives it is replication, not local doubling: segments are
//! immutable and self-describing, so a peer holding the same segment can be
//! asked for the frames a crc or a digest disagrees on. Duplicating writes
//! locally would cost 2× for strictly less protection than one off-site copy.
//!
//! # Format
//!
//! A directory of `<segment:020>.jrnl` files with dense ids from zero. Segment
//! zero is the declaration segment: it never rotates and never closes. Data
//! segments rotate at [`Options::rotate_bytes`].
//!
//! ```text
//! header (32 B):  "DJNL" | log_version u16 | header_len u16 | segment u64 | [u8; 16]
//! frame:          payload_len u32 | crc32 u32 | schema_id u16 | payload
//! footer (44 B):  "DJNF" | count u64 | digest [u8; 32]
//! ```
//!
//! The frame `crc32` covers `schema_id | payload` and catches damage inside a
//! record; the footer digest (blake3 over every frame byte) is what notices a
//! *missing* record — a crc cannot, because a truncated prefix of a segment
//! has only valid crcs. Neither defends against an adversary.
//!
//! Beside the segments sits `acked`: two 40-byte slots, `seq | decl_end |
//! segment | offset | crc32`, written in alternation so that a torn write of it
//! costs a slot rather than the record.
//!
//! # Recovery
//!
//! Opening is a pure function of the files: segment zero is scanned for
//! declarations, the last segment's tail is truncated at its first bad frame
//! unless that frame is at or below the acked head, and every other segment is
//! only checked for a header and a footer (frames are verified as they are
//! replayed). Interior damage is fatal, and so is a header that does not decode
//! in a segment the log acked: recreating it would throw away promised records.

#![deny(unsafe_code)]
#![warn(clippy::pedantic)]

pub mod error;
pub mod cursor;
pub mod schema;
pub mod frame;
pub mod segment;
mod acked;
pub mod log;

pub use cursor::Cursor;
pub use error::{Conflict, Error, ReplayError};
pub use frame::Frame;
pub use log::{Journal, Options};
pub use schema::{Decl, Registry, SchemaId};
