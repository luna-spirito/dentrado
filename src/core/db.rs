use std::{
    collections::HashMap,
    io,
    sync::{mpsc, Arc},
    thread,
};

use crate::{
    core::{
        core_ctx::{Core, CoreCmd, InterCoreMsg, NodeMsg, RerouteMsg},
        gear::Runtime,
    },
    types::{GlobalCoreId, NodeId},
    wire::{MergeError, RunGearError, WireEventBody, WireLocCtx},
};

pub struct DbConfig<R: Runtime> {
    pub num_cores: u32,
    pub node_id: NodeId,
    pub module: Arc<R::Module>,
    pub peers: HashMap<NodeId, PeerChannels<R>>,
}

pub struct PeerChannels<R: Runtime> {
    pub remote_num_cores: u32,
    pub channels: Vec<PeerChannelHalf<R>>,
}

pub struct PeerChannelHalf<R: Runtime> {
    pub(crate) tx: mpsc::Sender<NodeMsg<R>>,
    pub(crate) rx: mpsc::Receiver<NodeMsg<R>>,
}

#[must_use]
pub fn create_peer_channel_pair<R: Runtime>() -> (PeerChannelHalf<R>, PeerChannelHalf<R>) {
    let (tx_ab, rx_ab) = mpsc::channel();
    let (tx_ba, rx_ba) = mpsc::channel();
    (
        PeerChannelHalf {
            tx: tx_ab,
            rx: rx_ba,
        },
        PeerChannelHalf {
            tx: tx_ba,
            rx: rx_ab,
        },
    )
}

#[derive(Clone)]
struct CoreHandle<R: Runtime> {
    cmd_tx: mpsc::Sender<CoreCmd<R>>,
    intercore_tx: mpsc::Sender<InterCoreMsg<R>>,
    #[allow(dead_code)]
    core_idx: u32,
}

impl<R: Runtime> CoreHandle<R> {
    fn post_events(
        &self,
        wire_ctx: Arc<WireLocCtx<R>>,
        events: Arc<[WireEventBody<R::Group, R::Body>]>,
        global_core_ids: Arc<[GlobalCoreId]>,
        timestamp: u32,
        seed_indices: Vec<u32>,
    ) -> io::Result<Result<(), MergeError>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.cmd_tx
            .send(CoreCmd::PostEvents {
                wire_ctx,
                events,
                global_core_ids,
                timestamp,
                seed_indices,
                reply: reply_tx,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "core channel closed"))?;
        reply_rx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "core reply dropped"))
    }

    fn run_gear(
        &self,
        gear: R::GearId,
        wire_ctx: Arc<WireLocCtx<R>>,
    ) -> io::Result<Result<R::GearOut, RunGearError>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.cmd_tx
            .send(CoreCmd::RunGear {
                gear,
                wire_ctx,
                reply: reply_tx,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "core channel closed"))?;
        reply_rx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "core reply dropped"))
    }
}

#[derive(Clone)]
pub struct DbHandle<R: Runtime> {
    core_handles: Vec<CoreHandle<R>>,
}

impl<R: Runtime> DbHandle<R> {
    #[allow(clippy::cast_possible_truncation)]
    fn core_idx_for_global_core_id(&self, global_core_id: &GlobalCoreId) -> usize {
        let num_cores = self.core_handles.len() as u32;
        global_core_id.route(num_cores) as usize
    }

    pub fn post_events(
        &self,
        wire_ctx: WireLocCtx<R>,
        events: Vec<WireEventBody<R::Group, R::Body>>,
        timestamp: u32,
    ) -> Result<(), MergeError> {
        let global_core_ids: Vec<GlobalCoreId> = events
            .iter()
            .map(|e| R::route_group(&e.group, &wire_ctx))
            .collect::<Result<_, _>>()
            .map_err(MergeError::Route)?;

        let wire_ctx = Arc::new(wire_ctx);
        let events: Arc<[WireEventBody<R::Group, R::Body>]> = Arc::from(events);
        let global_core_ids: Arc<[GlobalCoreId]> = Arc::from(global_core_ids);

        let mut core_seeds: HashMap<usize, Vec<u32>> = HashMap::new();
        for (i, global_core_id) in global_core_ids.iter().enumerate() {
            let core_idx = self.core_idx_for_global_core_id(global_core_id);
            core_seeds.entry(core_idx).or_default().push(i as u32);
        }

        for (core_idx, seed_indices) in core_seeds {
            let core = &self.core_handles[core_idx];
            let result = core
                .post_events(
                    Arc::clone(&wire_ctx),
                    Arc::clone(&events),
                    Arc::clone(&global_core_ids),
                    timestamp,
                    seed_indices,
                )
                .unwrap_or_else(|_| panic!("post_events: core {core_idx} channel closed"));
            result?;
        }

        Ok(())
    }

    pub fn run_gear(
        &self,
        gear: R::GearId,
        wire_ctx: WireLocCtx<R>,
    ) -> Result<R::GearOut, RunGearError> {
        let (_, group) = R::meta(&gear);
        let global_core_id = R::route_group(&group, &wire_ctx).map_err(RunGearError::Route)?;
        let core_idx = self.core_idx_for_global_core_id(&global_core_id);
        let core = &self.core_handles[core_idx];

        core.run_gear(gear, Arc::new(wire_ctx))
            .unwrap_or_else(|_| panic!("run_gear: core channel closed"))
    }
}

pub struct Db<R: Runtime> {
    shutdown_handles: Vec<mpsc::Sender<CoreCmd<R>>>,
    join_handles: Vec<thread::JoinHandle<()>>,
}

impl<R: Runtime> Db<R> {
    pub fn start(mut config: DbConfig<R>) -> io::Result<(Self, DbHandle<R>)> {
        let num_cores = config.num_cores;
        let node_id = config.node_id;

        let mut peers_ordered: Vec<(NodeId, PeerChannels<R>)> = config.peers.drain().collect();
        peers_ordered.sort_by_key(|(nid, _)| nid.0);

        let mut core_inter_node_peers: Vec<Vec<(NodeId, u32, Option<mpsc::Sender<NodeMsg<R>>>)>> =
            (0..num_cores)
                .map(|_| Vec::with_capacity(peers_ordered.len()))
                .collect();
        let mut inter_node_rxs: Vec<Vec<Option<mpsc::Receiver<NodeMsg<R>>>>> = (0..num_cores)
            .map(|_| Vec::with_capacity(peers_ordered.len()))
            .collect();

        for (peer_node_id, peer_ch) in peers_ordered {
            let remote_num_cores = peer_ch.remote_num_cores;
            let num_channels = peer_ch.channels.len();
            for (core_id, half) in peer_ch.channels.into_iter().enumerate() {
                core_inter_node_peers[core_id].push((
                    peer_node_id,
                    remote_num_cores,
                    Some(half.tx),
                ));
                inter_node_rxs[core_id].push(Some(half.rx));
            }
            for core_id in num_channels as u32..num_cores {
                core_inter_node_peers[core_id as usize].push((
                    peer_node_id,
                    remote_num_cores,
                    None,
                ));
                inter_node_rxs[core_id as usize].push(None);
            }
        }

        let mut core_handles = Vec::with_capacity(num_cores as usize);
        let mut shutdown_handles = Vec::with_capacity(num_cores as usize);
        let mut join_handles = Vec::with_capacity(num_cores as usize);

        let mut intercore_txs: Vec<mpsc::Sender<InterCoreMsg<R>>> =
            Vec::with_capacity(num_cores as usize);
        let mut intercore_rxs: Vec<mpsc::Receiver<InterCoreMsg<R>>> =
            Vec::with_capacity(num_cores as usize);
        for _ in 0..num_cores {
            let (tx, rx) = mpsc::channel::<InterCoreMsg<R>>();
            intercore_txs.push(tx);
            intercore_rxs.push(rx);
        }
        let all_intercore_txs: Vec<mpsc::Sender<InterCoreMsg<R>>> = intercore_txs.clone();

        let mut reroute_txs: Vec<mpsc::Sender<RerouteMsg<R>>> =
            Vec::with_capacity(num_cores as usize);
        let mut reroute_rxs: Vec<mpsc::Receiver<RerouteMsg<R>>> =
            Vec::with_capacity(num_cores as usize);
        for _ in 0..num_cores {
            let (tx, rx) = mpsc::channel::<RerouteMsg<R>>();
            reroute_txs.push(tx);
            reroute_rxs.push(rx);
        }
        let all_reroute_txs: Vec<mpsc::Sender<RerouteMsg<R>>> = reroute_txs.clone();

        for core_id in 0..num_cores {
            let (cmd_tx, cmd_rx) = mpsc::channel::<CoreCmd<R>>();
            let module = config.module.clone();
            let intercore_rx = intercore_rxs.remove(0);
            let intercore_senders = all_intercore_txs.clone();
            let reroute_rx = reroute_rxs.remove(0);
            let reroute_senders = all_reroute_txs.clone();
            let inter_node_peers = std::mem::take(&mut core_inter_node_peers[core_id as usize]);
            let core_inter_node_rxs = std::mem::take(&mut inter_node_rxs[core_id as usize]);

            let join = thread::Builder::new()
                .name(format!("kolorinko-core-{core_id}"))
                .spawn(move || {
                    core_event_loop::<R>(
                        num_cores,
                        core_id,
                        node_id,
                        cmd_rx,
                        intercore_rx,
                        intercore_senders,
                        reroute_rx,
                        reroute_senders,
                        inter_node_peers,
                        core_inter_node_rxs,
                        module,
                    );
                })?;

            core_handles.push(CoreHandle {
                cmd_tx: cmd_tx.clone(),
                intercore_tx: intercore_txs[core_id as usize].clone(),
                core_idx: core_id,
            });
            shutdown_handles.push(cmd_tx);
            join_handles.push(join);
        }

        let db = Self {
            shutdown_handles,
            join_handles,
        };

        let cluster = DbHandle { core_handles };

        Ok((db, cluster))
    }
}

impl<R: Runtime> Drop for Db<R> {
    fn drop(&mut self) {
        for tx in &self.shutdown_handles {
            tx.send(CoreCmd::<R>::Shutdown)
                .expect("Db::drop: failed to send Shutdown to core");
        }
        for handle in self.join_handles.drain(..) {
            handle.join().expect("Db::drop: core thread panicked");
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn core_event_loop<R: Runtime>(
    num_cores: u32,
    core_id: u32,
    node_id: NodeId,
    cmd_rx: mpsc::Receiver<CoreCmd<R>>,
    intercore_rx: mpsc::Receiver<InterCoreMsg<R>>,
    intercore_senders: Vec<mpsc::Sender<InterCoreMsg<R>>>,
    reroute_rx: mpsc::Receiver<RerouteMsg<R>>,
    reroute_senders: Vec<mpsc::Sender<RerouteMsg<R>>>,
    inter_node_peers: Vec<(NodeId, u32, Option<mpsc::Sender<NodeMsg<R>>>)>,
    inter_node_rxs: Vec<Option<mpsc::Receiver<NodeMsg<R>>>>,
    module: Arc<R::Module>,
) {
    let mut state: Core<R> = Core::new(
        num_cores,
        core_id,
        node_id,
        module,
        intercore_senders,
        reroute_senders,
        inter_node_peers,
    );

    let recv_timeout = std::time::Duration::from_millis(1);
    loop {
        loop {
            match cmd_rx.try_recv() {
                Ok(cmd) => {
                    if state.handle_cmd(cmd) {
                        return;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }

        while let Ok(msg) = intercore_rx.try_recv() {
            state.handle_intercore_msg(msg);
        }

        for (peer_idx, rx_opt) in inter_node_rxs.iter().enumerate() {
            if let Some(rx) = rx_opt {
                while let Ok(msg) = rx.try_recv() {
                    state.handle_inter_node_msg(peer_idx, msg);
                }
            }
        }

        while let Ok(msg) = reroute_rx.try_recv() {
            state.handle_reroute_msg(msg);
        }

        match cmd_rx.recv_timeout(recv_timeout) {
            Ok(cmd) => {
                if state.handle_cmd(cmd) {
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}
