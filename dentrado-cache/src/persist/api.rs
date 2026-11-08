//! The desk: what the store calls — the async surface over the cache.

use std::cell::RefCell;
use std::rc::Rc;

use futures::channel::oneshot;

use super::PersistentCore;
use crate::blockcache::Submit;
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::{AlignedPage, PageId};

/// A wish's one ladder: the answer on the spot, or the queue's by the
/// receiver — one step over the cache either way.
async fn finish<R>(s: Submit<R>) -> Result<R, StoreError> {
    match s {
        Submit::Now(r) => Ok(r),
        Submit::Queued { rx, .. } => rx.await.map_err(|_| StoreError::ChannelClosed)?,
    }
}

/// Deliver a page resident and read it: `f` is pure — a page at rest
/// is served in the caller's section, anything else flies as a post.
/// A load failure (eof) arrives as an error — terminal.
pub(crate) async fn read<D: IsFilesystem, R>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    id: PageId,
    f: impl Fn(&AlignedPage) -> R + 'static,
) -> Result<R, StoreError>
where
    R: 'static,
{
    // the section ends before the await: a RefMut held across it would
    // starve the driver — the answer would never come
    let served = inner
        .borrow_mut()
        .core
        .request(id, true, move |p| (false, f(&*p)));
    finish(served).await
}

/// Deliver a page resident and update it in place: `f` mutates the
/// bytes right in the arena — the page never leaves the cache.
pub(crate) async fn update<D: IsFilesystem, R>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    id: PageId,
    f: impl Fn(&mut AlignedPage) -> R + 'static,
) -> Result<R, StoreError>
where
    R: 'static,
{
    let served = inner
        .borrow_mut()
        .core
        .request(id, true, move |p| (true, f(p)));
    finish(served).await
}

/// Create a page: nothing is read from the disk, the id has no past. `f`
/// writes over the slot's garbage bytes — no promises about them.
pub(crate) async fn init<D: IsFilesystem, R>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    id: PageId,
    f: impl Fn(&mut AlignedPage) -> R + 'static,
) -> Result<R, StoreError>
where
    R: 'static,
{
    let served = inner
        .borrow_mut()
        .core
        .request(id, false, move |p| (true, f(p)));
    finish(served).await
}

/// The page is whole on disk: an idle clean-or-missing one is already
/// there (answered at once), anything queued runs first and its dirt
/// gets a save. For point writes whose presence must happen before a
/// sync — the superblock.
pub(crate) async fn ensure_saved<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    id: PageId,
) -> Result<(), StoreError> {
    let (tx, rx) = oneshot::channel();
    inner.borrow_mut().core.persist(id, tx);
    rx.await.map_err(|_| StoreError::ChannelClosed)?
}

/// A cache checkpoint — a barrier over every queue and all dirt: the
/// answer comes when they have settled onto the disk (written — the
/// sync is the caller's business). A failure is a terminal error.
pub(crate) async fn write_back_all<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
) -> Result<(), StoreError> {
    let (tx, rx) = oneshot::channel();
    inner.borrow_mut().core.checkpoint(tx);
    rx.await.map_err(|_| StoreError::ChannelClosed)?
}
