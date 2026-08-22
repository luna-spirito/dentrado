//! The canonical addressing layer: opaque content-space and page-local
//! identifiers, plus canonical-route parsing.
//!
//! # The URL model
//! ```text
//! https://<host>/{space}/{local}[/decorative-slug]
//!                        │       └─ 11 chars base64url (8 bytes)
//!                        └─ 22 chars base64url (16 bytes)
//! ```
//! - **space** ([`SpaceId`]): 16 random-or-derived bytes identifying one
//!   content space (today: one Wikidot-export site; tomorrow: any gear-owned
//!   namespace). Registered in the server config; globally collision-safe
//!   (128-bit) so the identifier travels across a future federation without a
//!   coordinating registry.
//! - **local** ([`LocalId`]): 8 bytes identifying one page *within* its space.
//!   For Wikidot imports this is the exporter's numeric `page_id` — the one
//!   stable key Wikidot ever assigned (fullnames change on rename). Native
//!   spaces will use CSPRNG-64. Only birthday-safe, not attack-safe: each
//!   space has a server authority that rejects duplicates at insert.
//! - The optional third segment is a decorative slug: accepted for
//!   human-readable sharing, never parsed, and 301-redirected away to the
//!   canonical `/space/local` form.
//!
//! Both encodings are **canonical base64url without padding** with the
//! spare trailing bits required to be zero, and parse is strict (exact
//! length, exact alphabet, round-trip), so one identifier has exactly one
//! spelling — a URL either is canonical or does not parse.
//!
//! Reserved namespace: any path whose first segment starts with `-` (in
//! particular `/-/…`) is system space, never content (see [`SYSTEM_PREFIX`]).

use std::fmt;

use crate::{SafePathComponent, Slug};

/// Paths under `/-…` are the system namespace (assets served by the platform,
/// future APIs, static files — the GitLab `/-/` convention). Content
/// identifiers can never collide with it: a base64url segment never starts
/// with `-`… well, `-` *is* in the alphabet — but a canonical id segment has a
/// fixed length (22/11), so the check is done after canonical parse fails;
/// this constant marks the *reservation* in one place.
pub const SYSTEM_PREFIX: &str = "/-";

// ── base64url (no padding, strict) ──────────────────────────────────────────

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn sym_val(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// Encode `bytes` as unpadded base64url.
pub(crate) fn b64u_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).map_or(0, |&b| u32::from(b));
        let b2 = chunk.get(2).map_or(0, |&b| u32::from(b));
        let n = (b0 << 16) | (b1 << 8) | b2;
        let chars = [
            ALPHABET[((n >> 18) & 63) as usize] as char,
            ALPHABET[((n >> 12) & 63) as usize] as char,
            ALPHABET[((n >> 6) & 63) as usize] as char,
            ALPHABET[(n & 63) as usize] as char,
        ];
        let take = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        out.extend(&chars[..take]);
    }
    out
}

/// Strict unpadded base64url decode: rejects non-alphabet bytes, impossible
/// lengths (`len % 4 == 1`), and non-zero spare bits in the final partial
/// group (so a fixed-size id has exactly one valid spelling).
pub(crate) fn b64u_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4 + 1);
    for chunk in bytes.chunks(4) {
        // A group of `l` chars packs `6*l` bits, most-significant char first;
        // only the leading `8*l - 2*(4-l) - …` — in short: chars shift by
        // `6*(l-1-i)`, unlike a full group's `6*(3-i)`.
        let l = chunk.len();
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            n |= u32::from(sym_val(c)?) << (6 * (l - 1 - i));
        }
        match l {
            4 => {
                out.push((n >> 16) as u8);
                out.push((n >> 8) as u8);
                out.push(n as u8);
            }
            // 3 chars → 16 payload bits + 2 spare; 2 chars → 8 bits + 4 spare.
            3 => {
                if n & 0b11 != 0 {
                    return None;
                }
                out.push((n >> 10) as u8);
                out.push((n >> 2) as u8);
            }
            2 => {
                if n & 0b1111 != 0 {
                    return None;
                }
                out.push((n >> 4) as u8);
            }
            _ => unreachable!(),
        }
    }
    Some(out)
}

// ── SpaceId ─────────────────────────────────────────────────────────────────

/// A content-space identifier: 16 opaque bytes, canonically 22 base64url
/// chars. Collision-safe against the whole internet (128 bits), so it needs
/// no global registry to stay unique — a future p2p/federated deployment
/// replicates the mapping, never mints it.
///
/// # Derivation (recommendations, not enforced — the id is opaque)
/// The id should be **deterministically derived**, so independent operators
/// converge on the same address without coordination, but *what* it derives
/// from depends on who owns the content:
/// - **imported / read-only spaces** (Wikidot exports): derive from the
///   source, not from any operator's key —
///   `SHA-256("dentrado/space/v1" ‖ "wikidot-export" ‖ site)[0..16]`.
///   Whoever mirrors the export gets the same id (fork navigation = segment
///   swap), and no key compromise or loss can orphan the id.
/// - **owned spaces** (original content, future writes): derive from the
///   owner's signing key so future signatures can prove authority —
///   `SHA-256("dentrado/space/v1" ‖ pubkey ‖ label)[0..16]` (ed25519).
///   The `label` is mandatory: without it every space of one key collapses
///   into the same id. The private key never participates in derivation and
///   never leaves the owner; only the 32-byte public key is published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, dentrado_types::Localizable)]
#[localizable(skip)]
pub struct SpaceId([u8; 16]);

impl SpaceId {
    /// The canonical encoding length in characters.
    pub const LEN: usize = 22;

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Parse the canonical 22-char form. Strict: wrong length, wrong alphabet,
    /// or non-zero padding bits all fail (so re-encoding any parsed value
    /// reproduces the input byte-for-byte).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let bytes = b64u_decode(s)?;
        let bytes: [u8; 16] = bytes.try_into().ok()?;
        (s.len() == Self::LEN).then_some(Self(bytes))
    }

    /// The canonical 22-char spelling.
    #[must_use]
    pub fn as_str(&self) -> String {
        b64u_encode(&self.0)
    }
}

impl fmt::Display for SpaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

// Serialize as the canonical string form (wire-friendly: the client sees ids
// the way URLs spell them); deserialize through `parse` so a client-supplied
// frame can never smuggle a non-canonical spelling past validation.
impl serde::Serialize for SpaceId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for SpaceId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?)
            .ok_or_else(|| serde::de::Error::custom("invalid space id (want 22-char base64url)"))
    }
}

// ── LocalId ─────────────────────────────────────────────────────────────────

/// A page identifier *within* one space: 8 bytes, canonically 11 base64url
/// chars. Wikidot imports carry the exporter's numeric `page_id` here (stable
/// across renames — the fullname is not); native spaces will mint CSPRNG-64.
/// Uniqueness is per-space and birthday-safe only: each space's authority
/// rejects duplicates at insert, so targeted collisions are out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, dentrado_types::Localizable)]
#[localizable(skip)]
pub struct LocalId(u64);

impl LocalId {
    /// The canonical encoding length in characters.
    pub const LEN: usize = 11;

    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Parse a wikidot-export `page_id` string (decimal) into a local id.
    #[must_use]
    pub fn from_page_id(s: &str) -> Option<Self> {
        s.parse::<u64>().ok().map(Self)
    }

    /// Parse the canonical 11-char form (strict, like [`SpaceId::parse`]).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let bytes = b64u_decode(s)?;
        let bytes: [u8; 8] = bytes.try_into().ok()?;
        (s.len() == Self::LEN).then_some(Self(u64::from_be_bytes(bytes)))
    }
}

impl fmt::Display for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&b64u_encode(&self.0.to_be_bytes()))
    }
}

impl serde::Serialize for LocalId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for LocalId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?)
            .ok_or_else(|| serde::de::Error::custom("invalid local id (want 11-char base64url)"))
    }
}

// ── route parsing ───────────────────────────────────────────────────────────

/// A resolved content address: which dataset site serves the space, and the
/// page's slug within it. The bridge between the canonical URL layer and the
/// slug-keyed gears (`article_latest`, `shell`, …).
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, dentrado_types::Localizable,
)]
pub struct PageAddr {
    pub site: SafePathComponent,
    pub slug: Slug,
}

/// `/{space}/{local}` (exactly two segments) → the canonical page address.
/// `None` for anything else — a trailing slash, a third decorative-slug
/// segment (the server answers those with a 301; no client should ever
/// subscribe under one), a bare space, or a legacy site path. Shared by the
/// server's SSR dispatch and the web client's router, so both agree on what
/// a canonical route is.
#[must_use]
pub fn parse_canonical(path: &str) -> Option<(SpaceId, LocalId)> {
    let mut segs = path.trim_start_matches('/').split('/');
    let space = SpaceId::parse(segs.next()?)?;
    let local = LocalId::parse(segs.next()?)?;
    segs.next().is_none().then_some((space, local))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> SpaceId {
        // SHA-256("dentrado/space/v1\0wikidot-export\0obscurative")[0..16]
        SpaceId::parse("I5Xee8HsV1zTMRChatxfiw").unwrap()
    }

    #[test]
    fn codec_round_trips() {
        for len in 0..=32 {
            let bytes: Vec<u8> = (0..len as u8).map(|i| i.wrapping_mul(37)).collect();
            let enc = b64u_encode(&bytes);
            assert_eq!(b64u_decode(&enc).unwrap(), bytes, "len {len}");
        }
    }

    #[test]
    fn space_id_is_strict() {
        let s = space();
        assert_eq!(s.as_str().len(), 22);
        assert_eq!(SpaceId::parse(&s.as_str()), Some(s));
        // Wrong length.
        assert!(SpaceId::parse(&s.as_str()[..21]).is_none());
        assert!(SpaceId::parse(&format!("{}x", s.as_str())).is_none());
        // Standard base64 (+, /, =) rejected.
        assert!(SpaceId::parse("AAAAAAAAAAAAAAAAAAAAAA=").is_none());
        assert!(SpaceId::parse("AAAAAAAAAAAAAAAAAAAAA+AA").is_none());
        // Non-zero spare bits in the final char rejected (…AB carries bits).
        assert!(SpaceId::parse("AAAAAAAAAAAAAAAAAAAAAB").is_none());
    }

    #[test]
    fn local_id_is_strict_and_carries_page_ids() {
        let l = LocalId::from_page_id("1305054470").unwrap();
        assert_eq!(l.as_u64(), 1_305_054_470);
        let enc = l.to_string();
        assert_eq!(enc.len(), 11);
        assert_eq!(LocalId::parse(&enc), Some(l));
        assert!(LocalId::parse(&enc[..10]).is_none());
        assert!(LocalId::parse("AAAAAAAAAAA").is_some()); // page id 0
        assert!(LocalId::parse("AAAAAAAAAAB").is_none()); // spare bits set
        assert!(LocalId::from_page_id("not-a-number").is_none());
    }

    #[test]
    fn canonical_routes_parse() {
        let (sp, lo) = parse_canonical("/I5Xee8HsV1zTMRChatxfiw/AAAAAE3JjQY").unwrap();
        assert_eq!(sp, space());
        assert_eq!(lo.as_u64(), 1_305_054_470);
        // Strict: trailing slash, slug segment, bare space, legacy paths.
        assert!(parse_canonical("/I5Xee8HsV1zTMRChatxfiw/AAAAAE3JjQY/").is_none());
        assert!(parse_canonical("/I5Xee8HsV1zTMRChatxfiw/AAAAAE3JjQY/slag").is_none());
        assert!(parse_canonical("/I5Xee8HsV1zTMRChatxfiw").is_none());
        assert!(parse_canonical("/").is_none());
        // Legacy site paths do not parse as canonical.
        assert!(parse_canonical("/obscurative/syntax").is_none());
    }

    #[test]
    fn page_addr_serializes() {
        let addr = PageAddr {
            site: SafePathComponent::new("obscurative".into()).unwrap(),
            slug: (None, SafePathComponent::new("syntax".into()).unwrap()),
        };
        let json = serde_json::to_string(&addr).unwrap();
        assert_eq!(serde_json::from_str::<PageAddr>(&json).unwrap(), addr);
    }
}
