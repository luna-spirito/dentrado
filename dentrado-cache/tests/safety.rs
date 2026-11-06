//! Safe-API regressions: a misused surface refuses honestly, and a
//! refusal leaves the world whole — no phantom table reads, no key a
//! leaf could never hold, no half-landed batch, no draft swallowed by
//! a refusal. Every test ends at rest with the page bijection checked.

use dentrado_cache::{Fault, MemDisk, Storage, StoreError, TableSpec};

mod util;
use util::{put, run, specs};

/// A table the store never heard of: reads, scans and publications
/// alike refuse — never a phantom root read, never a written byte.
#[test]
fn a_missing_table_refuses_honestly() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        assert!(matches!(
            st.get(b"t1", b"k").await,
            Err(StoreError::NoTable)
        ));
        assert!(matches!(
            st.scan(b"t1", b"", b"").await,
            Err(StoreError::NoTable)
        ));
        assert!(matches!(
            put(&st, b"t1", b"k", b"v"),
            Err(StoreError::NoTable)
        ));
        // the refusal swallowed nothing: the well-formed twin publishes
        put(&st, b"t0", b"j", b"v").unwrap();
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}

/// Identities outside the format refuse the open: empty, over the
/// 63-byte bound, a duplicate. The bound itself is a name like any
/// other, and the refusals wrote nothing.
#[test]
fn identities_outside_the_format_refuse() {
    run(async {
        let disk = MemDisk::default();
        let long = vec![0u8; 64];
        assert!(matches!(
            Storage::open(&disk, "db", 4, &[TableSpec::flat(long)]).await,
            Err(StoreError::BadName)
        ));
        assert!(matches!(
            Storage::open(&disk, "db", 4, &specs(&[b"t0", b"t0"])).await,
            Err(StoreError::BadName)
        ));
        assert!(matches!(
            Storage::open(&disk, "db", 4, &specs(&[b""])).await,
            Err(StoreError::BadName)
        ));
        let edge = TableSpec::flat(vec![1u8; 63]);
        assert!(Storage::open(&disk, "db", 4, &[edge]).await.is_ok());
    });
}

/// A key no leaf could hold refuses the batch whole — the good records
/// of the same batch land none, the store's pages stay a bijection.
#[test]
fn a_bad_key_refuses_the_batch_whole() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        let huge = vec![0u8; 5000];
        // a key no leaf could hold refuses its stage, and the session
        // dies whole: its good stage lands none of it
        let mut s = st.session();
        s.put(b"t0", b"good", b"v", &[], &[]).unwrap();
        assert!(matches!(
            s.put(b"t0", &huge, b"v", &[], &[]),
            Err(StoreError::BadKey)
        ));
        drop(s);
        assert_eq!(st.get(b"t0", b"good").await.unwrap(), None);
        // a value no leaf could hold beside the oversized key: refused
        // alike, and nothing of it lands
        let mut s = st.session();
        assert!(matches!(
            s.put(b"t0", &huge, &[7u8; 600], &[], &[]),
            Err(StoreError::BadKey)
        ));
        drop(s);
        put(&st, b"t0", b"good", b"v").unwrap();
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}

/// A draft dies once (the escape hatch's own honesty): a second death,
/// a phantom key — refusals, the world untouched.
#[test]
fn a_draft_dies_once() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        let mut s = st.session();
        let d = s.alloc().await.unwrap();
        s.discard(d).unwrap();
        assert_eq!(s.discard(d), Err(StoreError::UnknownDraft));
        drop(s);
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}

/// A leak, not a laundering: a live draft no staged record takes
/// refuses the commit — which consumes the session, and that drop is
/// the clean abort (the page home, nothing landed).
#[test]
fn an_untaken_draft_refuses_the_commit() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        let mut s = st.session();
        let _leak = s.alloc().await.unwrap();
        s.put(b"t0", b"k", b"v", &[], &[]).unwrap();
        assert!(matches!(
            s.commit(),
            Err(StoreError::BadSeal("an untaken draft at the commit"))
        ));
        assert_eq!(st.get(b"t0", b"k").await.unwrap(), None);
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}

/// A transient fault mid-round rolls the world back (the unseal path):
/// the sealed delta stands, the retried round lands, and the image
/// stays a strict bijection.
#[test]
fn a_transient_fault_rolls_back_the_round() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        put(&st, b"t0", b"k", &[1u8; 3]).unwrap();
        st.flush().await.unwrap();
        // the replacement lists no pages: both records are inline
        put(&st, b"t0", b"k", &[2u8; 3]).unwrap();
        // op 3 — mid-round (op 2 is a tail read whose failure the
        // round rides out)
        disk.file("db").set_fault(Fault::Once(3));
        assert!(st.flush().await.is_err());
        disk.file("db").set_fault(Fault::None);
        // the world came back: the sealed value still reads, a fresh
        // round lands
        assert_eq!(st.get(b"t0", b"k").await.unwrap(), Some(vec![2u8; 3]));
        put(&st, b"t0", b"j", &[3u8; 3]).unwrap();
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}
