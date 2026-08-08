//! Runtime exercise of the `Shared` output family: a shared gear produces an
//! opaque value, a second gear reads it via `secondary_get` (the cross-core
//! gear-dependency path), and the value survives intact. This is the only test
//! that drives the shared machinery (arena refcount, `Shared` handle, cross-core
//! pointer push/unref) at runtime — the rest of the suite uses only `Ship`.

use std::fmt::Debug;

use dentrado::{
    core::{
        core_ctx::{Core, GearCtx},
        gear::{GearInput, GearMeta, GearProduce, GearResult, IsRuntime},
        storage::{CacheSer, InMemoryStorage, PageId, Storage},
    },
    types::*,
};

mod common;
use common::TestCluster;

/// A distinct event type so the shared and consumer gears are addressable.
const MSG_SHARED: LocMsgTypeId = LocMsgTypeId(7);

/// `Src` and `Len` deliberately share a `bucket` but sit in **different**
/// groups (`bucket` vs `bucket + 1000`), so on a multi-core cluster they land on
/// different cores — exercising the cross-core shared-pointer path. `Len`
/// `secondary_get`s `Src`: if `Src` is on another core, its `Shared` payload
/// arrives by raw pointer (`SubscriptionUpdateShared`/`SharedUnref`).
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum SharedGear {
    Src { bucket: i64 },
    Len { bucket: i64 },
}

impl Localizable for SharedGear {
    async fn localize<Rm: Remapper>(self, _r: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

/// The shared payload family: `String`-carrying, `Sync`, deliberately **not**
/// `Clone` on the runtime side (the core refcounts the allocation, never copies
/// it). `Src` is the only shared gear.
#[derive(Debug)]
pub enum SharedOut {
    SrcOut(String),
}

#[derive(Debug, Clone, Default)]
pub struct SharedCache<W>(core::marker::PhantomData<W>);

impl<W> CacheSer for SharedCache<W> {
    fn page_roots(&self) -> &[PageId] {
        &[]
    }
}

#[derive(Debug, Clone)]
pub struct SharedRuntime;

impl IsRuntime for SharedRuntime {
    type GearId = SharedGear;
    type GearOut = usize;
    type GearOutShared = SharedOut;
    type GearOutLocal = ();
    type Module = ();
    type Group = i64;
    type Body = ();
    type Data = ();
    type GearCache<W>
        = SharedCache<W>
    where
        W: Debug + Clone + 'static;

    fn meta(gear: &Self::GearId) -> GearMeta<Self> {
        match gear {
            SharedGear::Src { bucket } => GearMeta::Event {
                msg_type: MSG_SHARED,
                group: *bucket,
            },
            // A different group so `Len` routes to (likely) another core than
            // `Src`, forcing the cross-core shared path.
            SharedGear::Len { bucket } => GearMeta::Event {
                msg_type: MSG_SHARED,
                group: bucket + 1000,
            },
        }
    }

    fn make_cache<W: Debug + Clone + Default + 'static>(
        _gear: &Self::GearId,
    ) -> Self::GearCache<W> {
        SharedCache(core::marker::PhantomData)
    }

    async fn run_step<S: Storage<Self>>(
        ctx: &mut GearCtx<Self, S>,
        _input: GearInput<Self>,
        _cache: &mut Self::GearCache<S::Watermark>,
    ) -> GearProduce<Self> {
        match ctx.gear() {
            SharedGear::Src { bucket } => {
                GearProduce::Shared(SharedOut::SrcOut(format!("hello-{bucket}")))
            }
            SharedGear::Len { bucket } => {
                // Read the shared `Src` output — locally if co-located, by raw
                // pointer if cross-core — and project the payload out of the
                // `Shared` handle.
                let res = ctx.secondary_get(SharedGear::Src { bucket: *bucket }).await;
                let GearResult::Shared(s) = res else {
                    unreachable!("Src produces a Shared result");
                };
                let SharedOut::SrcOut(text) = &*s;
                GearProduce::Ship(text.len())
            }
        }
    }
}

enum Cmd {
    /// Read `Src`'s shared payload out of its `GearResult::Shared` and reply.
    ReadSrc(i64, flume::Sender<String>),
    /// Read `Len` (a `Ship` gear that `secondary_get`s `Src`) and reply.
    ReadLen(i64, flume::Sender<usize>),
}

/// Single-core smoke test: `Src` installs a shared allocation, `read_gear`
/// returns `GearResult::Shared`, and the `Shared` handle derefs to the payload.
#[test]
fn shared_install_and_read() {
    let (cmd_tx, cmd_rx) = flume::unbounded::<Cmd>();
    let worker = move |core: std::rc::Rc<Core<SharedRuntime, InMemoryStorage<SharedRuntime>>>| {
        let cmd_rx = cmd_rx.clone();
        async move {
            while let Ok(cmd) = cmd_rx.recv_async().await {
                match cmd {
                    Cmd::ReadSrc(bucket, reply) => {
                        let res = core.read_gear(SharedGear::Src { bucket }).await;
                        let s = res.into_shared().expect("Src is Shared");
                        let SharedOut::SrcOut(text) = &*s;
                        let _ = reply.send(text.clone());
                    }
                    Cmd::ReadLen(bucket, reply) => {
                        let res = core.read_gear(SharedGear::Len { bucket }).await;
                        let _ = reply.send(res.expect_ship());
                    }
                }
            }
        }
    };

    let mut tc: TestCluster<SharedRuntime, InMemoryStorage<SharedRuntime>> =
        TestCluster::start_with_worker(&[1], (), worker);
    let bucket = 0;
    tc.mk_loc_group(MSG_SHARED, bucket);
    tc.mk_loc_group(MSG_SHARED, bucket + 1000);

    let (tx, rx) = flume::bounded(1);
    cmd_tx.send(Cmd::ReadSrc(bucket, tx)).unwrap();
    assert_eq!(rx.recv().unwrap(), "hello-0");

    let (tx, rx) = flume::bounded(1);
    cmd_tx.send(Cmd::ReadLen(bucket, tx)).unwrap();
    assert_eq!(rx.recv().unwrap(), "hello-0".len());
}

/// Multi-core: `Src` and `Len` route to different cores, so `Len`'s
/// `secondary_get` of `Src` crosses a core boundary — the shared payload
/// travels as a raw pointer and the unref round-trips back to the owner. The
/// value must be correct regardless.
#[test]
fn shared_cross_core_secondary_get() {
    let (cmd_tx, cmd_rx) = flume::unbounded::<Cmd>();
    let worker = move |core: std::rc::Rc<Core<SharedRuntime, InMemoryStorage<SharedRuntime>>>| {
        let cmd_rx = cmd_rx.clone();
        async move {
            while let Ok(cmd) = cmd_rx.recv_async().await {
                match cmd {
                    Cmd::ReadSrc(bucket, reply) => {
                        let res = core.read_gear(SharedGear::Src { bucket }).await;
                        let s = res.into_shared().expect("Src is Shared");
                        let SharedOut::SrcOut(text) = &*s;
                        let _ = reply.send(text.clone());
                    }
                    Cmd::ReadLen(bucket, reply) => {
                        let res = core.read_gear(SharedGear::Len { bucket }).await;
                        let _ = reply.send(res.expect_ship());
                    }
                }
            }
        }
    };

    let mut tc: TestCluster<SharedRuntime, InMemoryStorage<SharedRuntime>> =
        TestCluster::start_with_worker(&[4], (), worker);
    // Several buckets → several `Src`/`Len` pairs, raising the chance at least
    // one `Len` reads a `Src` on a genuinely different core.
    for b in 0..8 {
        tc.mk_loc_group(MSG_SHARED, b);
        tc.mk_loc_group(MSG_SHARED, b + 1000);
    }

    for bucket in 0..8 {
        let (tx, rx) = flume::bounded(1);
        cmd_tx.send(Cmd::ReadLen(bucket, tx)).unwrap();
        assert_eq!(
            rx.recv().unwrap(),
            format!("hello-{bucket}").len(),
            "Len read Src (bucket {bucket})"
        );
    }
}
