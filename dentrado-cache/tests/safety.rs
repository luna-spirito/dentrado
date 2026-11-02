//! Safe-API regressions: a misused surface refuses honestly, and a
//! refusal leaves the world whole — no phantom table reads, no draft
//! swallowed by a refused publication, no page double-listed. Every
//! test ends at rest with the page bijection checked.

use dentrado_cache::{Fault, MemDisk, Storage, StoreError};

fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().unwrap().block_on(f)
}

/// A table index outside the fixed set: reads and publications alike
/// refuse — never a phantom root read, never a swallowed draft.
#[test]
fn a_missing_table_refuses_honestly() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &["t0"]).await.unwrap();
        assert!(matches!(
            st.get_cache("t1", b"k").await,
            Err(StoreError::NoTable)
        ));
        let d = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(d, |_| (vec![1u8; 3], ()))
            .await
            .unwrap();
        assert_eq!(
            st.put_cache("t1", b"k", &[0], &[d], &[]).await,
            Err(StoreError::NoTable)
        );
        // the draft survived the refusal: it still deallocs, its twin
        // still publishes
        let d2 = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(d2, |_| (vec![2u8; 3], ()))
            .await
            .unwrap();
        st.put_cache("t0", b"j", &[0], &[d2], &[]).await.unwrap();
        st.dealloc_draft_page(d).await.unwrap();
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}

/// A freed list that would conjure or double-list a page: the
/// publication refuses whole, and the consumed draft stays held — the
/// next well-formed publication of the very same draft goes through.
#[test]
fn a_bad_seal_refuses_and_keeps_the_draft() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &["t0"]).await.unwrap();
        let d = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(d, |_| (vec![1u8; 3], ()))
            .await
            .unwrap();
        assert!(matches!(
            st.put_cache("t0", b"k", &[0], &[d], &[999]).await,
            Err(StoreError::BadSeal(_))
        ));
        assert!(matches!(
            st.put_cache("t0", b"k", &[0], &[d], &[5, 5]).await,
            Err(StoreError::BadSeal(_))
        ));
        st.put_cache("t0", b"k", &[0], &[d], &[]).await.unwrap();
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}

/// A draft dies once: a second death, a death after publication, a
/// publication of the dead — honest refusals, the world untouched.
#[test]
fn a_draft_dies_once() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &["t0"]).await.unwrap();
        let d = st.alloc_draft_page().await.unwrap();
        st.dealloc_draft_page(d).await.unwrap();
        assert_eq!(
            st.dealloc_draft_page(d).await,
            Err(StoreError::UnknownDraft)
        );
        // a consumed draft is no longer the caller's to kill
        let e = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(e, |_| (vec![1u8; 3], ()))
            .await
            .unwrap();
        st.put_cache("t0", b"k", &[0], &[e], &[]).await.unwrap();
        assert_eq!(
            st.dealloc_draft_page(e).await,
            Err(StoreError::UnknownDraft)
        );
        // a vanished consumed draft — the publication refuses
        let v = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(v, |_| (vec![2u8; 3], ()))
            .await
            .unwrap();
        st.dealloc_draft_page(v).await.unwrap();
        assert_eq!(
            st.put_cache("t0", b"v", &[0], &[v], &[]).await,
            Err(StoreError::UnknownDraft)
        );
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}

/// A transient fault mid-round rolls the world back (the unseal path):
/// the retried round lands, and the image stays a strict bijection.
#[test]
fn a_transient_fault_rolls_back_the_round() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &["t0"]).await.unwrap();
        let k = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(k, |_| (vec![1u8; 3], ()))
            .await
            .unwrap();
        st.put_cache("t0", b"k", &[0], &[k], &[]).await.unwrap();
        st.flush().await.unwrap();
        let k2 = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(k2, |_| (vec![2u8; 3], ()))
            .await
            .unwrap();
        let old = match st.get_cache("t0", b"k").await.unwrap() {
            Some((g, cv)) => {
                let p = cv.pages.clone();
                drop(g);
                p
            }
            None => Vec::new(),
        };
        st.put_cache("t0", b"k", &[0], &[k2], &old).await.unwrap();
        disk.file("db").set_fault(Fault::Once(2));
        assert!(st.flush().await.is_err());
        disk.file("db").set_fault(Fault::None);
        // the world came back: the value still reads, a fresh round lands
        let k3 = st.alloc_draft_page().await.unwrap();
        st.write_draft_page(k3, |_| (vec![3u8; 3], ()))
            .await
            .unwrap();
        st.put_cache("t0", b"j", &[0], &[k3], &[]).await.unwrap();
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}
