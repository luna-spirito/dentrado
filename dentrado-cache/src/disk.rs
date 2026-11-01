//! The disk layer — what the model's `Op` instruction layer was
//! (open/read/write/sync). The [`Storage`] trait is abstract over the
//! file: [`MemDisk`] is memory with fault injection (crash tests),
//! [`FsDisk`] is a real file (compio positional io over io_uring).
//!
//! Reads and writes work over the caller's buffer (the compio traits
//! [`IoBuf`]/[`IoBufMut`]): a page goes to the OS whole, the bytes live
//! with their owner (a pool arena slot — see [`crate::blockcache`]), the
//! transport makes no copies.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};

use compio::buf::{IoBuf, IoBufMut};

use crate::page::{PAGE_SIZE, Page, PageId, TORN_BYTE};

/// A compio file — `!Send`/`!Sync` by construction (`SharedFd = Rc`,
/// thread-per-core): the store and its tasks live on one runtime
/// thread, like the model's scheduler. `'static` — the pool's flights
/// outlive operations and live in the driver's detached task.
pub trait IsFilesystem: 'static {
    /// The positional-io handle — cloneable by nature (it carries no
    /// state): the pool's flights take a copy whole — the future owns
    /// the file, borrowing it is not an option (a flight lives in the
    /// driver longer than any of the store's sections).
    type File: Clone;
    /// Operation futures — nameable: the flight registry keeps them
    /// monomorphically, with no `dyn`.
    type Read<B: IoBufMut>: Future<Output = (io::Result<()>, B)>;
    type Write<B: IoBuf>: Future<Output = (io::Result<()>, B)>;
    type Sync: Future<Output = io::Result<()>>;

    fn open(&self, path: &str) -> impl Future<Output = io::Result<Self::File>>;
    /// Read a page into the given buffer (the page whole). The buffer
    /// always comes back — filled on success, arbitrary on error: an
    /// error is a completed operation, the kernel needs no memory.
    fn read_page<B: IoBufMut>(f: Self::File, id: PageId, buf: B) -> Self::Read<B>;
    /// Write the buffer's bytes as a whole page. The buffer always comes
    /// back: an error is a completed operation, untouched by the write
    /// contract.
    fn write_page<B: IoBuf>(f: Self::File, id: PageId, buf: B) -> Self::Write<B>;
    fn sync(f: Self::File) -> Self::Sync;
}

fn eof(id: PageId) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, format!("eof: page {id}"))
}

// ## MemDisk — memory with fault injection

/// A disk fault: `At(n)` — from the n-th operation on (after arming)
/// everything fails (sticky, ≈ a process crash at the model's tick n);
/// `Once(n)` — the n-th operation alone fails, the disk heals right
/// after (a transient EIO/ENOSPC — the store must survive it live);
/// `Torn(n)` — a crash at tick n whose write is in flight: the write
/// lands torn (the first half of the page new, the rest the old
/// bytes) and reports success, everything from the n-th operation on
/// fails; `Dead` — dead from birth.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    None,
    At(u64),
    Once(u64),
    Torn(u64),
    Dead,
}

struct MemState {
    pages: Vec<Option<Page>>,
    uncommitted: Vec<PageId>,
    ops: u64,
    fault: Fault,
}

impl Default for MemState {
    fn default() -> Self {
        Self {
            pages: vec![],
            uncommitted: vec![],
            ops: 0,
            fault: Fault::None,
        }
    }
}

/// An in-memory file — shared state by path.
#[derive(Clone, Default)]
pub struct MemFile {
    st: Arc<Mutex<MemState>>,
}

impl MemFile {
    /// Arm a fault: the operation counter resets, from the n-th on
    /// everything fails.
    pub fn set_fault(&self, f: Fault) {
        let mut st = self.st.lock().unwrap();
        st.ops = 0;
        st.fault = f;
    }

    /// Crash tearing: the uncommitted pages matched by the predicate
    /// turn into `TORN_BYTE` noise (same length); the fault resets.
    pub fn torn_by(&self, torn: impl Fn(PageId) -> bool + Send + Sync + 'static) {
        let mut st = self.st.lock().unwrap();
        let ids = std::mem::take(&mut st.uncommitted);
        for id in ids {
            if torn(id) {
                if let Some(slot) = st.pages.get_mut(id as usize) {
                    *slot = Some(Page([TORN_BYTE; PAGE_SIZE]));
                }
            }
        }
        st.fault = Fault::None;
    }

    /// The page by number (test inspection).
    pub fn page(&self, id: PageId) -> Option<Page> {
        self.st
            .lock()
            .unwrap()
            .pages
            .get(id as usize)
            .cloned()
            .flatten()
    }

    /// The file's page count.
    pub fn len(&self) -> usize {
        self.st.lock().unwrap().pages.len()
    }

    /// The gate: `Ok(torn)` — the op passes (a write that must land
    /// torn reports so), `Err` — the op fails.
    fn barrier(st: &mut MemState, write: bool) -> io::Result<bool> {
        st.ops += 1;
        match st.fault {
            Fault::None => Ok(false),
            Fault::Dead => Err(io::Error::other("fault")),
            Fault::At(n) if st.ops >= n => Err(io::Error::other("fault")),
            Fault::Once(n) if st.ops == n => Err(io::Error::other("fault")),
            Fault::Torn(n) if st.ops > n => Err(io::Error::other("fault")),
            // the crash moment itself: a write lands torn, a read dies
            Fault::Torn(n) if st.ops == n && write => Ok(true),
            Fault::Torn(n) if st.ops == n => Err(io::Error::other("fault")),
            _ => Ok(false),
        }
    }
}

/// An in-memory disk: files by path (a reopen sees what was written).
#[derive(Clone, Default)]
pub struct MemDisk {
    files: Arc<Mutex<HashMap<String, MemFile>>>,
}

impl MemDisk {
    /// A file's handle by path — created on first touch.
    pub fn file(&self, path: &str) -> MemFile {
        let mut fs = self.files.lock().unwrap();
        fs.entry(path.to_string()).or_default().clone()
    }
}

impl IsFilesystem for MemDisk {
    type File = MemFile;
    type Read<B: IoBufMut> = impl Future<Output = (io::Result<()>, B)>;
    type Write<B: IoBuf> = impl Future<Output = (io::Result<()>, B)>;
    type Sync = impl Future<Output = io::Result<()>>;

    async fn open(&self, path: &str) -> io::Result<MemFile> {
        Ok(self.file(path))
    }

    fn read_page<B: IoBufMut>(f: MemFile, id: PageId, mut buf: B) -> Self::Read<B> {
        async move {
            let mut st = f.st.lock().unwrap();
            if let Err(e) = MemFile::barrier(&mut st, false) {
                return (Err(e), buf);
            }
            match st.pages.get(id as usize) {
                Some(Some(p)) => {
                    let dst = buf.ensure_init();
                    let n = dst.len().min(PAGE_SIZE);
                    dst[..n].copy_from_slice(&p.0[..n]);
                    (Ok(()), buf)
                }
                _ => (Err(eof(id)), buf),
            }
        }
    }

    fn write_page<B: IoBuf>(f: MemFile, id: PageId, buf: B) -> Self::Write<B> {
        async move {
            let mut st = f.st.lock().unwrap();
            let r = MemFile::barrier(&mut st, true).and_then(|torn| {
                let src = buf.as_init();
                let mut b = [0u8; PAGE_SIZE];
                let n = src.len().min(PAGE_SIZE);
                b[..n].copy_from_slice(&src[..n]);
                if id as usize >= st.pages.len() {
                    st.pages.resize(id as usize + 1, None);
                }
                if torn {
                    let half = PAGE_SIZE / 2;
                    match st.pages.get(id as usize) {
                        Some(Some(old)) => b[half..].copy_from_slice(&old.0[half..]),
                        _ => b[half..].fill(TORN_BYTE),
                    }
                }
                st.pages[id as usize] = Some(Page(b));
                st.uncommitted.push(id);
                Ok(())
            });
            (r, buf)
        }
    }

    fn sync(f: MemFile) -> Self::Sync {
        async move {
            let mut st = f.st.lock().unwrap();
            MemFile::barrier(&mut st, false)?;
            st.uncommitted.clear();
            Ok(())
        }
    }
}

// ## FsDisk — a real file

/// An OS file: compio positional io (`io_uring`; the buffer belongs to
/// the operation until it completes — hence the `!Send` op futures).
#[derive(Clone, Copy, Default)]
pub struct FsDisk;

impl IsFilesystem for FsDisk {
    type File = compio::fs::File;
    type Read<B: IoBufMut> = impl Future<Output = (io::Result<()>, B)>;
    type Write<B: IoBuf> = impl Future<Output = (io::Result<()>, B)>;
    type Sync = impl Future<Output = io::Result<()>>;

    async fn open(&self, path: &str) -> io::Result<compio::fs::File> {
        compio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .await
    }

    fn read_page<B: IoBufMut>(f: compio::fs::File, id: PageId, buf: B) -> Self::Read<B> {
        async move {
            use compio::io::AsyncReadAtExt;
            let compio::buf::BufResult(r, b) = f.read_exact_at(buf, id * PAGE_SIZE as u64).await;
            (r, b)
        }
    }

    fn write_page<B: IoBuf>(f: compio::fs::File, id: PageId, buf: B) -> Self::Write<B> {
        async move {
            use compio::io::AsyncWriteAtExt;
            let mut f = f;
            let compio::buf::BufResult(r, b) = f.write_all_at(buf, id * PAGE_SIZE as u64).await;
            (r, b)
        }
    }

    fn sync(f: compio::fs::File) -> Self::Sync {
        async move { f.sync_all().await }
    }
}
