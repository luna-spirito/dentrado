use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io,
    num::NonZero,
    panic::AssertUnwindSafe,
    pin::Pin,
    rc::Rc,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use crate::{
    core::{
        core_ctx::{
            CoordCmd, Core, CoreCmd, HandleMsgResult, InterCoreMsg, InterNodeMsg, RerouteMsg,
            broadcast_core_death,
        },
        gear::IsRuntime,
        storage::Storage,
    },
    types::{GlobalCoreId, GlobalHash, NodeId},
    wire::{MergeError, RunGearError, WireEventBody, WireLocCtx},
};
use compio::runtime::JoinHandle;
use futures::future::select_all;

pub use crate::core::doorbell::{Doorbell, DoorbellHandle};

pub struct DbConfig<R: IsRuntime, S: Storage<R>> {
    pub num_cores: NonZero<u32>,
    pub node_id: NodeId,
    pub module: Arc<R::Module>,
    pub peers: HashMap<NodeId, PeerChannels<R>>,
    /// One doorbell per core (the waiting side). Created by the caller,
    /// passed here so `Db::start` can use them in `core_event_loop`.
    pub doorbells: Vec<(Doorbell, DoorbellHandle)>,
    /// Constructs a fresh storage backend for each core, **on that core's
    /// thread**. Building on-thread (rather than moving a pre-built backend
    /// in from the caller) means `Storage` need not be `Send` — backends are
    /// free to hold `!Send` single-threaded types like `Rc`/`RefCell`.
    pub make_storage: Arc<dyn Fn() -> S + Send + Sync>,
}

pub struct PeerChannels<R: IsRuntime> {
    pub remote_num_cores: NonZero<u32>,
    pub channels: Vec<PeerChannelHalf<R>>,
}

pub struct PeerChannelHalf<R: IsRuntime> {
    pub(crate) tx: mpsc::Sender<InterNodeMsg<R>>,
    /// Doorbell of the remote core that receives from this channel.
    pub(crate) remote_doorbell: DoorbellHandle,
    pub(crate) rx: mpsc::Receiver<InterNodeMsg<R>>,
}

#[must_use]
pub fn create_peer_channel_pair<R: IsRuntime>(
    doorbell_a: DoorbellHandle,
    doorbell_b: DoorbellHandle,
) -> (PeerChannelHalf<R>, PeerChannelHalf<R>) {
    let (tx_ab, rx_ab) = mpsc::channel();
    let (tx_ba, rx_ba) = mpsc::channel();
    (
        PeerChannelHalf {
            tx: tx_ab,
            remote_doorbell: doorbell_b,
            rx: rx_ba,
        },
        PeerChannelHalf {
            tx: tx_ba,
            remote_doorbell: doorbell_a,
            rx: rx_ab,
        },
    )
}

/// Used by `Db` to send commands to a core.
#[derive(Clone)]
struct CoreHandle<R: IsRuntime> {
    cmd_tx: mpsc::Sender<CoordCmd<R>>,
    doorbell: DoorbellHandle,
}

/// Result of routing events to their target cores.
pub(crate) struct RoutedEvents<R: IsRuntime> {
    pub(crate) wire_ctx: Arc<WireLocCtx<R>>,
    pub(crate) events: Arc<[WireEventBody<R::Group, R::Body>]>,
    pub(crate) global_core_ids: Arc<[GlobalCoreId]>,
    /// From `target_core` to `event ids`.
    pub(crate) core_seeds: HashMap<u32, Vec<u32>>,
}

/// Shared function to route events
pub(crate) fn route_events<R: IsRuntime>(
    wire_ctx: WireLocCtx<R>,
    events: Vec<WireEventBody<R::Group, R::Body>>,
    num_cores: NonZero<u32>,
) -> Result<RoutedEvents<R>, MergeError> {
    let global_core_ids: Vec<GlobalCoreId> = events
        .iter()
        .map(|e| e.group.global_hash(&wire_ctx).map(GlobalCoreId::from_hash))
        .collect::<Result<_, _>>()
        .map_err(MergeError::Route)?;

    let wire_ctx = Arc::new(wire_ctx);
    let events: Arc<[WireEventBody<R::Group, R::Body>]> = Arc::from(events);
    let global_core_ids: Arc<[GlobalCoreId]> = Arc::from(global_core_ids);

    let mut core_seeds: HashMap<u32, Vec<u32>> = HashMap::new();
    for (i, gcid) in global_core_ids.iter().enumerate() {
        let target_core = gcid.route(num_cores);
        core_seeds.entry(target_core).or_default().push(i as u32);
    }

    Ok(RoutedEvents {
        wire_ctx,
        events,
        global_core_ids,
        core_seeds,
    })
}

/// Shared function to route gear to the target core
pub(crate) fn route_gear<R: IsRuntime>(
    gear: &R::GearId,
    wire_ctx: &WireLocCtx<R>,
    num_cores: NonZero<u32>,
) -> Result<u32, RunGearError> {
    let global_core_id = R::meta(gear)
        .group()
        .global_hash(wire_ctx)
        .map(GlobalCoreId::from_hash)
        .map_err(RunGearError::Route)?;
    Ok(global_core_id.route(num_cores))
}

pub struct Db<R: IsRuntime> {
    node_id: NodeId,
    handles: Vec<CoreHandle<R>>,
    join_handles: Vec<thread::JoinHandle<()>>,
}

impl<R: IsRuntime> Db<R> {
    /// Start the database with no user worker function.
    /// Cores run only the core event loop.
    #[must_use]
    pub fn start<S: Storage<R>>(config: DbConfig<R, S>) -> io::Result<Self> {
        Self::start_with_worker::<S, _, _>(config, |_| std::future::pending::<()>())
    }

    /// Start the database with a user-provided worker function per core.
    ///
    /// # Core death
    ///
    /// A core can die at any moment — its thread panics (`io_uring` `ENOMEM`, a
    /// bug), or a task running on it panics (compio detaches spawned tasks, so
    /// their panics would otherwise vanish without unwinding the thread).
    /// Death is *not* silent: the dying core broadcasts
    /// [`InterCoreMsg::CoreDied`] to every peer — they stop too — so the whole
    /// `Db` dies together (`Db::park`); queued ops fail through their channel
    /// drops, parked awaiters are cancelled along with their cores, and the
    /// callers of [`Db::post_events`] / [`Db::run_gear`] get
    /// [`MergeError::CoreDied`] / [`RunGearError::CoreDied`] back, because
    /// serving with a hole would only strand work routed through the
    /// dead core.
    #[must_use]
    pub fn start_with_worker<S: Storage<R>, W, F>(
        mut config: DbConfig<R, S>,
        worker_fn: W,
    ) -> io::Result<Self>
    where
        W: Fn(Rc<Core<R, S>>) -> F + Clone + Send + 'static,
        F: Future<Output = ()> + 'static,
    {
        let num_cores = config.num_cores;
        let node_id = config.node_id;

        let mut peers_ordered: Vec<(NodeId, PeerChannels<R>)> = config.peers.drain().collect();
        peers_ordered.sort_by_key(|(nid, _)| nid.0);

        // Doorbell handles for this node's cores (cloned everywhere)
        let doorbell_handles: Vec<DoorbellHandle> =
            config.doorbells.iter().map(|(_d, h)| h.clone()).collect();

        let mut core_inter_node_peers: Vec<
            Vec<(
                NodeId,
                NonZero<u32>,
                Option<(mpsc::Sender<InterNodeMsg<R>>, DoorbellHandle)>,
            )>,
        > = (0..num_cores.get())
            .map(|_| Vec::with_capacity(peers_ordered.len()))
            .collect();
        let mut inter_node_rxs: Vec<Vec<Option<mpsc::Receiver<InterNodeMsg<R>>>>> = (0..num_cores
            .get())
            .map(|_| Vec::with_capacity(peers_ordered.len()))
            .collect();

        for (peer_node_id, peer_ch) in peers_ordered {
            let remote_num_cores = peer_ch.remote_num_cores;
            let num_channels = peer_ch.channels.len();
            for (core_id, half) in peer_ch.channels.into_iter().enumerate() {
                core_inter_node_peers[core_id].push((
                    peer_node_id,
                    remote_num_cores,
                    Some((half.tx, half.remote_doorbell)),
                ));
                inter_node_rxs[core_id].push(Some(half.rx));
            }
            for core_id in num_channels as u32..num_cores.get() {
                core_inter_node_peers[core_id as usize].push((
                    peer_node_id,
                    remote_num_cores,
                    None,
                ));
                inter_node_rxs[core_id as usize].push(None);
            }
        }

        // tx_from_to[i][j] = sender from core i to core j
        let mut tx_from_to: Vec<Vec<mpsc::Sender<InterCoreMsg<R>>>> = (0..num_cores.get())
            .map(|_| Vec::with_capacity(num_cores.get() as usize))
            .collect();
        // rx_on_from[j][i] = receiver on core j from core i
        let mut rx_on_from: Vec<Vec<mpsc::Receiver<InterCoreMsg<R>>>> = (0..num_cores.get())
            .map(|_| Vec::with_capacity(num_cores.get() as usize))
            .collect();

        for i in 0..num_cores.get() as usize {
            for j in 0..num_cores.get() as usize {
                let (tx, rx) = mpsc::channel::<InterCoreMsg<R>>();
                tx_from_to[i].push(tx);
                rx_on_from[j].push(rx);
            }
        }

        let mut reroute_txs: Vec<mpsc::Sender<RerouteMsg<R>>> =
            Vec::with_capacity(num_cores.get() as usize);
        let mut reroute_rxs: Vec<mpsc::Receiver<RerouteMsg<R>>> =
            Vec::with_capacity(num_cores.get() as usize);
        for _ in 0..num_cores.get() {
            let (tx, rx) = mpsc::channel::<RerouteMsg<R>>();
            reroute_txs.push(tx);
            reroute_rxs.push(rx);
        }
        let all_reroute_txs: Vec<mpsc::Sender<RerouteMsg<R>>> = reroute_txs.clone();

        let mut handles = Vec::with_capacity(num_cores.get() as usize);
        let mut join_handles = Vec::with_capacity(num_cores.get() as usize);

        for (core_id, (doorbell, _dbh)) in (0..num_cores.get()).zip(config.doorbells) {
            let (cmd_tx, cmd_rx) = mpsc::channel::<CoordCmd<R>>();
            let module = config.module.clone();

            let intercore_senders = std::mem::take(&mut tx_from_to[core_id as usize]);
            let intercore_rxs = std::mem::take(&mut rx_on_from[core_id as usize]);

            let reroute_rx = reroute_rxs.remove(0);
            let reroute_senders = all_reroute_txs.clone();
            let inter_node_peers = std::mem::take(&mut core_inter_node_peers[core_id as usize]);
            let core_inter_node_rxs = std::mem::take(&mut inter_node_rxs[core_id as usize]);
            let core_doorbells = doorbell_handles.clone();

            let worker_fn = worker_fn.clone();
            let make_storage = config.make_storage.clone();

            let join = thread::Builder::new()
                .name(format!("dentrado-core-{core_id}"))
                // .stack_size(CORE_THREAD_STACK_SIZE)
                .spawn(move || {
                    // Death guard: if this thread exits other than via a clean
                    // event-loop shutdown (a panic, a runtime-build failure),
                    // broadcast this core's death — every peer stops serving,
                    // the whole `Db` dies together, and waiters get errors from
                    // the closing channels instead of a freeze.
                    let mut death =
                        CoreDeathGuard::new(intercore_senders.clone(), core_doorbells.clone());

                    let body = AssertUnwindSafe(|| {
                        // Build the storage backend on this thread so that
                        // `!Send` single-threaded backends stay valid.
                        let storage = make_storage();

                        let runtime = build_core_runtime(core_id);

                        runtime.block_on(async move {
                            let state = Rc::new(Core::new(
                                num_cores,
                                core_id,
                                node_id,
                                module,
                                intercore_senders,
                                reroute_senders,
                                core_doorbells,
                                inter_node_peers,
                                storage,
                            ));

                            // A panic anywhere — the event loop (inline in
                            // this `block_on`) or a supervised task — unwinds
                            // this thread, and the death guard broadcasts.
                            // Only the event loop's completion ends the core;
                            // a worker/ticker that finishes normally early
                            // just leaves supervision (`pending`).
                            let tasks: Vec<Pin<Box<dyn Future<Output = ()>>>> = vec![
                                Box::pin(supervised(compio::runtime::spawn(worker_fn(
                                    state.clone(),
                                )))),
                                Box::pin(supervised(compio::runtime::spawn(
                                    state.clone().epoch_ticker_task(),
                                ))),
                                Box::pin(core_event_loop(
                                    state,
                                    doorbell,
                                    cmd_rx,
                                    intercore_rxs,
                                    reroute_rx,
                                    core_inter_node_rxs,
                                )),
                            ];
                            let _ = select_all(tasks).await;
                        });
                    });
                    let result = std::panic::catch_unwind(body);
                    // Clean shutdown — nothing to report. On panic the guard
                    // stays armed and its `Drop` fires the broadcast while the
                    // unwind leaves this thread.
                    if result.is_ok() {
                        death.disarm();
                    }
                    if let Err(panic) = result {
                        std::panic::resume_unwind(panic);
                    }
                })?;

            handles.push(CoreHandle {
                cmd_tx,
                doorbell: doorbell_handles[core_id as usize].clone(),
            });
            join_handles.push(join);
        }

        Ok(Self {
            node_id,
            handles,
            join_handles,
        })
    }

    pub fn post_events(
        &self,
        wire_ctx: WireLocCtx<R>,
        events: Vec<WireEventBody<R::Group, R::Body>>,
        timestamp: u32,
    ) -> Result<(), MergeError> {
        let routed = route_events(
            wire_ctx,
            events,
            NonZero::new(self.handles.len() as u32).unwrap(),
        )?;

        let mut reply_rxs = Vec::with_capacity(routed.core_seeds.len());
        for (target_core, seed_indices) in routed.core_seeds {
            let handle = &self.handles[target_core as usize];
            let (reply_tx, reply_rx) = flume::bounded(1);
            handle
                .cmd_tx
                .send(CoordCmd::Op(CoreCmd::PostEvents {
                    wire_ctx: routed.wire_ctx.clone(),
                    events: routed.events.clone(),
                    global_core_ids: routed.global_core_ids.clone(),
                    timestamp,
                    seed_indices,
                    forwarded_from: None,
                    reply: Some(reply_tx),
                }))
                .map_err(|_| MergeError::CoreDied)?;
            handle.doorbell.ring();
            reply_rxs.push(reply_rx);
        }
        for reply_rx in reply_rxs {
            reply_rx.recv().map_err(|_| MergeError::CoreDied)??;
        }

        Ok(())
    }

    pub fn run_gear(
        &self,
        gear: R::GearId,
        wire_ctx: WireLocCtx<R>,
    ) -> Result<R::GearOut, RunGearError> {
        let target_core = route_gear(
            &gear,
            &wire_ctx,
            NonZero::new(self.handles.len() as u32).unwrap(),
        )?;
        let handle = &self.handles[target_core as usize];

        let (reply_tx, reply_rx) = flume::bounded(1);
        handle
            .cmd_tx
            .send(CoordCmd::Op(CoreCmd::RunGear {
                gear,
                wire_ctx,
                reply: reply_tx,
            }))
            .map_err(|_| RunGearError::CoreDied)?;
        handle.doorbell.ring();
        match reply_rx.recv() {
            Ok(result) => result,
            Err(_) => Err(RunGearError::CoreDied),
        }
    }

    /// Wait for every core thread to finish. A `Db` whose cores are dying
    /// (the `CoreDied` cascade or a clean shutdown) parks until the last
    /// thread is gone — it never hangs on a live-but-dead core.
    pub fn park(&mut self) {
        for join in self.join_handles.drain(..) {
            let _ = join.join();
        }
    }
}

impl<R: IsRuntime> Drop for Db<R> {
    fn drop(&mut self) {
        for handle in &self.handles {
            let _ = handle.cmd_tx.send(CoordCmd::<R>::Shutdown);
            handle.doorbell.ring();
        }
        self.park();
    }
}

/// Fires a core-death broadcast ([`broadcast_core_death`]) on drop, unless
/// disarmed by a clean event-loop shutdown. Guards the whole body of a core
/// thread: storage build, compio runtime build (the ENOMEM retry), and the
/// event loop itself.
struct CoreDeathGuard<R: IsRuntime> {
    txs: Vec<mpsc::Sender<InterCoreMsg<R>>>,
    doorbells: Vec<DoorbellHandle>,
    armed: bool,
}

impl<R: IsRuntime> CoreDeathGuard<R> {
    fn new(txs: Vec<mpsc::Sender<InterCoreMsg<R>>>, doorbells: Vec<DoorbellHandle>) -> Self {
        Self {
            txs,
            doorbells,
            armed: true,
        }
    }

    /// A clean event-loop shutdown — no death to report.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<R: IsRuntime> Drop for CoreDeathGuard<R> {
    fn drop(&mut self) {
        if self.armed {
            broadcast_core_death(&self.txs, &self.doorbells);
        }
    }
}

/// Await a supervised core task so its panic cannot vanish: compio catches a
/// panicking task's unwind into its `JoinHandle` result, so resuming it here —
/// on the `block_on` path, not from inside a task — unwinds the core thread,
/// where the death guard broadcasts the node's death. A task that finishes
/// normally early is simply left detached from supervision; only the event
/// loop's completion ends the core.
async fn supervised(handle: JoinHandle<()>) {
    if let Err(e) = handle.await {
        e.resume_unwind();
    }
    std::future::pending::<()>().await;
}

/// Build a core's compio runtime, retrying transient `ENOMEM`: `io_uring` ring
/// allocation fails under system memory pressure (many test clusters at once,
/// a busy host), which passes on a retry rather than indicating a real leak.
/// A retry-exhausted or non-transient failure panics — the thread's death
/// guard then notifies awaiters ([`Db::start_with_worker`]).
fn build_core_runtime(core_id: u32) -> compio::runtime::Runtime {
    for attempt in 0..5u32 {
        let err = match compio::runtime::RuntimeBuilder::new()
            .thread_affinity(HashSet::from([core_id as usize]))
            .build()
        {
            Ok(runtime) => return runtime,
            Err(e) => e,
        };
        assert!(
            !(err.kind() != io::ErrorKind::OutOfMemory || attempt == 4),
            "compio runtime build failed: {err}"
        );
        thread::sleep(Duration::from_millis(10 << attempt));
    }
    unreachable!()
}

async fn core_event_loop<R: IsRuntime, S: Storage<R>>(
    state: Rc<Core<R, S>>,
    doorbell: Doorbell,
    cmd_rx: mpsc::Receiver<CoordCmd<R>>,
    intercore_rxs: Vec<mpsc::Receiver<InterCoreMsg<R>>>,
    reroute_rx: mpsc::Receiver<RerouteMsg<R>>,
    inter_node_rxs: Vec<Option<mpsc::Receiver<InterNodeMsg<R>>>>,
) {
    loop {
        // Clear doorbell BEFORE draining. any ring during drain
        // sets the flag, so the next wait returns immediately.
        doorbell.clear();

        let mut did_work = false;

        // 1. Drain inter-node channels (forwarded events first)
        for (peer_idx, rx_opt) in inter_node_rxs.iter().enumerate() {
            if let Some(rx) = rx_opt {
                loop {
                    match rx.try_recv() {
                        Ok(msg) => {
                            state.handle_inter_node_msg(peer_idx, msg).await;
                            did_work = true;
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    }
                }
            }
        }

        // 2. Drain inter-core SPSC channels. The channel index *is* the sender's
        //    core id, so the handlers learn the sender from it (no `from_core`
        //    field needed on shared messages).
        for (from_core, rx) in intercore_rxs.iter().enumerate() {
            loop {
                match rx.try_recv() {
                    Ok(msg) => {
                        match state.handle_intercore_msg(msg, from_core as u32).await {
                            HandleMsgResult::Ok => (),
                            HandleMsgResult::Die => return, // A peer's `CoreDied` marks us dead — exit promptly.
                        }
                        did_work = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }

        // 3. Drain reroute channel
        loop {
            match reroute_rx.try_recv() {
                Ok(msg) => {
                    state.handle_reroute_msg(msg);
                    did_work = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }

        // 4. Drain commands (from Db).
        loop {
            match cmd_rx.try_recv() {
                Ok(CoordCmd::Op(op)) => {
                    state.handle_client_op(op).await;
                    did_work = true;
                }
                Ok(CoordCmd::Shutdown) => return,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }

        // 5. If no work, await doorbell (blocks task, releases OS thread)
        if !did_work {
            doorbell.wait().await;
        }
    }
}
