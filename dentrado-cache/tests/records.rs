//! The record surface's own semantics: scan's order and overlays,
//! deletion down to an empty table, the inline/blob boundary, and the
//! schema-walked closures of `drop_table` — a shared page dies once, a
//! content cycle rides the seen-set, a refusing schema keeps the world.

use dentrado_cache::{
    INLINE_MAX, MemDisk, PageId, PageSchema, Rec, Storage, StoreError, TableSchema, TableSpec,
};

fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().unwrap().block_on(f)
}

fn specs(ids: &[&[u8]]) -> Vec<TableSpec> {
    ids.iter().map(|i| TableSpec::flat(i.to_vec())).collect()
}

fn keys(recs: &[(Vec<u8>, Vec<u8>)]) -> Vec<Vec<u8>> {
    recs.iter().map(|(k, _)| k.clone()).collect()
}

/// Scan reads in key order — the standing tree pruned to the range,
/// the delta overlaid on it (a put replaces, a deletion hides), and
/// `after` excludes strictly: the page after the named key.
#[test]
fn scan_orders_and_overlays() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 8, &specs(&[b"t0"]))
            .await
            .unwrap();
        for (k, v) in [("b", 2u8), ("a", 1), ("d", 4)] {
            st.put_many(
                b"t0",
                &[Rec::Put {
                    key: k.as_bytes(),
                    val: &[v],
                }],
            )
            .await
            .unwrap();
        }
        st.flush().await.unwrap();
        // the delta half over the tree: a fresh key, an overwrite, a kill
        st.put_many(
            b"t0",
            &[
                Rec::Put {
                    key: b"aa",
                    val: &[9],
                },
                Rec::Put {
                    key: b"a",
                    val: &[7],
                },
                Rec::Del { key: b"d" },
            ],
        )
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
        // an after past the prefix's end sees nothing
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
            st.put_many(
                b"t0",
                &[Rec::Put {
                    key: k.as_bytes(),
                    val: &[1, 2],
                }],
            )
            .await
            .unwrap();
        }
        st.flush().await.unwrap();
        st.put_many(b"t0", &[Rec::Del { key: b"no" }])
            .await
            .unwrap();
        assert_eq!(st.scan(b"t0", b"", b"").await.unwrap().len(), 3);
        st.put_many(
            b"t0",
            &[
                Rec::Del { key: b"x" },
                Rec::Del { key: b"y" },
                Rec::Del { key: b"z" },
            ],
        )
        .await
        .unwrap();
        assert!(st.scan(b"t0", b"", b"").await.unwrap().is_empty());
        for k in ["x", "y", "z"] {
            assert_eq!(st.get(b"t0", k.as_bytes()).await.unwrap(), None);
        }
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
        // the emptied table lives on: a fresh record lands and survives
        st.put_many(
            b"t0",
            &[Rec::Put {
                key: b"w",
                val: &[3],
            }],
        )
        .await
        .unwrap();
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

/// The inline boundary is sharp: a value of exactly [`INLINE_MAX`]
/// stays inline, one byte more becomes a value chain — and the
/// chain's death frees its pages whole.
#[test]
fn the_inline_boundary() {
    run(async {
        let disk = MemDisk::default();
        let st = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        let inline = vec![7u8; INLINE_MAX];
        let chained = vec![9u8; INLINE_MAX + 1];
        st.put_many(
            b"t0",
            &[
                Rec::Put {
                    key: b"i",
                    val: &inline,
                },
                Rec::Put {
                    key: b"b",
                    val: &chained,
                },
            ],
        )
        .await
        .unwrap();
        st.flush().await.unwrap();
        drop(st);
        let st2 = Storage::open(&disk, "db", 4, &specs(&[b"t0"]))
            .await
            .unwrap();
        assert_eq!(st2.get(b"t0", b"i").await.unwrap(), Some(inline));
        assert_eq!(st2.get(b"t0", b"b").await.unwrap(), Some(chained));
        // the chained value's replacement frees its pages: the world
        // stays a strict bijection
        st2.put_many(
            b"t0",
            &[Rec::Put {
                key: b"b",
                val: &[1],
            }],
        )
        .await
        .unwrap();
        st2.flush().await.unwrap();
        st2.partition_exact().await.unwrap();
    });
}

// ## Schemas: hand-made walks over a real store

/// A hand-made schema: bytes that start with the magic name one page
/// (the id after the magic, little-endian) — an inline record or a
/// named page's payload alike. A content cycle between two named
/// pages rides the seen-set and dies as a closure, not an error.
struct Ids;

const IDS: &dyn TableSchema = &Ids;
const IDS_PAGE: &dyn PageSchema = &Ids;
const MAGIC: &[u8] = &[0x42; 8];

fn one_id(b: &[u8]) -> (PageId, &'static dyn PageSchema) {
    (u64::from_le_bytes(b[8..16].try_into().unwrap()), IDS_PAGE)
}

impl TableSchema for Ids {
    fn val_deps(&self, val: &[u8]) -> Result<Vec<(PageId, &'static dyn PageSchema)>, StoreError> {
        Ok(if val.starts_with(MAGIC) {
            vec![one_id(val)]
        } else {
            Vec::new() // a chained record: its mt is a length, not bytes of ours
        })
    }
}

impl PageSchema for Ids {
    fn deps(&self, page: &[u8]) -> Result<Vec<(PageId, &'static dyn PageSchema)>, StoreError> {
        Ok(if page.starts_with(MAGIC) {
            vec![one_id(page)]
        } else {
            Vec::new()
        })
    }
}

const VAL_P: usize = dentrado_cache::core::val::VAL_PAYLOAD;

/// A drop frees the closure exactly once per page: one record's value
/// chain, another record's schema-named share of it, a third's hop
/// through content into a cycle — all one set of deaths, and the
/// world a strict bijection after the round that spends them.
#[test]
fn a_shared_closure_dies_once() {
    run(async {
        let disk = MemDisk::default();
        let spec = TableSpec {
            id: b"t0".to_vec(),
            schema: IDS,
        };
        let st = Storage::open(&disk, "db", 4, &[spec]).await.unwrap();
        // the chain: two pages (2 then 4), the first naming 4 in its
        // content, the second naming 2 — a content cycle
        let mut chain = vec![0u8; 2 * VAL_P];
        chain[..8].copy_from_slice(MAGIC);
        chain[8..16].copy_from_slice(&4u64.to_le_bytes());
        chain[VAL_P..][..8].copy_from_slice(MAGIC);
        chain[VAL_P + 8..][..8].copy_from_slice(&2u64.to_le_bytes());
        // the inline namers: one at the chain's head, one at its tail
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
                    val: &chain,
                },
                Rec::Put {
                    key: b"h",
                    val: &namer(2),
                },
                Rec::Put {
                    key: b"t",
                    val: &namer(4),
                },
            ],
        )
        .await
        .unwrap();
        st.flush().await.unwrap();
        // the world: tree {3}, value {2, 4} — one set of three deaths
        st.drop_table(b"t0").await.unwrap();
        assert_eq!(st.debug_state().dying_len, 3);
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}

/// A schema that refuses keeps the world: `drop_table` propagates the
/// refusal before any death registers, and the table reads on.
#[test]
fn a_refusing_schema_keeps_the_world() {
    struct Refuses;
    const REFUSES: &dyn TableSchema = &Refuses;
    impl TableSchema for Refuses {
        fn val_deps(
            &self,
            val: &[u8],
        ) -> Result<Vec<(PageId, &'static dyn PageSchema)>, StoreError> {
            if val.first() == Some(&0xFF) {
                Err(StoreError::CorruptValue("a poisoned record"))
            } else {
                Ok(Vec::new())
            }
        }
    }
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
        st.put_many(
            b"t0",
            &[Rec::Put {
                key: b"k",
                val: &[0xFF, 1],
            }],
        )
        .await
        .unwrap();
        st.flush().await.unwrap();
        assert!(matches!(
            st.drop_table(b"t0").await,
            Err(StoreError::CorruptValue(_))
        ));
        assert_eq!(st.tables(), [b"t0".to_vec()]);
        assert_eq!(st.get(b"t0", b"k").await.unwrap(), Some(vec![0xFF, 1]));
        assert_eq!(st.debug_state().dying_len, 0);
        st.flush().await.unwrap();
        st.partition_exact().await.unwrap();
    });
}
