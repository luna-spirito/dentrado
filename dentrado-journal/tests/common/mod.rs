//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use dentrado_journal::{Cursor, Decl, Frame, Journal, SchemaId};

pub fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().unwrap().block_on(f)
}

pub const COUNTER: &str = "example/counter";
pub const COUNTER_SHAPE: &str = "{at:u64}";

pub async fn counter(log: &mut Journal) -> SchemaId {
    log.intern(Decl {
        name: COUNTER,
        shape: COUNTER_SHAPE,
    })
    .await
    .unwrap()
}

/// Every record from `from` on, declarations included.
pub async fn records(log: &Journal, from: Cursor) -> Vec<(Cursor, SchemaId, Vec<u8>)> {
    let mut out = Vec::new();
    log.replay_from(from, async |at, frame: Frame<'_>| {
        out.push((at, frame.schema, frame.payload.to_vec()));
        ControlFlow::Continue(())
    })
    .await
    .unwrap();
    out
}

/// Only the consumer's records, with their payloads.
pub async fn payloads(log: &Journal) -> Vec<Vec<u8>> {
    records(log, Cursor::START)
        .await
        .into_iter()
        .filter(|(_, schema, _)| schema.is_user())
        .map(|(_, _, payload)| payload)
        .collect()
}

pub fn segments(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jrnl"))
        .collect();
    files.sort();
    files
}

pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

pub fn flip(path: &Path, offset: u64) {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut byte = [0u8; 1];
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0xff;
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&byte).unwrap();
}

pub fn truncate(path: &Path, len: u64) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(len)
        .unwrap();
}
