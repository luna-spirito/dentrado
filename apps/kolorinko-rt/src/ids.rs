//! The canonical addressing layer: opaque content-space and page-local
//! identifiers, page-route parsing, and title-segment shaping.
//!
//! # The URL model
//! ```text
//! https://<host>/{space}/{local}[/title]
//!                │       └─ 'L' + 11 chars base64url (64 bits)
//!                └─ 'S' + 22 chars base64url (128 bits)
//! ```
//! - **space** ([`SpaceId`]): 16 derived-or-random bytes identifying one
//!   content space (today: one Wikidot-export site; tomorrow: any gear-owned
//!   namespace). Registered in the server config; collision-safe (all 128
//!   payload bits) so the identifier travels across a future federation
//!   without a coordinating registry.
//! - **local** ([`LocalId`]): 8 bytes identifying one page *within* its space.
//!   For Wikidot imports this is the exporter's numeric `page_id` — the one
//!   stable key Wikidot ever assigned (fullnames change on rename). Native
//!   spaces will use CSPRNG-64. Only birthday-safe, not attack-safe: each
//!   space has a server authority that rejects duplicates at insert.
//! - The optional third segment is a human-readable **title** derived from the
//!   page's title ([`title_slug`]): accepted for sharing, never parsed, and
//!   regenerated on redirects.
//!
//! # The marker char
//! Every id's canonical form is `'S'/'L' ‖ base64url(payload)`: one literal
//! uppercase prefix character, then the raw payload at its full width. The
//! prefix is chosen from **outside the slug alphabet**: slugs are lowercase
//! by construction (Wikidot normalizes imported page names to lowercase,
//! [`title_slug`] lowercases, and native spaces mint lowercase), and URL
//! path matching is case-sensitive — so a segment starting with an
//! uppercase letter can *never* be a slug, and a slug can *never* parse as
//! an id. This is what makes URL dispatch purely syntactic: no page name —
//! however word-like or adversarial — can shadow a canonical route, and no
//! id can swallow a page's slug URL. (`adventurous`, `wonderments`, any
//! 11-char name: all unambiguously slugs.)
//!
//! A marker *bit* cannot do this job: any bit-layout inside the shared
//! base64url alphabet leaves exactly ¼ of 11-char strings parseable as ids
//! (upper-half first char × zero spare bit), and real words live in that
//! quarter — the prefix char spends one character instead and gets
//! certainty.
//!
//! The prefix also keeps ids out of every other reserved corner: an id
//! never starts with `-` (so the whole `/-…` namespace is free for the
//! system) and always eyeball-distinct from names. Base64url without
//! padding is canonical (spare trailing bits required zero, parse strict on
//! length/alphabet/round-trip), so one identifier has exactly one spelling.
//!
//! Reserved namespace: paths under `/-…` are system space, never content
//! (see [`SYSTEM_PREFIX`]); ids start with `S`/`L`, never `-`.

use std::fmt;

/// Paths under `/-…` are the system namespace (mirrored content-addressed
/// blobs, future platform APIs — the GitLab `/-/` convention). Content ids
/// start with `S`/`L` and can never collide with it.
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
fn b64u_encode(bytes: &[u8]) -> String {
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
fn b64u_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4 + 1);
    for chunk in bytes.chunks(4) {
        // A group of `l` chars packs `6*l` bits, most-significant char first;
        // chars shift by `6*(l-1-i)`, unlike a full group's `6*(3-i)`.
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

/// A content-space identifier: 16 opaque payload bytes, canonically
/// `'S' ‖ base64url(payload)` — 23 chars (see the module's
/// [marker char](self#the-marker-char) section; the marker lives only in
/// the string form — the bytes are raw). Collision-safe against the whole
/// internet (128 payload bits), so it needs no global registry to stay
/// unique — a future p2p/federated deployment replicates the mapping, never
/// mints it.
///
/// # Derivation (recommendations, not enforced — the id is opaque)
/// The id should be **deterministically derived**, so independent operators
/// converge on the same address without coordination, but *what* it derives
/// from depends on who owns the content:
/// - **imported / read-only spaces** (Wikidot exports): derive from the
///   source, not from any operator's key —
///   `SHA-256("wikidot-evakuilo/v1" ‖ site)[0..16]`, wrapped raw. Whoever
///   mirrors the export gets the same id (fork navigation =
///   segment swap), and no key compromise or loss can orphan the id.
/// - **owned spaces** (original content, future writes): derive from the
///   owner's signing key so future signatures can prove authority —
///   `SHA-256("dentrado/space/v1" ‖ pubkey ‖ label)[0..16]` (ed25519),
///   wrapped raw. The `label` is mandatory: without it every space of one key
///   collapses into the same id. The private key never participates in
///   derivation and never leaves the owner; only the 32-byte public key is
///   published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, dentrado_types::Localizable)]
#[localizable(skip)]
pub struct SpaceId([u8; 16]);

impl SpaceId {
    /// The marker char every canonical spelling starts with.
    pub const PREFIX: char = 'S';

    /// The canonical encoding length in characters (prefix + 22).
    pub const LEN: usize = 23;

    /// Wrap raw payload bytes as-is (the marker is a property of the string
    /// encoding, not of the value).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Parse the canonical 23-char form (`'S'` + 22). Strict: wrong prefix,
    /// wrong length, wrong alphabet, or non-zero padding bits all fail (so
    /// re-encoding any parsed value reproduces the input byte-for-byte).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix(Self::PREFIX)?;
        if s.len() != Self::LEN {
            return None;
        }
        let bytes: [u8; 16] = b64u_decode(rest)?.try_into().ok()?;
        Some(Self(bytes))
    }

    /// The canonical 23-char spelling.
    #[must_use]
    pub fn as_str(&self) -> String {
        format!("{}{}", Self::PREFIX, b64u_encode(&self.0))
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
        Self::parse(&String::deserialize(d)?).ok_or_else(|| {
            serde::de::Error::custom("invalid space id (want S + 22-char base64url)")
        })
    }
}

// ── LocalId ─────────────────────────────────────────────────────────────────

/// A page identifier *within* one space: 8 raw payload bytes, canonically
/// `'L' ‖ base64url(payload)` — 12 chars (see the module's
/// [marker char](self#the-marker-char) section; the marker lives only in
/// the string form). Wikidot imports carry the exporter's numeric `page_id`
/// here (stable across renames — the fullname is not); native spaces will
/// mint CSPRNG-64. Uniqueness is per-space and birthday-safe only: each
/// space's authority rejects duplicates at insert, so targeted collisions
/// are out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, dentrado_types::Localizable)]
#[localizable(skip)]
pub struct LocalId(u64);

impl LocalId {
    /// The marker char every canonical spelling starts with.
    pub const PREFIX: char = 'L';

    /// The canonical encoding length in characters (prefix + 11).
    pub const LEN: usize = 12;

    /// Wrap a page number as-is (the marker is added by the string encoding;
    /// the payload keeps all 64 bits).
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// The wrapped page number — the wikidot `page_id` for imported spaces.
    #[must_use]
    pub const fn page_id(&self) -> u64 {
        self.0
    }

    /// Parse a wikidot-export `page_id` string (decimal) into a local id.
    #[must_use]
    pub fn from_page_id(s: &str) -> Option<Self> {
        s.parse::<u64>().ok().map(Self::new)
    }

    /// Parse the canonical 12-char form (`'L'` + 11; strict, like
    /// [`SpaceId::parse`]).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix(Self::PREFIX)?;
        if s.len() != Self::LEN {
            return None;
        }
        let bytes: [u8; 8] = b64u_decode(rest)?.try_into().ok()?;
        Some(Self(u64::from_be_bytes(bytes)))
    }
}

impl fmt::Display for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", Self::PREFIX, b64u_encode(&self.0.to_be_bytes()))
    }
}

impl serde::Serialize for LocalId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for LocalId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?).ok_or_else(|| {
            serde::de::Error::custom("invalid local id (want L + 11-char base64url)")
        })
    }
}

// ── route parsing ───────────────────────────────────────────────────────────

/// `/{space}/{local}[/title]` → the canonical page address. The optional
/// third segment is a decorative title: accepted (so shared pretty URLs keep
/// working) but never inspected. Anything else — a trailing slash, a fourth
/// segment, a bare space, a legacy site path — is `None`. Shared by the
/// server's SSR dispatch and the web client's router, so both agree on what
/// a canonical route is.
#[must_use]
pub fn parse_page_route(path: &str) -> Option<(SpaceId, LocalId)> {
    let mut segs = path.trim_start_matches('/').split('/');
    let space = SpaceId::parse(segs.next()?)?;
    let local = LocalId::parse(segs.next()?)?;
    title_only(segs).then_some((space, local))
}

/// `/{local}[/title]` → the space-less page address of a wiki served on its
/// own configured domain: there the `Host` already names the space, so the
/// path needn't (the server routes it per-host; the client pairs it with
/// [`crate::DEFAULT_SPACE_GLOBAL`]). The same shape as
/// [`parse_page_route`] minus the space segment — the 'L' marker keeps it
/// disjoint from every slug family a wiki domain serves.
#[must_use]
pub fn parse_local_route(path: &str) -> Option<LocalId> {
    let mut segs = path.trim_start_matches('/').split('/');
    let local = LocalId::parse(segs.next()?)?;
    title_only(segs).then_some(local)
}

/// What may follow a route's id segments: at most one non-empty decorative
/// title segment, nothing more.
fn title_only(segs: std::str::Split<'_, char>) -> bool {
    let mut segs = segs;
    match segs.next() {
        None => true,
        Some(title) if !title.is_empty() && segs.next().is_none() => true,
        Some(_) => false,
    }
}

/// The inverse of [`parse_page_route`] where it matters: the canonical
/// page URL `/[/SPACE]/LOCAL/TITLE` with the title slug-shaped and
/// percent-encoded — the form a slug redirect lands on and `og:url` names.
/// Omit the space segment when the origin already names the space (a wiki
/// served on its own configured domain, where paths carry no `S…` segment).
#[must_use]
pub fn format_page_route(space: Option<SpaceId>, local: LocalId, title: &str) -> String {
    let title = encode_path_segment(&title_slug(title));
    match space {
        Some(s) => format!("/{s}/{local}/{title}"),
        None => format!("/{local}/{title}"),
    }
}

/// A path's short form against `default` — the space a wiki's own domain
/// already names: the `/{default}` segment drops (`/{d}/rest` → `/rest`,
/// `/{d}` → `/`), everything else passes verbatim (other spaces, slug
/// families, assets — the id's fixed width keeps the prefix from bleeding
/// into anything else). The single simplifier of the system: the server
/// always emits full-weight links, and the client — address bar, pushed
/// navigations, every href it builds or hydrates — shortens them against
/// its origin's default space ([`crate::DEFAULT_SPACE_GLOBAL`]).
#[must_use]
pub fn simplify(default: Option<SpaceId>, path: &str) -> String {
    let Some(d) = default else {
        return path.to_string();
    };
    match path.strip_prefix(&format!("/{d}")) {
        Some("") => "/".to_string(),
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        _ => path.to_string(),
    }
}

// ── title shaping ───────────────────────────────────────────────────────────

/// Shape a page title into the decorative third URL segment: whitespace →
/// `-`, alphanumeric characters kept (lowercased, Unicode-aware), `-`/`_`
/// kept, everything else dropped, leading/trailing `-` trimmed. Empty results
/// (an untitled page) fall back to `page`. Never parses back into anything —
/// the segment exists for humans, redirects regenerate it.
#[must_use]
pub fn title_slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    // Collapse runs: each whitespace char is one '-', but adjacent ones (or
    // one next to a literal dash) don't stack — "a  -  b" is "a-b".
    let mut prev_dash = false;
    for ch in title.chars() {
        if ch.is_whitespace() {
            if !prev_dash {
                out.push('-');
                prev_dash = true;
            }
        } else if ch.is_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if (ch == '-' || ch == '_') && !prev_dash {
            out.push(ch);
            prev_dash = ch == '-';
        }
    }
    while out.starts_with('-') {
        out.remove(0);
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Percent-encode a path segment (RFC 3986 unreserved characters kept as-is).
/// The redirect `Location` and the client's `pushState` both go through this,
/// so a title segment has exactly one spelling everywhere.
#[must_use]
pub fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(b));
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256("wikidot-evakuilo/v1/obscurative")[0..16], S-prefixed —
    /// the id the dev config's `ensure-evakuilo-sites` derives for its
    /// `"obscurative"` key.
    fn space() -> SpaceId {
        SpaceId::parse("S70P6lbBZxbc-kcpGOCYmZA").unwrap()
    }

    #[test]
    fn format_page_route_roundtrips() {
        // `LAAAAADXVfyo` is a-109's local id — a real spelling, not
        // `LocalId::new`, so the expected strings below are wire forms,
        // not this test's own formatting echoed back.
        let local = LocalId::parse("LAAAAADXVfyo").unwrap();
        let url = format_page_route(Some(space()), local, "A-109/108");
        assert_eq!(url, "/S70P6lbBZxbc-kcpGOCYmZA/LAAAAADXVfyo/a-109108");
        assert_eq!(parse_page_route(&url), Some((space(), local)));
        // The space-less form (a wiki's own domain) carries no segment to
        // parse back — only the titled shape is shared.
        assert_eq!(
            format_page_route(None, local, "Затерянные"),
            "/LAAAAADXVfyo/%D0%B7%D0%B0%D1%82%D0%B5%D1%80%D1%8F%D0%BD%D0%BD%D1%8B%D0%B5"
        );
    }

    #[test]
    fn b64u_codec_round_trips() {
        for len in 0..=32 {
            let bytes: Vec<u8> = (0..len as u8).map(|i| i.wrapping_mul(37)).collect();
            let enc = b64u_encode(&bytes);
            assert_eq!(b64u_decode(&enc).unwrap(), bytes, "len {len}");
        }
    }

    #[test]
    fn space_id_is_strict() {
        let s = space();
        assert_eq!(s.as_str().len(), 23);
        assert_eq!(SpaceId::parse(&s.as_str()), Some(s));
        // The marker char shows: every id starts with its uppercase prefix.
        assert!(s.as_str().starts_with('S'));
        // The bytes are the raw payload — no marker inside, from_bytes is a
        // plain wrap.
        assert_eq!(SpaceId::from_bytes(*s.as_bytes()), s);
        // Wrong length.
        assert!(SpaceId::parse(&s.as_str()[..22]).is_none());
        assert!(SpaceId::parse(&format!("{}x", s.as_str())).is_none());
        // Missing marker char rejected (lowercase prefix is a slug's shape).
        assert!(SpaceId::parse(&format!("s{}", &s.as_str()[1..])).is_none());
        // Standard base64 (+, /, =) rejected.
        assert!(SpaceId::parse("SAAAAAAAAAAAAAAAAAAAAA=").is_none());
        assert!(SpaceId::parse("SAAAAAAAAAAAAAAAAAAAA+AA").is_none());
        // Non-zero spare bits in the final char rejected (…AB carries a bit).
        assert!(SpaceId::parse("SAAAAAAAAAAAAAAAAAAAAAB").is_none());
    }

    #[test]
    fn local_id_is_strict_and_carries_page_ids() {
        let l = LocalId::from_page_id("986050317").unwrap();
        assert_eq!(l.page_id(), 986_050_317);
        assert_eq!(l.to_string(), "LAAAAADrF7w0");
        assert!(LocalId::parse("LAAAAADrF7w0").is_some());
        assert!(LocalId::parse(&l.to_string()[..11]).is_none());
        assert!(LocalId::parse("lAAAAADrF7w0").is_none()); // marker char missing
        assert!(LocalId::parse("LAAAAAAAAAB").is_none()); // spare bits set

        // Word-shaped names never parse — the whole point of the marker char:
        // a marker *bit* would let exactly ¼ of 11-char strings through
        // (`wonderments` among them); an uppercase prefix lets none.
        assert!(LocalId::parse("wonderments").is_none());
        assert!(LocalId::parse("adventurous").is_none());
        assert!(LocalId::from_page_id("not-a-number").is_none());
    }

    #[test]
    fn page_routes_parse() {
        let (sp, lo) = parse_page_route("/S70P6lbBZxbc-kcpGOCYmZA/LAAAAADrF7w0").unwrap();
        assert_eq!(sp, space());
        assert_eq!(lo.page_id(), 986_050_317);
        // The decorative title rides along unparsed.
        let (sp2, lo2) =
            parse_page_route("/S70P6lbBZxbc-kcpGOCYmZA/LAAAAADrF7w0/%D1%82%D0%B5%D0%BD%D1%8C")
                .unwrap();
        assert_eq!((sp2, lo2), (sp, lo));
        // Strict: trailing slash, a fourth segment, bare space, junk, legacy.
        assert!(parse_page_route("/S70P6lbBZxbc-kcpGOCYmZA/LAAAAADrF7w0/").is_none());
        assert!(parse_page_route("/S70P6lbBZxbc-kcpGOCYmZA/LAAAAADrF7w0/a/b").is_none());
        assert!(parse_page_route("/S70P6lbBZxbc-kcpGOCYmZA").is_none());
        assert!(parse_page_route("/").is_none());
        assert!(parse_page_route("/obscurative/syntax").is_none());
    }

    #[test]
    fn simplify_drops_default_space_segment() {
        let d = SpaceId::parse("S70P6lbBZxbc-kcpGOCYmZA").unwrap();
        let s = |p: &str| simplify(Some(d), p);
        // The default space's segment drops — the root to `/`.
        assert_eq!(s("/S70P6lbBZxbc-kcpGOCYmZA"), "/");
        assert_eq!(
            s("/S70P6lbBZxbc-kcpGOCYmZA/LAAAAADrF7w0/page"),
            "/LAAAAADrF7w0/page"
        );
        assert_eq!(s("/S70P6lbBZxbc-kcpGOCYmZA/docs:guide"), "/docs:guide");
        // Everything else passes verbatim: other spaces, slug families,
        // assets, the about screen, absolute URLs.
        assert_eq!(
            s("/SNhwIJuhsyCE-mxtJJE6aWg/LAAAAADrF7w0"),
            "/SNhwIJuhsyCE-mxtJJE6aWg/LAAAAADrF7w0"
        );
        assert_eq!(s("/docs:guide"), "/docs:guide");
        assert_eq!(s("/~/about"), "/~/about");
        assert_eq!(s("https://x.example/a"), "https://x.example/a");
        // No default: the server side, always full-weight.
        assert_eq!(
            simplify(None, "/S70P6lbBZxbc-kcpGOCYmZA/LAAAAADrF7w0"),
            "/S70P6lbBZxbc-kcpGOCYmZA/LAAAAADrF7w0"
        );
    }

    #[test]
    fn local_routes_parse() {
        // The wiki's-own-domain family: `/L…[/title]` names the default
        // space — the same strictness minus the space segment.
        assert_eq!(
            parse_local_route("/LAAAAADrF7w0").map(|l| l.page_id()),
            Some(986_050_317)
        );
        assert_eq!(
            parse_local_route("/LAAAAADrF7w0/title").map(|l| l.page_id()),
            Some(986_050_317)
        );
        // A space segment, a trailing slash, too deep, bare root, a slug.
        assert!(parse_local_route("/S70P6lbBZxbc-kcpGOCYmZA/LAAAAADrF7w0").is_none());
        assert!(parse_local_route("/LAAAAADrF7w0/").is_none());
        assert!(parse_local_route("/LAAAAADrF7w0/a/b").is_none());
        assert!(parse_local_route("/").is_none());
        assert!(parse_local_route("/some-page").is_none());
    }

    #[test]
    fn titles_shape_into_segments() {
        assert_eq!(title_slug("Тень подъезда"), "тень-подъезда");
        assert_eq!(title_slug("Hello, World!"), "hello-world");
        assert_eq!(title_slug("  spaced  out  "), "spaced-out");
        assert_eq!(title_slug("???"), "page");
        assert_eq!(
            encode_path_segment("тень-подъезда"),
            "%D1%82%D0%B5%D0%BD%D1%8C-%D0%BF%D0%BE%D0%B4%D1%8A%D0%B5%D0%B7%D0%B4%D0%B0"
        );
        assert_eq!(encode_path_segment("plain-1.2_~"), "plain-1.2_~");
    }
}
