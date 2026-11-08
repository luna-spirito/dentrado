//! The publication worker handle and the `repo` gear's local output.
//!
//! These types are deliberately outside [`crate::wikidot_page`]: the generated
//! runtime's `GearOutLocal` embeds [`RepoState`], so it must be nameable before
//! both the runtime and the gear implementations. The worker itself (the
//! thread, the manifest reader, the snapshot builder) stays in
//! `wikidot_page::out_worker`; only the channel handle and the output value
//! live here.

use std::{fmt, rc::Rc};

use kolorinko_rt::RepoSnapshot;

/// A request to the publication worker thread. Each carries its own one-shot
/// reply channel; the oracle core sends one and `.await`s the reply, so it
/// never blocks while the worker decompresses archives on its own thread.
pub(crate) enum OutReq {
    /// Rescan the publication and rebuild what drifted. `changed` is `false`
    /// when nothing drifted; the caller then keeps its cached `Rc` (the data is
    /// still the worker's current snapshot).
    Tick { reply: flume::Sender<OutReply> },
    /// The current dataset without rescanning (cold start / first non-tick run).
    Snapshot { reply: flume::Sender<OutReply> },
}

/// One worker reply: the current dataset, whether a `Tick` adopted anything,
/// and — when the worker's state changed since the last reply — its freshly
/// serialized [`PersistedWorker`](crate::wikidot_page::out_worker::PersistedWorker)
/// image for the gear view.
pub(crate) struct OutReply {
    pub(crate) data: RepoSnapshot,
    pub(crate) changed: bool,
    pub(crate) image: Option<Vec<u8>>,
}

impl Default for OutReply {
    fn default() -> Self {
        Self {
            data: RepoSnapshot::default(),
            changed: false,
            image: None,
        }
    }
}

/// Cloneable, `Send` handle to the publication worker thread, held by the
/// [`repo`](crate::wikidot_page::repo) oracle's cache so all synchronous
/// filesystem work — manifest parsing, zstd decompression, body
/// materialisation — happens off the async core.
#[derive(Clone)]
pub(crate) struct OutMailbox(pub(crate) flume::Sender<OutReq>);

impl OutMailbox {
    /// Rescan + rebuild what drifted.
    pub(crate) async fn tick(&self) -> OutReply {
        self.request(|reply| OutReq::Tick { reply }).await
    }

    /// Current dataset without rescanning.
    pub(crate) async fn snapshot(&self) -> OutReply {
        self.request(|reply| OutReq::Snapshot { reply }).await
    }

    async fn request(&self, req: impl FnOnce(flume::Sender<OutReply>) -> OutReq) -> OutReply {
        let (tx, rx) = flume::bounded(1);
        if self.0.send_async(req(tx)).await.is_err() {
            return OutReply::default();
        }
        rx.recv_async().await.unwrap_or_default()
    }
}

/// The `repo` gear's local output: the publication worker handle (spawned
/// lazily on the first run) and the last adopted snapshot. The worker handle is
/// process-local and cannot be persisted, so it rides the output and is
/// recovered through
/// [`GearCtx::previous_output`](dentrado::core::core_ctx::GearCtx::previous_output).
#[derive(Clone)]
pub(crate) struct RepoState {
    pub(crate) mailbox: OutMailbox,
    pub(crate) data: Rc<RepoSnapshot>,
}

impl fmt::Debug for RepoState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the snapshot: it is the whole corpus.
        f.debug_struct("RepoState").finish_non_exhaustive()
    }
}

impl RepoState {
    /// The adopted snapshot.
    pub(crate) fn snapshot(&self) -> &RepoSnapshot {
        &self.data
    }
}
