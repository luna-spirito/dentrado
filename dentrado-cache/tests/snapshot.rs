//! The session's read snapshot: one coherent world across foreign
//! commits and flushes, its pages held from reuse until the session
//! lets go. A write-only session never pins.

use dentrado_cache::{IsFilesystem, MemDisk, Session, Storage};

mod util;
use util::{chain_specs, put, run, specs};

/// Read a chain through a session's physical page read.
async fn read_chain<D: IsFilesystem>(s: &mut Session<D>, mt: &[u8]) -> Vec<u8> {
    let mut head = u64::from_le_bytes(mt[..8].try_into().unwrap());
    let mut out = Vec::new();
    while head != 0 {
        let (next, chunk) = s
            .read(head, |pg| {
                (
                    u64::from_le_bytes(pg.0[..8].try_into().unwrap()),
                    pg.0[8..].to_vec(),
                )
            })
            .await
            .unwrap();
        out.extend_from_slice(&chunk);
        head = next;
    }
    out
}

/// The first read takes the snapshot: foreign commits — puts, deletes,
/// new keys, even another table — stay out of it; a commit consumes the
/// session, and a fresh session sees the committed world.
#[test]
fn session_reads_one_world() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 8, &specs(&[b"t0", b"t1"]))
            .await
            .unwrap();
        put(&st, b"t0", b"a", b"one").unwrap();
        let mut s = st.session();
        assert_eq!(s.get(b"t0", b"a").await.unwrap(), Some(b"one".to_vec()));
        let scan = s.scan(b"t0", b"", b"").await.unwrap();
        assert_eq!(scan, [(b"a".to_vec(), b"one".to_vec())]);

        // The live world moves on; the session does not.
        put(&st, b"t0", b"a", b"two").unwrap();
        put(&st, b"t0", b"b", b"new").unwrap();
        put(&st, b"t1", b"a", b"other").unwrap();
        assert_eq!(s.get(b"t0", b"a").await.unwrap(), Some(b"one".to_vec()));
        assert_eq!(s.get(b"t0", b"b").await.unwrap(), None);
        assert_eq!(s.scan(b"t0", b"", b"").await.unwrap(), scan);
        assert_eq!(s.get(b"t1", b"a").await.unwrap(), None);

        // The commit consumes the session; the next session sees its
        // records at once, alongside everything committed before it.
        s.put(b"t0", b"a", b"three", &[], &[]).unwrap();
        s.commit().unwrap();

        let mut s = st.session();
        assert_eq!(s.get(b"t0", b"a").await.unwrap(), Some(b"three".to_vec()));
        assert_eq!(s.get(b"t0", b"b").await.unwrap(), Some(b"new".to_vec()));
        st.partition_exact().await.unwrap();
    });
}

/// A snapshot survives foreign replacements and flushes: the old chain's
/// pages sit in the pending buckets (limbo in the image), never reused
/// under the reader; dropping the session releases them, and the next
/// flush spends them.
#[test]
fn a_snapshot_survives_flush_and_reuse() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 8, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        let old = vec![1u8; 3 * util::PAYLOAD];
        let old_pages = util::put_big(&st, b"t0", b"k", &old, &[]).await.unwrap();
        st.flush().await.unwrap();

        let mut s = st.session();
        let old_mt = s.get(b"t0", b"k").await.unwrap().expect("the old record");
        assert_eq!(read_chain(&mut s, &old_mt).await, old);

        // A foreign replacement kills the old chain; the flush packs the
        // deaths (pending under the session's pin) as limbo, not reuse.
        let new = vec![2u8; 5 * util::PAYLOAD];
        util::put_big(&st, b"t0", b"k", &new, &old_pages)
            .await
            .unwrap();
        st.flush().await.unwrap();

        // The session still reads the old world, pages included.
        assert_eq!(s.get(b"t0", b"k").await.unwrap(), Some(old_mt.clone()));
        assert_eq!(read_chain(&mut s, &old_mt).await, old);

        // Letting go releases the deaths; the next flush spends them.
        drop(s);
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
        drop(st);

        let st2 = Storage::open(&disk, "db", 8, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(util::read_big(&st2, b"t0", b"k").await.unwrap(), new);
        st2.partition_exact().await.unwrap();
    });
}

/// A write-only session never pins: its commit leaves the old tree's
/// deaths to drain straight into the pile, so the next flush can spend
/// them without any pending churn.
#[test]
fn a_write_only_session_never_pins() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 8, &specs(&[b"t0"]))
            .await
            .unwrap();
        put(&st, b"t0", b"k", b"one").unwrap();
        st.flush().await.unwrap();
        let mut s = st.session();
        s.put(b"t0", b"k", b"two", &[], &[]).unwrap();
        s.commit().unwrap();
        st.flush().await.unwrap();
        assert!(
            st.debug_state().dying_len > 0,
            "a write-only session must not hold the old tree's deaths"
        );
        st.partition_exact().await.unwrap();
    });
}
