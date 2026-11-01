//! The core's safe-API regressions: a misuse refuses honestly and
//! registers nothing — the world stays whole for the next well-formed
//! step. Pure sections, no runtime: the cache below sits idle.

use super::*;
use crate::disk::MemDisk;

fn core() -> Core<MemDisk> {
    Core::fresh(MemDisk::default().file("db"), 4, 2)
}

/// A fresh pool serves at once (the head, then the counter).
fn draft(c: &mut Core<MemDisk>) -> DraftId {
    match c.alloc_step() {
        Ok(AllocAct::Got(k)) => k,
        _ => panic!("a fresh pool serves at once"),
    }
}

/// A table index outside the fixed set is an honest refusal — never a
/// phantom root read.
#[test]
fn a_missing_table_refuses() {
    let mut c = core();
    assert!(c.get_step(2, b"k").is_none());
    assert!(c.get_step(0, b"k").is_some());
    let k = draft(&mut c);
    assert_eq!(
        c.seal_step(2, b"k", b"", &[k], &[]),
        Err(StoreError::NoTable)
    );
}

/// A seal that would double-list or forge a page refuses whole — and
/// registers nothing: no deaths, no table entry, the drafts stay held.
#[test]
fn a_bad_seal_refuses_whole() {
    let mut c = core();
    let a = draft(&mut c); // page 2
    let b = draft(&mut c); // page 4
    let sp = draft(&mut c); // page 6 — a spare kept for the pool check
    let why = |r| match r {
        Err(StoreError::BadSeal(w)) => w,
        other => panic!("a bad seal: {other:?}"),
    };
    assert_eq!(
        why(c.seal_step(0, b"k", b"", &[a], &[2])),
        "a page both consumed and freed"
    );
    assert_eq!(
        why(c.seal_step(0, b"k", b"", &[a], &[4])),
        "a freed page still held as a draft"
    );
    assert_eq!(
        why(c.seal_step(0, b"k", b"", &[a], &[7, 7])),
        "a page freed twice"
    );
    assert_eq!(
        why(c.seal_step(0, b"k", b"", &[a], &[999])),
        "a freed page outside the issued range"
    );
    assert_eq!(
        why(c.seal_step(0, b"k", b"", &[a], &[0])),
        "a freed page outside the issued range"
    );
    // nothing was registered by any refusal
    let ram = c.ram();
    assert_eq!((ram.dying, ram.delta, ram.drafts.len()), (0, 0, 3));
    assert_eq!(c.draft(&a), Some((2, true)));
    // a published page freed (the legit flow), then — a dying page, a
    // pooled page — refused alike
    let j = draft(&mut c); // page 8
    assert!(c.seal_step(0, b"j", b"", &[j], &[]).is_ok());
    let d = draft(&mut c); // page 10
    assert!(c.seal_step(0, b"k2", b"", &[d], &[8]).is_ok());
    let ram = c.ram();
    assert!(ram.held.contains(&8));
    let e = draft(&mut c);
    assert_eq!(
        why(c.seal_step(0, b"k3", b"", &[e], &[8])),
        "a freed page already dying"
    );
    c.dealloc_step(b); // page 4 back into its pool's head
    assert_eq!(
        why(c.seal_step(0, b"k4", b"", &[sp], &[4])),
        "a freed page back in a pool"
    );
    // a vanished draft — refused, and the good seal still goes through
    let v = draft(&mut c);
    c.dealloc_step(v);
    assert_eq!(
        c.seal_step(0, b"v", b"", &[v], &[]),
        Err(StoreError::ConsumedVanished)
    );
    assert!(c.seal_step(0, b"k", b"m", &[a], &[]).is_ok());
    assert!(c.pages_of(&[a]).is_none()); // consumed — gone from the arena
}

/// A draft dies once: an unknown key and a second death are honest
/// refusals (a double retire would list the page in a pool twice).
#[test]
fn a_draft_dies_once() {
    let mut c = core();
    let k = draft(&mut c);
    assert!(c.dealloc_step(k));
    assert!(!c.dealloc_step(k));
    assert!(!c.dealloc_step(DraftId(99)));
}

/// A seal over a taken pool is a bug caught loud (the round owns the
/// pool until its commit or rollback).
#[test]
#[should_panic(expected = "a seal over a taken pool")]
fn a_second_seal_is_caught() {
    let mut c = core();
    let _ = c.seal();
    let _ = c.seal();
}

/// A failed round's rollback restores the seal-time world whole — the
/// pools, the containers, the deaths pile, the drafts, the delta.
#[test]
fn unseal_restores_the_world() {
    let mut c = core();
    let a = draft(&mut c);
    assert!(c.seal_step(0, b"k", b"m", &[a], &[3]).is_ok()); // an in-range page dies
    let before = c.ram();
    let s = c.seal();
    assert!(c.round_live());
    c.unseal(s.ahead, s.atail, s.anext, s.spent, s.dying);
    assert!(!c.round_live());
    let after = c.ram();
    assert_eq!(after.heads, before.heads);
    assert_eq!(after.nexts, before.nexts);
    assert_eq!(after.tails, before.tails);
    assert_eq!(after.spent, before.spent);
    assert_eq!(after.held, before.held);
    assert_eq!(
        (after.dying, after.delta, after.drafts),
        (before.dying, before.delta, before.drafts)
    );
}
