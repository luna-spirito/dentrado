//! QUIC connection-ID steering for kolorinko's per-core QUIC endpoints.
//!
//! The userspace half lives in [`kolorinko::steer`]: it mints connection IDs
//! that encode the owning core and enrolls each core's QUIC socket in the
//! `socks` map below. This program runs on every datagram arriving at the
//! shared `SO_REUSEPORT` port and, for 1-RTT packets, steers the datagram to
//! the socket owning the packet's connection — so a client whose UDP flow got
//! remapped by its NAT still reaches the core holding the connection state,
//! and QUIC path validation migrates the connection instead of dropping it
//! (the plain 4-tuple hash would land the packet on a foreign core, whose
//! stateless reset kills the session — see `server.rs`).
//!
//! Packet anatomy, from the start of the QUIC packet (that is, `ctx.data()`
//! plus the UDP header): byte 0 is the flags byte, where a clear bit `0x80`
//! marks a short (1-RTT) header whose fixed-length DCID follows right after.
//! The DCID's first two bytes carry the core encoding — `nonce`, then
//! `perm[core] ^ nonce` — see `kolorinko::steer` for the generator and the
//! rationale. Everything else falls through with `SK_PASS` and no selection,
//! which the kernel treats as "no BPF verdict": long headers (Initial and
//! handshake packets have no owning core yet) and undecodable cores get the
//! default 4-tuple-hash dispatch, exactly as without this program.

#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{map, sk_reuseport},
    maps::{Array, ReusePortSockArray},
    programs::SkReuseportContext,
};

/// The `SK_REUSEPORT` verdicts (`include/uapi/linux/bpf.h`). Returning
/// `SK_PASS` without calling `select_reuseport` keeps the kernel's own hash
/// dispatch.
const SK_PASS: u32 = 1;

/// The directly accessible data starts at the UDP header (8 bytes); the QUIC
/// packet follows it.
const UDP_HEADER: usize = 8;
/// Flags-byte bit distinguishing long headers (set) from short ones (clear).
const LONG_HEADER: u8 = 0x80;
/// The DCID starts right after the flags byte. Its first byte is the nonce,
/// its second the obfuscated core — offsets from the start of the QUIC packet
/// (`ctx.data()` plus the UDP header).
const DCID_NONCE: usize = 1;
const DCID_OBF: usize = 2;
/// The deepest packet byte this program touches: `DCID_OBF`'s offset.
const LAST_READ: usize = DCID_OBF;

/// Cores beyond the map's capacity never match a `socks` entry, so their
/// packets fall through to the kernel hash dispatch — dispatch degrades for
/// oversized machines, never breaks.
const MAX_CORES: u32 = 64;

/// `rev[perm[core]] = core`: the userspace half fills this with the inverse
/// of its secret per-process permutation, undoing the CID's obfuscation in
/// one lookup.
#[map(name = "rev")]
static REV: Array<u8> = Array::with_max_entries(256, 0);

/// Core id → enrolled QUIC socket, filled in by the userspace half as cores
/// bind their endpoints.
#[map(name = "socks")]
static SOCKS: ReusePortSockArray = ReusePortSockArray::with_max_entries(MAX_CORES, 0);

/// Reads one directly-accessible packet byte. The caller must bound the
/// offset against `ctx.data_end()` first, so the verifier can prove the
/// access in-bounds.
fn byte(at: usize) -> u8 {
    unsafe { *(at as *const u8) }
}

#[sk_reuseport]
fn steer(ctx: SkReuseportContext) -> u32 {
    let quic = ctx.data() + UDP_HEADER;
    if quic + LAST_READ >= ctx.data_end() {
        return SK_PASS;
    }
    // Only short headers belong to established, core-owned connections.
    if byte(quic) & LONG_HEADER != 0 {
        return SK_PASS;
    }
    let key = (byte(quic + DCID_OBF) ^ byte(quic + DCID_NONCE)) as u32;
    if let Some(&core) = REV.get(key) {
        // A failed select (core not enrolled, or beyond `MAX_CORES`) leaves
        // nothing selected: `SK_PASS` → kernel hash dispatch.
        let _ = SOCKS.select_reuseport(&ctx, core as u32);
    }
    SK_PASS
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
