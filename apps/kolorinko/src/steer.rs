//! QUIC connection-ID steering across the per-core `SO_REUSEPORT` endpoints.
//!
//! The QUIC port is shared by every core through `SO_REUSEPORT`, and the
//! kernel's default dispatch hashes datagrams by client 4-tuple. Over the
//! real internet that 4-tuple is not stable: a NAT that remaps the client's
//! UDP flow (or a Wi-Fi → cellular hop) changes the source port, the next
//! packet lands on a core where the connection doesn't exist, and the
//! stateless reset that core answers with kills the WebTransport session
//! (see `server.rs`). This module makes the dispatch connection-aware:
//!
//! 1. [`SteerCid`], installed as the endpoint's `ConnectionIdGenerator`,
//!    mints every connection ID with its owning core encoded in the DCID —
//!    obfuscated through a secret per-process permutation ([`permutation`]),
//!    so the encoding survives RFC 9000's anti-linkability rule.
//! 2. [`bpf::enroll`] loads the steering program (`steer-bpf`, compiled by
//!    `build.rs`) once and registers each core's socket under its core id.
//!    The program decodes the core straight out of each 1-RTT packet's DCID
//!    and selects that core's socket; anything else — long headers, unknown
//!    cores, oversized core counts — falls through to the kernel's default
//!    hash dispatch, which is also the whole behavior when the program
//!    couldn't be loaded (no `CAP_BPF`, non-Linux, `server.steer = false`).
//! 3. With packets now reaching the owning core from the client's new
//!    address, quinn's path validation migrates the connection: the
//!    WebTransport session survives the rebind.
//!
//! The CID encoding itself is platform-independent (harmless where nothing
//! decodes it); only the eBPF half is, and it engages with `server.steer`.

use std::sync::OnceLock;

use compio_quic::{ConnectionId, ConnectionIdGenerator};
use ring::rand::SecureRandom;

/// The DCID length minted by [`SteerCid`]: byte 0 is the nonce, byte 1 the
/// obfuscated core, the rest pure randomness (the eBPF program mirrors these
/// offsets — see `steer-bpf`).
const CID_NONCE: usize = 0;
const CID_OBF: usize = 1;
const CID_LEN: usize = 8;

/// The steering program supports cores below this id; a machine with more
/// cores than that simply leaves the excess on kernel-hash dispatch (the
/// `socks` map in `steer-bpf` caps at the same value).
pub(crate) const MAX_CORES: u32 = 64;

/// Enroll `socket` — already bound into the shared `SO_REUSEPORT` group — as
/// `core`'s endpoint, loading and attaching the steering program on the first
/// call. Never fatal: steering problems only log and fall back to the
/// kernel's hash dispatch. A no-op unless [`MAX_CORES`] covers the machine
/// (see [`run_server`]'s warning) and the platform carries eBPF.
pub(crate) fn enroll(socket: &(impl std::os::fd::AsFd + std::os::fd::AsRawFd), core: u32) {
    if core >= MAX_CORES {
        return;
    }
    bpf::enroll(socket, core);
}

/// The per-core `ConnectionIdGenerator` (see the module docs). Also the place
/// the CID scheme lives for [`steer`]'s e2e test to decode independently.
///
/// [`steer`]: self
pub(crate) struct SteerCid {
    core: u8,
    rng: ring::rand::SystemRandom,
}

impl SteerCid {
    pub(crate) fn new(core: u32) -> Self {
        Self {
            core: core as u8,
            rng: ring::rand::SystemRandom::new(),
        }
    }

    /// Decode which core `cid` claims, the way the eBPF program would: the
    /// permutation position of `DCID_OBF ^ DCID_NONCE`.
    #[cfg(test)]
    fn decodes_to(cid: &[u8]) -> usize {
        permutation()
            .iter()
            .position(|obf| *obf == cid[CID_OBF] ^ cid[CID_NONCE])
            .expect("every obfuscation byte decodes")
    }
}

impl ConnectionIdGenerator for SteerCid {
    fn generate_cid(&mut self) -> ConnectionId {
        let mut cid = [0u8; CID_LEN];
        self.rng.fill(&mut cid).expect("system randomness");
        cid[CID_OBF] = permutation()[self.core as usize] ^ cid[CID_NONCE];
        ConnectionId::new(&cid)
    }

    fn cid_len(&self) -> usize {
        CID_LEN
    }

    fn cid_lifetime(&self) -> Option<std::time::Duration> {
        None
    }
}

/// The per-process secret permutation behind the core encoding: CID byte
/// [`CID_OBF`] is `permutation()[core] ^ CID_NONCE`. The XOR against a fresh
/// random nonce per CID is what keeps rotated CIDs of one connection from
/// sharing a constant, correlatable byte (RFC 9000 §5.1); the permutation is
/// what keeps two cores from colliding. The eBPF side inverts it through its
/// `rev` map.
fn permutation() -> &'static [u8; 256] {
    static PERMUTATION: OnceLock<[u8; 256]> = OnceLock::new();
    PERMUTATION.get_or_init(|| {
        let rng = ring::rand::SystemRandom::new();
        let mut perm = [0u8; 256];
        for (i, slot) in perm.iter_mut().enumerate() {
            *slot = i as u8;
        }
        // Fisher–Yates from a single batch of randomness.
        let mut randomness = [[0u8; 4]; 255];
        rng.fill(randomness.as_flattened_mut())
            .expect("system randomness");
        for i in (1..perm.len()).rev() {
            perm.swap(i, u32::from_le_bytes(randomness[i - 1]) as usize % (i + 1));
        }
        perm
    })
}

// The eBPF half only exists where eBPF does; elsewhere [`enroll`] is a no-op
// and dispatch stays the kernel hash.
#[cfg(target_os = "linux")]
mod bpf {
    use aya::{
        Ebpf,
        maps::{Array, MapData, ReusePortSockArray},
        programs::SkReuseport,
    };
    use log::{info, warn};
    use std::sync::Mutex;

    use super::permutation;

    /// The compiled steering program, produced by `build.rs`. On hosts
    /// without the BPF toolchain this is an empty placeholder, so [`setup`]
    /// fails cleanly and [`enroll`] reports steering off.
    const OBJECT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/kolorinko-steer"));

    /// Process-global program state. A `Mutex`, not a `OnceLock`: cores bind
    /// concurrently at startup, and each must land its own socket in the map.
    static STATE: Mutex<State> = Mutex::new(State::Untried);

    enum State {
        /// No core enrolled yet — the next enrollment loads the program.
        Untried,
        /// The program is attached; late enrollments only add map entries.
        Active(ReusePortSockArray<MapData>),
        /// Loading failed once (logged); don't retry per core.
        Off,
    }

    pub(super) fn enroll(socket: &(impl std::os::fd::AsFd + std::os::fd::AsRawFd), core: u32) {
        let mut state = STATE.lock().expect("steer state poisoned");
        if matches!(*state, State::Untried) {
            *state = match setup(socket) {
                Ok(socks) => {
                    info!("quic steering: program attached, CID dispatch active");
                    State::Active(socks)
                }
                Err(e) => {
                    warn!("quic steering unavailable ({e:#}); kernel hash dispatch stays");
                    State::Off
                }
            };
        }
        if let State::Active(socks) = &mut *state
            && let Err(e) = socks.set(core, socket, 0)
        {
            warn!("quic steering: core {core} not enrolled ({e}); it keeps hash dispatch");
        }
    }

    /// Load the program, fill the `rev` map with the permutation's inverse,
    /// and attach to `anchor`'s reuseport group — attaching through any group
    /// member covers the whole group, and the group holds the program's
    /// reference past this process' own fd.
    fn setup(
        anchor: &(impl std::os::fd::AsFd + std::os::fd::AsRawFd),
    ) -> anyhow::Result<ReusePortSockArray<MapData>> {
        if OBJECT.is_empty() {
            anyhow::bail!(
                "the embedded steer object is the build-time placeholder; rebuild with bpf-linker available (e.g. in the nix dev shell)"
            );
        }
        let mut bpf = Ebpf::load(OBJECT)?;
        let mut rev: Array<_, u8> = bpf
            .take_map("rev")
            .expect("steer object carries a rev map")
            .try_into()?;
        for (core, &obf) in permutation().iter().enumerate() {
            rev.set(obf as u32, core as u8, 0)?;
        }
        let program: &mut SkReuseport = bpf
            .program_mut("steer")
            .expect("steer object carries a steer program")
            .try_into()?;
        program.load()?;
        program.attach(anchor)?;
        Ok(bpf
            .take_map("socks")
            .expect("steer object carries a socks map")
            .try_into()?)
    }
}

#[cfg(not(target_os = "linux"))]
mod bpf {
    use std::os::fd::{AsFd, AsRawFd};

    pub(super) fn enroll(_socket: &(impl AsFd + AsRawFd), _core: u32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The CID the generator mints must decode — through the permutation,
    /// exactly the way the eBPF program inverts it — back to its core.
    #[test]
    fn cids_encode_their_core() {
        for core in [0, 1, 17, 63] {
            let cid = SteerCid::new(core).generate_cid();
            assert_eq!(cid.len(), CID_LEN);
            assert_eq!(SteerCid::decodes_to(&cid) as u32, core);
        }
    }

    /// The obfuscation must not degenerate into the constant-core-byte
    /// scheme: two CIDs of one connection share neither the nonce nor the
    /// obfuscated byte, so nothing in the CID is stable across rotation
    /// except the encoded core behind the secret permutation.
    #[test]
    fn cid_obfuscation_rotates() {
        let mut mint = SteerCid::new(3);
        let a = mint.generate_cid();
        let b = mint.generate_cid();
        assert_ne!(a[CID_NONCE], b[CID_NONCE]);
        assert_ne!(a[CID_OBF], b[CID_OBF]);
        // ...while still decoding to the same core.
        assert_eq!(SteerCid::decodes_to(&a), SteerCid::decodes_to(&b));
    }

    /// The permutation is a bijection — the eBPF `rev` map and the decoder
    /// above rely on every obfuscation byte identifying exactly one core.
    #[test]
    fn permutation_is_a_bijection() {
        let mut seen = [false; 256];
        for &obf in permutation() {
            assert!(!seen[obf as usize], "byte {obf} used twice");
            seen[obf as usize] = true;
        }
    }

    /// The whole steering path against the real kernel: two enrolled
    /// sockets, then a crafted 1-RTT datagram must land on the socket whose
    /// core its CID encodes, while a long-header datagram falls through to
    /// the kernel hash dispatch. Needs `CAP_BPF`, so it only runs when asked
    /// for explicitly (`cargo test -p kolorinko steer -- --ignored` as
    /// root).
    #[test]
    #[ignore = "needs CAP_BPF (run as root)"]
    fn steers_1rtt_packets_to_the_owning_core() {
        use std::net::UdpSocket;
        use std::time::Duration;

        // Warnings from `enroll` (load/attach failures, placeholders) are the
        // test's diagnostics — surface them on stderr.
        let _ = env_logger::builder()
            .filter_level(log::LevelFilter::Info)
            .try_init();

        let member = |bind: std::net::SocketAddr| {
            use socket2::{Domain, Protocol, Socket, Type};
            let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
            sock.set_reuse_port(true).unwrap();
            sock.bind(&bind.into()).unwrap();
            UdpSocket::from(sock)
        };
        let core0 = member("127.0.0.1:0".parse().unwrap());
        let port = core0.local_addr().unwrap().port();
        // The reuseport group IS the port: a second member must bind the
        // first one's resolved port — another ":0" would just allocate a
        // fresh, unrelated port.
        let core1 = member(format!("127.0.0.1:{port}").parse().unwrap());
        assert_eq!(core1.local_addr().unwrap().port(), port);
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();

        super::enroll(&core0, 0);
        super::enroll(&core1, 1);

        for sock in [&core0, &core1] {
            sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        }
        // A 1-RTT packet claiming core 1: short-header flags byte, then the
        // DCID's nonce and obfuscated core. The two DCID bytes come straight
        // from the generator — the packet must carry ITS nonce, not a local
        // one, since the decoder derives the key as obf ^ nonce.
        let cid = SteerCid::new(1).generate_cid();
        let mut packet = vec![0x40, cid[CID_NONCE], cid[CID_OBF]];
        packet.extend([0u8; 16]);
        sender.send_to(&packet, ("127.0.0.1", port)).unwrap();

        let mut landed = vec![0u8; packet.len()];
        let got = core1.recv(&mut landed).expect("core 1 misses its packet");
        assert_eq!(&landed[..got], packet.as_slice());

        // ...and core 0 must have received nothing.
        core0
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        assert!(
            core0.recv(&mut landed).is_err(),
            "core 0 got a packet encoded for core 1"
        );

        // A long-header packet has no owning core: it falls through to the
        // kernel hash dispatch, which always delivers it to exactly one
        // group member.
        sender.send_to(&[0xc0u8; 32], ("127.0.0.1", port)).unwrap();
        let mut anyone_received = false;
        for sock in [&core0, &core1] {
            sock.set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            anyone_received |= sock.recv(&mut landed).is_ok();
        }
        assert!(anyone_received, "long-header packet dropped");
    }
}
