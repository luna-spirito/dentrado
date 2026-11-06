//! The record surface's own semantics: scan's order and overlays,
//! deletion down to an empty table, the batch's last-key-wins with its
//! lists unioned, and the schema-walked closures of `drop_table` — a
//! shared closure dies once, a content cycle rides the seen-set, a
//! refusing schema keeps the world, an untaken page shows in the
//! partition.

use dentrado_cache::{
    MemDisk, NO_DEPS, PageId, PageSchema, Rec, Storage, StoreError, TableSchema, TableSpec,
    Violation,
};

mod util;
use util::{chain_specs, del, keys, put, put_big, read_big, run, specs};

/// Scan reads in key order — the standing tree pruned to the range,
/// the delta overlaid on it (a put replaces, a deletion hides), and
/// `after` excludes strictly: the records past the named key.
#[test]
fn scan_orders_and_overlays() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 8, &specs(&[b"t0"]))
            .await
            .unwrap();
        for (k, v) in [("b", 2u8), ("a", 1), ("d", 4)] {
            st.put_many(b"t0", &[put(k.as_bytes(), &[v])])
                .await
                .unwrap();
        }
        st.flush().await.unwrap();
        // the delta half over the tree: a fresh key, an overwrite, a kill
        st.put_many(b"t0", &[put(b"aa", &[9]), put(b"a", &[7]), del(b"d")])
            .await
            .unwrap();
        let all = st.scan(b"t0", b"", b"").await.unwrap();
        assert_eq!(
            all,
            [
                (b"a".to_vec(), vec![7u8]),
                (b"aa".to_vec(), vec![9]),
                (b"b".to_vec(), vec![2]),
            ]
        );
        // prefix and after (exclusive)
        assert_eq!(
            keys(&st.scan(b"t0", b"a", b"").await.unwrap()),
            [b"a".to_vec(), b"aa".to_vec()]
        );
        assert_eq!(
            keys(&st.scan(b"t0", b"a", b"a").await.unwrap()),
            [b"aa".to_vec()]
        );
        assert!(st.scan(b"t0", b"a", b"aa").await.unwrap().is_empty());
        // an after past the prefix's span sees nothing
        assert!(st.scan(b"t0", b"a", b"z").await.unwrap().is_empty());
        // the merged view survives its flush and a reopen
        st.flush().await.unwrap();
        drop(st);
        let st2 = Storage::open(&disk, "db", 8, &specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(st2.scan(b"t0", b"", b"").await.unwrap(), all);
        st2.partition_exact().await.unwrap();
    });
}

/// Deletion is real: the whole contents go in one batch, the table
/// empties (its root with them), and the emptied table still takes
/// fresh records. A delete of a key that never was is a quiet no-op.
#[test]
fn deletion_empties_the_table() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        for k in ["x", "y", "z"] {
            st.put_many(b"t0", &[put(k.as_bytes(), &[1, 2])])
                .await
                .unwrap();
        }
        st.flush().await.unwrap();
        st.put_many(b"t0", &[del(b"no")]).await.unwrap();
        assert_eq!(st.scan(b"t0", b"", b"").await.unwrap().len(), 3);
        st.put_many(b"t0", &[del(b"x"), del(b"y"), del(b"z")])
            .await
            .unwrap();
        assert!(st.scan(b"t0", b"", b"").await.unwrap().is_empty());
        for k in ["x", "y", "z"] {
            assert_eq!(st.get(b"t0", k.as_bytes()).await.unwrap(), None);
        }
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
        // the emptied table lives on: a fresh record lands and survives
        st.put_many(b"t0", &[put(b"w", &[3])]).await.unwrap();
        st.flush().await.unwrap();
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(st2.get(b"t0", b"w").await.unwrap(), Some(vec![3]));
        assert_eq!(st2.get(b"t0", b"x").await.unwrap(), None);
        st2.partition_exact().await.unwrap();
    });
}

/// A key written twice in one batch takes its last record; the takes
/// and the frees of its records union. A vanished take — a draft that
/// died before the batch — refuses the batch whole, and its good
/// neighbour registers nothing; the live draft stays held — the
/// caller publishes it (naming its page, or the partition cries leak).
#[test]
fn a_batch_is_last_key_wins() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &chain_specs(&[b"t0"]))
            .await
            .unwrap();
        st.put_many(b"t0", &[put(b"k", &[1]), put(b"k", &[2]), put(b"j", &[3])])
            .await
            .unwrap();
        assert_eq!(st.get(b"t0", b"k").await.unwrap(), Some(vec![2]));
        assert_eq!(st.scan(b"t0", b"", b"").await.unwrap().len(), 2);
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
        // the vanished take: one draft dealloc'd, one alive
        let gone = st.alloc_draft_page().await.unwrap();
        st.dealloc_draft_page(gone).await.unwrap();
        let fresh = st.alloc_draft_page().await.unwrap();
        let r = st
            .put_many(
                b"t0",
                &[
                    Rec::Put {
                        key: b"good",
                        val: b"v",
                        take: &[gone, fresh],
                        free: &[],
                    },
                    del(b"j"),
                ],
            )
            .await;
        assert!(matches!(r, Err(StoreError::ConsumedVanished)));
        assert_eq!(st.get(b"t0", b"good").await.unwrap(), None);
        assert_eq!(st.get(b"t0", b"j").await.unwrap(), Some(vec![3]));
        // the live draft stayed held: the retry publishes it alone,
        // the record naming its page (a chain head, `next` = 0)
        let p = st.draft_page(&fresh).await.unwrap();
        st.write_draft_page(fresh, move |pg| {
            pg.0[..8].fill(0);
            pg.0[8..11].copy_from_slice(b"vvv");
        })
        .await
        .unwrap();
        st.put_many(
            b"t0",
            &[Rec::Put {
                key: b"good",
                val: &p.to_le_bytes(),
                take: &[fresh],
                free: &[],
            }],
        )
        .await
        .unwrap();
        assert!(
            read_big(&st, b"t0", b"good")
                .await
                .unwrap()
                .starts_with(b"vvv")
        );
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}

// ## Schemas: hand-made walks over a real store

/// A hand-made schema, one pair of functions for both roles: bytes
/// that start with the magic name one page (the id after the magic,
/// little-endian) — an inline record or a named page's payload alike.
/// A content cycle between two named pages rides the seen-set and
/// dies as a closure, not an error.
const MAGIC: &[u8] = &[0x42; 8];

fn ids_deps(b: &[u8]) -> Result<Vec<(PageId, PageSchema)>, StoreError> {
    Ok(if b.starts_with(MAGIC) {
        vec![(u64::from_le_bytes(b[8..16].try_into().unwrap()), IDS_PAGE)]
    } else {
        Vec::new()
    })
}

const IDS: TableSchema = TableSchema { val_deps: ids_deps };
const IDS_PAGE: PageSchema = PageSchema { deps: ids_deps };

/// A drop frees the closure exactly once per page: one record's taken
/// pair, two records' schema-named shares of them, a hop through
/// content into a cycle — all one set of deaths, and the world a
/// strict bijection after the round that spends them.
#[test]
fn a_shared_closure_dies_once() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(
            &disk,
            "db",
            4,
            &[TableSpec {
                id: b"t0".to_vec(),
                schema: IDS,
            }],
        )
        .await
        .unwrap();
        // the taken pages: 2 then 4, the first naming 4 in its content,
        // the second naming 2 — a content cycle
        let mut ks = Vec::new();
        for names in [4u64, 2u64] {
            let k = st.alloc_draft_page().await.unwrap();
            st.write_draft_page(k, move |pg| {
                pg.0[..8].copy_from_slice(MAGIC);
                pg.0[8..16].copy_from_slice(&names.to_le_bytes());
            })
            .await
            .unwrap();
            ks.push(k);
        }
        let mut pages = Vec::new();
        for k in &ks {
            pages.push(st.draft_page(k).await.unwrap());
        }
        assert_eq!(pages, [2, 4], "свежий пул раздаёт чётные");
        // the inline namers: one at the pair's head, one at its tail
        let namer = |p: u64| {
            let mut v = vec![0u8; 16];
            v[..8].copy_from_slice(MAGIC);
            v[8..].copy_from_slice(&p.to_le_bytes());
            v
        };
        st.put_many(
            b"t0",
            &[
                Rec::Put {
                    key: b"c",
                    val: &namer(2),
                    take: &ks,
                    free: &[],
                },
                Rec::Put {
                    key: b"h",
                    val: &namer(2),
                    take: &[],
                    free: &[],
                },
                Rec::Put {
                    key: b"t",
                    val: &namer(4),
                    take: &[],
                    free: &[],
                },
            ],
        )
        .await
        .unwrap();
        st.flush().await.unwrap();
        // the world: tree {3}, the pair {2, 4} — one set of three deaths
        st.drop_table(b"t0").await.unwrap();
        assert_eq!(st.debug_state().dying_len, 3);
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}

/// A schema that refuses keeps the world: `drop_table` propagates the
/// refusal before any death registers, the table reads on — and the
/// partition audit refuses with the schema: a record whose deps it
/// cannot enumerate is a walk it cannot vouch for.
#[test]
fn a_refusing_schema_keeps_the_world() {
    fn refuses(b: &[u8]) -> Result<Vec<(PageId, PageSchema)>, StoreError> {
        if b.first() == Some(&0xFF) {
            Err(StoreError::CorruptValue("a poisoned record"))
        } else {
            Ok(Vec::new())
        }
    }
    const REFUSES: TableSchema = TableSchema { val_deps: refuses };
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(
            &disk,
            "db",
            4,
            &[TableSpec {
                id: b"t0".to_vec(),
                schema: REFUSES,
            }],
        )
        .await
        .unwrap();
        st.put_many(b"t0", &[put(b"k", &[0xFF, 1])]).await.unwrap();
        st.flush().await.unwrap();
        assert!(matches!(
            st.drop_table(b"t0").await,
            Err(StoreError::CorruptValue(_))
        ));
        assert_eq!(st.tables(), [b"t0".to_vec()]);
        assert_eq!(st.get(b"t0", b"k").await.unwrap(), Some(vec![0xFF, 1]));
        assert_eq!(st.debug_state().dying_len, 0);
        st.flush().await.unwrap();
        let v = st
            .partition_exact()
            .await
            .expect_err("the audit refuses too");
        assert!(v.iter().any(|x| matches!(x, Violation::Broken(_))), "{v:?}");
    });
}

/// A `NO_DEPS` table that took pages anyway: the seal is the caller's
/// word, but the partition invariants are not — the orphaned page
/// reports as a missing role (leak detection, not silent rot).
#[test]
fn untaken_pages_show_in_the_partition() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(
            &disk,
            "db",
            4,
            &[TableSpec {
                id: b"t0".to_vec(),
                schema: NO_DEPS,
            }],
        )
        .await
        .unwrap();
        let k = st.alloc_draft_page().await.unwrap();
        let p = st.draft_page(&k).await.unwrap();
        st.put_many(
            b"t0",
            &[Rec::Put {
                key: b"kk",
                val: b"v",
                take: &[k],
                free: &[],
            }],
        )
        .await
        .unwrap();
        st.flush().await.unwrap();
        let v = st.partition_exact().await.expect_err("orphaned page");
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::Missing(q) if *q == p)),
            "{v:?}"
        );
    });
}
