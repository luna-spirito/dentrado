#![allow(clippy::pedantic)]
#![allow(unsafe_code)]
#![allow(dead_code)]

use compio::BufResult;
use libc::O_DIRECT;
use std::{
    alloc::Layout,
    cell::{Cell, RefCell},
    collections::HashMap,
    io,
    mem::MaybeUninit,
    num::NonZero,
    path::Path,
    ptr,
    rc::Rc,
};

use compio::{
    buf::{IoBuf, IoBufMut, SetLen},
    fs::{File as CompioFile, OpenOptions},
    io::{AsyncReadAtExt, AsyncWriteAtExt},
    runtime::spawn,
};
use synchrony::unsync::event::Event;

pub(crate) const PAGE_SIZE: usize = 4096;

#[repr(align(4096))]
pub(crate) struct AlignedPage(pub [u8; PAGE_SIZE]);

impl IoBuf for AlignedPage {
    fn as_init(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PageId(pub u32);

const KEYS_SENTINEL: (FileId, PageId) = (FileId(u32::MAX), PageId(u32::MAX));

pub(crate) struct OwnedPage {
    ptr: &'static mut [MaybeUninit<u8>; PAGE_SIZE],
    len: usize,
}

impl OwnedPage {
    fn new(page: &'static mut [MaybeUninit<u8>; PAGE_SIZE]) -> Self {
        Self { ptr: page, len: 0 }
    }
}

impl IoBuf for OwnedPage {
    fn as_init(&self) -> &[u8] {
        unsafe { self.ptr[..self.len].assume_init_ref() }
    }
}

impl SetLen for OwnedPage {
    unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= PAGE_SIZE);
        self.len = len;
    }
}

impl IoBufMut for OwnedPage {
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        self.ptr
    }
}

struct WriteLock(Event);

impl Drop for WriteLock {
    fn drop(&mut self) {
        self.0.notify_all();
    }
}

pub(crate) struct File {
    id: FileId,
    file: CompioFile,
    page_loaded: Event,
    write_lock: RefCell<Rc<WriteLock>>,
    flush_in_progress: Cell<bool>,
    flush_done: Event,
}

impl File {
    pub(crate) async fn metadata(&self) -> io::Result<compio::fs::Metadata> {
        self.file.metadata().await
    }
}

struct AlignedPageBuf {
    ptr: ptr::NonNull<AlignedPage>,
    capacity: NonZero<u32>,
    taken: u32,
}

impl AlignedPageBuf {
    fn layout(capacity: NonZero<u32>) -> Layout {
        Layout::new::<AlignedPage>()
            .repeat(capacity.get() as usize)
            .unwrap()
            .0
    }

    fn new(capacity: NonZero<u32>) -> Self {
        let ptr = unsafe { std::alloc::alloc(Self::layout(capacity)).cast::<AlignedPage>() };
        Self {
            ptr: ptr::NonNull::new(ptr).unwrap(),
            capacity,
            taken: 0,
        }
    }

    unsafe fn take_mut(&mut self, idx: u32) -> OwnedPage {
        self.taken += 1;
        OwnedPage::new(self.ptr.add(idx as usize).cast().as_mut())
    }

    unsafe fn ret_mut(&mut self, _page: OwnedPage) {
        self.taken -= 1;
    }

    unsafe fn borrow(&self, idx: u32) -> &AlignedPage {
        self.ptr.add(idx as usize).as_ref()
    }

    unsafe fn fill(&mut self, idx: u32, data: &[u8; PAGE_SIZE]) {
        let dst = self.ptr.add(idx as usize);
        ptr::copy_nonoverlapping::<u8>(data.as_ptr(), dst.cast().as_ptr(), PAGE_SIZE);
    }

    unsafe fn overwrite_take(&mut self, idx: u32) -> &'static AlignedPage {
        self.taken += 1;
        self.ptr.add(idx as usize).as_ref()
    }

    unsafe fn ret(&mut self, _page: &'static AlignedPage) {
        self.taken -= 1;
    }
}

impl Drop for AlignedPageBuf {
    fn drop(&mut self) {
        if self.taken != 0 {
            panic!(
                "AlignedPageBuf dropped with {} taken pages still in use by compio",
                self.taken,
            );
        }
        unsafe { std::alloc::dealloc(self.ptr.as_ptr().cast(), Self::layout(self.capacity)) }
    }
}

struct SlotMap {
    forward: HashMap<(FileId, PageId), u32>,
    backward: Box<[(FileId, PageId)]>, // KEYS_SENTINEL as sentinel
}

impl SlotMap {
    fn new(capacity: u32) -> Self {
        Self {
            forward: HashMap::new(),
            backward: vec![KEYS_SENTINEL; capacity as usize].into_boxed_slice(),
        }
    }

    fn get_forward(&self, file: FileId, page: PageId) -> Option<u32> {
        self.forward.get(&(file, page)).copied()
    }

    #[allow(dead_code)]
    fn free_slot(&mut self, slot: u32) {
        let old_key = self.backward[slot as usize];
        if old_key != KEYS_SENTINEL {
            self.forward.remove(&old_key);
        }
        self.backward[slot as usize] = KEYS_SENTINEL;
    }

    fn use_slot_for_new(&mut self, slot: u32, file: FileId, page: PageId) {
        let old_key = self.backward[slot as usize];
        if old_key != KEYS_SENTINEL {
            self.forward.remove(&old_key);
        }
        self.backward[slot as usize] = (file, page);
        let replaced = self.forward.insert((file, page), slot);
        debug_assert_eq!(replaced, None);
    }

    fn expire(&mut self, file: FileId, page: PageId, from_slot: u32, to_slot: u32) {
        self.backward[from_slot as usize] = KEYS_SENTINEL;
        let old_key = self.backward[to_slot as usize];
        if old_key != KEYS_SENTINEL {
            self.forward.remove(&old_key);
        }
        self.backward[to_slot as usize] = (file, page);
        self.forward.insert((file, page), to_slot);
    }

    fn overwritten_with(&self, file: FileId, page: PageId, original_slot: u32) -> Option<u32> {
        match self.forward.get(&(file, page)) {
            Some(&slot) if slot == original_slot => None,
            Some(&slot) => Some(slot),
            None => None,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AllocStatus {
    Free,
    Loading,
    OccupiedRef,
    OccupiedUnref,
    SavingRef,
    SavingUnref,
    QueuedSavingRef,
    QueuedSavingUnref,
}

struct FsInner {
    buf: AlignedPageBuf,
    map: SlotMap,
    alloc: Box<[AllocStatus]>,
    clock_hand: u32,
    next_file_id: u32,
}

impl FsInner {
    fn new(capacity: NonZero<u32>) -> Self {
        let cap = capacity.get();
        Self {
            buf: AlignedPageBuf::new(capacity),
            map: SlotMap::new(cap),
            alloc: vec![AllocStatus::Free; cap as usize].into_boxed_slice(),
            clock_hand: 0,
            next_file_id: 0,
        }
    }

    fn register_file(&mut self) -> FileId {
        let result = FileId(self.next_file_id);
        self.next_file_id = self.next_file_id.checked_add(1).expect("File id overflow");
        result
    }

    fn find_allocatable(&mut self) -> Option<u32> {
        let capacity = self.buf.capacity.get();
        loop {
            let mut demoted = false;

            for _ in 0..capacity {
                use AllocStatus::{
                    Free, Loading, OccupiedRef, OccupiedUnref, QueuedSavingRef, QueuedSavingUnref,
                    SavingRef, SavingUnref,
                };
                let slot = self.clock_hand;
                self.clock_hand = (self.clock_hand + 1) % capacity;

                match self.alloc[slot as usize] {
                    Free | OccupiedUnref => return Some(slot),
                    OccupiedRef => {
                        self.alloc[slot as usize] = OccupiedUnref;
                        demoted = true;
                    }
                    SavingRef => {
                        self.alloc[slot as usize] = SavingUnref;
                        demoted = true;
                    }
                    QueuedSavingRef => {
                        self.alloc[slot as usize] = QueuedSavingUnref;
                        demoted = true;
                    }
                    Loading | SavingUnref | QueuedSavingUnref => {}
                }
            }

            if !demoted {
                return None;
            }
        }
    }
}

pub(crate) struct Fs {
    inner: Rc<RefCell<FsInner>>,
    page_freeable: Event,
}

impl Fs {
    #[must_use]
    pub(crate) fn new(capacity: NonZero<u32>) -> Self {
        Self {
            inner: Rc::new(RefCell::new(FsInner::new(capacity))),
            page_freeable: Event::new(),
        }
    }

    pub(crate) async fn open(&self, path: &Path) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .custom_flags(O_DIRECT)
            .open(path)
            .await?;
        let id = self.inner.borrow_mut().register_file();
        Ok(File {
            id,
            file,
            page_loaded: Event::new(),
            write_lock: RefCell::new(Rc::new(WriteLock(Event::new()))),
            flush_in_progress: Cell::new(false),
            flush_done: Event::new(),
        })
    }

    pub(crate) async fn read<T>(
        &self,
        file: &File,
        page: PageId,
        f: impl Fn(&AlignedPage) -> T,
    ) -> T {
        use AllocStatus::{
            Free, Loading, OccupiedRef, OccupiedUnref, QueuedSavingRef, QueuedSavingUnref,
            SavingRef, SavingUnref,
        };

        loop {
            enum Decision {
                Found(u32),
                WaitLoaded,
                Load,
            }

            let decision = {
                let mut inner = self.inner.borrow_mut();
                match inner.map.get_forward(file.id, page) {
                    Some(slot) => match inner.alloc[slot as usize] {
                        Loading => Decision::WaitLoaded,
                        OccupiedRef | SavingRef | QueuedSavingRef => Decision::Found(slot),
                        OccupiedUnref => {
                            inner.alloc[slot as usize] = OccupiedRef;
                            Decision::Found(slot)
                        }
                        SavingUnref => {
                            inner.alloc[slot as usize] = SavingRef;
                            Decision::Found(slot)
                        }
                        QueuedSavingUnref => {
                            inner.alloc[slot as usize] = QueuedSavingRef;
                            Decision::Found(slot)
                        }
                        Free => unreachable!("Free slot in map"),
                    },
                    None => Decision::Load,
                }
            };

            match decision {
                Decision::WaitLoaded => {
                    file.page_loaded.listen().await;
                    continue;
                }
                Decision::Found(slot) => {
                    let inner = self.inner.borrow();
                    let page_ref = unsafe { inner.buf.borrow(slot) };
                    return f(page_ref);
                }
                Decision::Load => {
                    let slot = {
                        let alloc_result = {
                            let mut inner = self.inner.borrow_mut();

                            if let Some(slot) = inner.find_allocatable() {
                                inner.map.use_slot_for_new(slot, file.id, page);
                                inner.alloc[slot as usize] = Loading;
                                Some(slot) // Возвращаем успех
                            } else {
                                None // Места нет
                            }
                        };

                        if let Some(slot) = alloc_result {
                            slot
                        } else {
                            self.page_freeable.listen().await;
                            continue;
                        }
                    };

                    let owned = {
                        let mut inner = self.inner.borrow_mut();
                        unsafe { inner.buf.take_mut(slot) }
                    };

                    let offset = u64::from(page.0) * PAGE_SIZE as u64;
                    let BufResult(result, owned) = file.file.read_exact_at(owned, offset).await;

                    let mut inner = self.inner.borrow_mut();
                    unsafe {
                        inner.buf.ret_mut(owned);
                    }

                    if let Err(e) = result {
                        panic!("IO error during read: {e}");
                    }

                    self.page_freeable.notify_additional(1);

                    if inner.map.overwritten_with(file.id, page, slot).is_none() {
                        inner.alloc[slot as usize] = OccupiedRef;
                        file.page_loaded.notify_all();
                    } else {
                        inner.alloc[slot as usize] = Free;
                        file.page_loaded.notify_all();
                    }
                }
            }
        }
    }

    pub(crate) async fn write(&self, file: &File, page: PageId, data: &[u8; PAGE_SIZE]) {
        use AllocStatus::{
            Free, Loading, OccupiedRef, OccupiedUnref, QueuedSavingRef, QueuedSavingUnref,
            SavingRef, SavingUnref,
        };

        enum Decision {
            Done,
            WaitFreeable,
            WritePage { slot: u32 },
        }

        loop {
            let decision = {
                let mut inner = self.inner.borrow_mut();
                match inner.map.get_forward(file.id, page) {
                    Some(slot) => match inner.alloc[slot as usize] {
                        Loading => match inner.find_allocatable() {
                            Some(new_slot) => {
                                inner.map.expire(file.id, page, slot, new_slot);
                                unsafe {
                                    inner.buf.fill(new_slot, data);
                                }
                                inner.alloc[new_slot as usize] = SavingRef;
                                Decision::WritePage { slot: new_slot }
                            }
                            None => Decision::WaitFreeable,
                        },
                        SavingRef | SavingUnref => match inner.find_allocatable() {
                            Some(new_slot) => {
                                inner.map.expire(file.id, page, slot, new_slot);
                                unsafe {
                                    inner.buf.fill(new_slot, data);
                                }
                                inner.alloc[new_slot as usize] = QueuedSavingRef;
                                Decision::Done
                            }
                            None => Decision::WaitFreeable,
                        },
                        OccupiedRef | OccupiedUnref => {
                            unsafe {
                                inner.buf.fill(slot, data);
                            }
                            inner.alloc[slot as usize] = SavingRef;
                            Decision::WritePage { slot }
                        }
                        QueuedSavingRef | QueuedSavingUnref => {
                            inner.alloc[slot as usize] = QueuedSavingRef;
                            unsafe {
                                inner.buf.fill(slot, data);
                            }
                            Decision::Done
                        }
                        Free => unreachable!("Free slot in map"),
                    },
                    None => match inner.find_allocatable() {
                        Some(new_slot) => {
                            inner.map.use_slot_for_new(new_slot, file.id, page);
                            unsafe {
                                inner.buf.fill(new_slot, data);
                            }
                            inner.alloc[new_slot as usize] = SavingRef;
                            Decision::WritePage { slot: new_slot }
                        }
                        None => Decision::WaitFreeable,
                    },
                }
            };

            match decision {
                Decision::Done => return,
                Decision::WaitFreeable => {
                    self.page_freeable.listen().await;
                    continue;
                }
                Decision::WritePage { slot } => {
                    let page_ref = {
                        let mut inner = self.inner.borrow_mut();
                        unsafe { inner.buf.overwrite_take(slot) }
                    };
                    let write_lock = file.write_lock.borrow().clone();
                    let compio_file = file.file.clone();
                    let inner_rc = self.inner.clone();
                    let page_freeable = self.page_freeable.clone();
                    let file_id = file.id;
                    let page_id = page;

                    spawn(write_page_task(
                        compio_file,
                        page_ref,
                        write_lock,
                        inner_rc,
                        page_freeable,
                        file_id,
                        page_id,
                        slot,
                    ))
                    .detach();

                    return;
                }
            }
        }
    }

    pub(crate) async fn flush(&self, file: &File) {
        while file.flush_in_progress.get() {
            file.flush_done.listen().await;
        }

        file.flush_in_progress.set(true);

        let old_lock = file.write_lock.replace(Rc::new(WriteLock(Event::new())));
        let listener = old_lock.0.listen();
        drop(old_lock);
        listener.await;

        if let Err(e) = file.file.sync_data().await {
            panic!("IO error during flush sync_data: {e}");
        }

        file.flush_in_progress.set(false);
        file.flush_done.notify_all();
    }
}

async fn write_page_task(
    compio_file: CompioFile,
    page_ref: &'static AlignedPage,
    _write_lock: Rc<WriteLock>,
    inner: Rc<RefCell<FsInner>>,
    page_freeable: Event,
    file_id: FileId,
    page_id: PageId,
    slot: u32,
) {
    let mut current_slot = slot;
    let mut current_page = page_ref;

    loop {
        let offset = u64::from(page_id.0) * PAGE_SIZE as u64;
        let BufResult(result, returned_page) =
            (&compio_file).write_all_at(current_page, offset).await;
        if let Err(e) = result {
            panic!("IO error during write: {e}");
        }

        let next = {
            let mut inner = inner.borrow_mut();
            unsafe {
                inner.buf.ret(returned_page);
            }

            match inner.map.overwritten_with(file_id, page_id, current_slot) {
                None => {
                    use AllocStatus::{OccupiedRef, OccupiedUnref, SavingRef, SavingUnref};
                    match inner.alloc[current_slot as usize] {
                        SavingRef => {
                            inner.alloc[current_slot as usize] = OccupiedRef;
                        }
                        SavingUnref => {
                            inner.alloc[current_slot as usize] = OccupiedUnref;
                        }
                        _ => unreachable!(
                            "write_page_task: slot {} in unexpected state {:?}",
                            current_slot, inner.alloc[current_slot as usize]
                        ),
                    }
                    page_freeable.notify_additional(1);
                    None
                }
                Some(queued_slot) => {
                    use AllocStatus::{
                        Free, QueuedSavingRef, QueuedSavingUnref, SavingRef, SavingUnref,
                    };
                    inner.alloc[current_slot as usize] = Free;
                    match inner.alloc[queued_slot as usize] {
                        QueuedSavingRef => {
                            inner.alloc[queued_slot as usize] = SavingRef;
                        }
                        QueuedSavingUnref => {
                            inner.alloc[queued_slot as usize] = SavingUnref;
                        }
                        _ => unreachable!(
                            "write_page_task: queued slot {} in unexpected state {:?}",
                            queued_slot, inner.alloc[queued_slot as usize]
                        ),
                    }
                    let new_page = unsafe { inner.buf.overwrite_take(queued_slot) };
                    Some((queued_slot, new_page))
                }
            }
        };

        match next {
            None => break,
            Some((new_slot, new_page)) => {
                current_slot = new_slot;
                current_page = new_page;
            }
        }
    }
}
