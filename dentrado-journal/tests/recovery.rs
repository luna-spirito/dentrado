//! Opening, repairing, and refusing: what the files on disk mean.

use std::ops::ControlFlow;

use dentrado_journal::{Conflict, Cursor, Decl, Error, Frame, Journal, Options, SchemaId};

mod common;
use common::{COUNTER, COUNTER_SHAPE, copy_dir, counter, flip, records, run, segments, truncate};

#[test]
fn an_empty_directory_is_a_log() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(log.head(), Cursor::START);
        assert!(records(&log, Cursor::START).await.is_empty());
        drop(log);
        assert!(segments(dir.path()).is_empty(), "opening wrote to the log");
    });
}

#[test]
fn declarations_survive_a_restart() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"x")]).await.unwrap();
        log.sync().await.unwrap();
        let head = log.head();
        drop(log);

        let log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(log.head(), head);
        assert_eq!(log.truncated(), 0);
        let declared = log.decls().get(schema).unwrap();
        assert_eq!(declared.name, COUNTER);
        assert_eq!(declared.shape, COUNTER_SHAPE);
        assert_eq!(records(&log, Cursor::START).await.len(), 2);
    });
}

#[test]
fn a_shape_keeps_its_id_and_a_change_gets_a_new_one() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let first = counter(&mut log).await;
        assert_eq!(counter(&mut log).await, first);
        let changed = log
            .intern(Decl {
                name: COUNTER,
                shape: "{at:u32}",
            })
            .await
            .unwrap();
        assert_ne!(changed, first);

        let refused = log
            .intern(Decl {
                name: "example/other",
                shape: COUNTER_SHAPE,
            })
            .await;
        assert!(matches!(
            refused,
            Err(Error::Schema {
                conflict: Conflict::Renamed,
                ..
            })
        ));
    });
}

#[test]
fn an_undeclared_schema_is_refused() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let refused = log.append(&[Frame::new(SchemaId(200), b"x")]).await;
        assert!(matches!(refused, Err(Error::Undeclared { id: 200 })));
        let refused = log.append(&[Frame::new(SchemaId::DECL, b"x")]).await;
        assert!(matches!(refused, Err(Error::Reserved { id: 0 })));
    });
}

#[test]
fn caps_are_enforced() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let long = "x".repeat(dentrado_journal::schema::MAX_TEXT + 1);
        let refused = log
            .intern(Decl {
                name: &long,
                shape: "u8",
            })
            .await;
        assert!(matches!(refused, Err(Error::TooLarge { .. })));

        let schema = counter(&mut log).await;
        let big = vec![0u8; dentrado_journal::frame::MAX_PAYLOAD + 1];
        let refused = log.append(&[Frame::new(schema, &big)]).await;
        assert!(matches!(refused, Err(Error::TooLarge { .. })));
    });
}

#[test]
fn a_declaration_after_data_does_not_move_the_head() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let first = counter(&mut log).await;
        log.append(&[Frame::new(first, b"x")]).await.unwrap();
        let head = log.head();
        assert_eq!(head.segment, 1);

        let second = log
            .intern(Decl {
                name: "example/other",
                shape: "[u8;32]",
            })
            .await
            .unwrap();
        assert_eq!(log.head(), head, "a declaration went to segment zero");
        log.sync().await.unwrap();
        drop(log);

        let log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(log.head(), head);
        assert_eq!(log.decls().get(first).unwrap().name, COUNTER);
        assert_eq!(log.decls().get(second).unwrap().name, "example/other");
        let payloads: Vec<_> = records(&log, Cursor::START)
            .await
            .into_iter()
            .filter(|(_, schema, _)| schema.is_user())
            .map(|(_, _, payload)| payload)
            .collect();
        assert_eq!(payloads, [b"x".to_vec()]);

        // A cursor past the head has nothing to replay.
        assert!(records(&log, Cursor::new(99, 0)).await.is_empty());
    });
}

#[test]
fn appending_after_a_close_starts_a_new_segment() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"before")]).await.unwrap();
        log.close().await.unwrap();
        let closed = segments(dir.path()).len();
        assert_eq!(closed, 2, "the declaration segment plus one data segment");
        log.append(&[Frame::new(schema, b"after")]).await.unwrap();
        assert_eq!(segments(dir.path()).len(), closed + 1);
        drop(log);

        let log = Journal::open(dir.path()).await.unwrap();
        let payloads: Vec<_> = records(&log, Cursor::START)
            .await
            .into_iter()
            .filter(|(_, schema, _)| schema.is_user())
            .map(|(_, _, payload)| payload)
            .collect();
        assert_eq!(payloads, [b"before".to_vec(), b"after".to_vec()]);
    });
}

#[test]
fn rotation_keeps_the_stream_and_the_cursors() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let options = Options { rotate_bytes: 128 };
        let mut log = Journal::open_with(dir.path(), options).await.unwrap();
        let schema = counter(&mut log).await;
        for round in 0..20u8 {
            log.append(&[Frame::new(schema, &[round; 16])])
                .await
                .unwrap();
        }
        log.sync().await.unwrap();
        assert!(segments(dir.path()).len() > 3);

        let all = records(&log, Cursor::START).await;
        assert_eq!(all.len(), 21);
        for (round, (_, _, payload)) in all[1..].iter().enumerate() {
            assert_eq!(payload, &[round as u8; 16]);
        }
        assert!(all.windows(2).all(|pair| pair[0].0 < pair[1].0));

        let middle = all[all.len() / 2].0;
        assert_eq!(records(&log, middle).await, all[all.len() / 2..]);
    });
}

#[test]
fn damage_in_a_closed_segment_is_fatal() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let options = Options { rotate_bytes: 128 };
        let mut log = Journal::open_with(dir.path(), options).await.unwrap();
        let schema = counter(&mut log).await;
        for round in 0..20u8 {
            log.append(&[Frame::new(schema, &[round; 16])])
                .await
                .unwrap();
        }
        log.close().await.unwrap();
        drop(log);

        let files = segments(dir.path());
        assert!(files.len() > 2);
        flip(&files[1], 32 + 4);

        let log = Journal::open(dir.path()).await.unwrap();
        let refused = log
            .replay_from(Cursor::START, async |_, _| ControlFlow::Continue(()))
            .await;
        assert!(matches!(refused, Err(Error::Corrupt { .. })), "{refused:?}");
    });
}

#[test]
fn damage_inside_a_sealed_tail_is_fatal_not_a_torn_tail() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        for round in 0..4u8 {
            log.append(&[Frame::new(schema, &[round; 16])])
                .await
                .unwrap();
        }
        log.close().await.unwrap();
        drop(log);

        let files = segments(dir.path());
        flip(&files[1], 32);

        let opened = Journal::open(dir.path()).await.map(|_| ());
        assert!(
            matches!(opened, Err(Error::Corrupt { .. })),
            "a sealed segment with a damaged frame must abort, not lose its tail: {opened:?}"
        );
    });
}

#[test]
fn an_interior_segment_without_a_footer_is_refused_at_open() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let options = Options { rotate_bytes: 128 };
        let mut log = Journal::open_with(dir.path(), options).await.unwrap();
        let schema = counter(&mut log).await;
        for round in 0..20u8 {
            log.append(&[Frame::new(schema, &[round; 16])])
                .await
                .unwrap();
        }
        log.close().await.unwrap();
        drop(log);

        let files = segments(dir.path());
        assert!(files.len() > 2);
        let len = std::fs::metadata(&files[1]).unwrap().len();
        truncate(&files[1], len - 44);

        let opened = Journal::open(dir.path()).await.map(|_| ());
        assert!(
            matches!(opened, Err(Error::Corrupt { .. })),
            "a sealed interior segment that lost its footer must be refused: {opened:?}"
        );
    });
}

#[test]
fn a_footer_that_disagrees_is_fatal() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let options = Options { rotate_bytes: 128 };
        let mut log = Journal::open_with(dir.path(), options).await.unwrap();
        let schema = counter(&mut log).await;
        for round in 0..20u8 {
            log.append(&[Frame::new(schema, &[round; 16])])
                .await
                .unwrap();
        }
        log.close().await.unwrap();
        drop(log);

        let files = segments(dir.path());
        let len = std::fs::metadata(&files[1]).unwrap().len();
        flip(&files[1], len - 40);

        let log = Journal::open(dir.path()).await.unwrap();
        let refused = log
            .replay_from(Cursor::START, async |_, _| ControlFlow::Continue(()))
            .await;
        assert!(matches!(refused, Err(Error::Corrupt { .. })), "{refused:?}");
    });
}

#[test]
fn a_torn_tail_replays_exactly_its_valid_prefix() {
    run(async {
        let source = tempfile::tempdir().unwrap();
        let mut log = Journal::open(source.path()).await.unwrap();
        let schema = counter(&mut log).await;
        for payload in [&b""[..], b"a", &[7u8; 300], b"last"] {
            log.append(&[Frame::new(schema, payload)]).await.unwrap();
        }
        let full = records(&log, Cursor::START).await;
        drop(log);

        let tail = segments(source.path()).pop().unwrap();
        let len = std::fs::metadata(&tail).unwrap().len();
        let tail_name = tail.file_name().unwrap().to_owned();

        for cut in 0..=len {
            let victim = tempfile::tempdir().unwrap();
            copy_dir(source.path(), victim.path());
            truncate(&victim.path().join(&tail_name), cut);

            let mut log = Journal::open(victim.path()).await.unwrap();
            let segment = log.head().segment;
            let want: Vec<_> = full
                .iter()
                .filter(|(at, _, payload)| {
                    at.segment < segment || at.offset + 10 + payload.len() as u64 <= cut
                })
                .cloned()
                .collect();
            assert_eq!(
                records(&log, Cursor::START).await,
                want,
                "cut at {cut} of {len}"
            );

            // And the log goes on from there.
            let head = log.head();
            log.append(&[Frame::new(schema, b"recovered")])
                .await
                .unwrap();
            let seen = records(&log, Cursor::START).await;
            assert_eq!(seen.len(), want.len() + 1, "cut at {cut} of {len}");
            assert_eq!(seen.last().unwrap().2, b"recovered");
            assert!(head < log.head());
        }
    });
}

#[test]
fn a_cut_tail_is_repaired_but_a_closed_one_is_not() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"kept")]).await.unwrap();
        log.close().await.unwrap();
        log.append(&[Frame::new(schema, b"new")]).await.unwrap();
        log.sync().await.unwrap();
        let head = log.head();
        drop(log);

        let files = segments(dir.path());
        let len = std::fs::metadata(&files[2]).unwrap().len();
        truncate(&files[2], len / 2);

        let log = Journal::open(dir.path()).await.unwrap();
        assert!(log.truncated() > 0);
        assert!(log.head() < head);
        let payloads: Vec<_> = records(&log, Cursor::START)
            .await
            .into_iter()
            .filter(|(_, schema, _)| schema.is_user())
            .map(|(_, _, payload)| payload)
            .collect();
        assert_eq!(payloads, [b"kept".to_vec()]);
    });
}

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn any_stream_survives_any_torn_tail(
        payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..40), 1..6),
        seed in any::<u64>(),
    ) {
        run(async {
            let source = tempfile::tempdir().unwrap();
            let mut log = Journal::open(source.path()).await.unwrap();
            let schema = counter(&mut log).await;
            for payload in &payloads {
                log.append(&[Frame::new(schema, payload)]).await.unwrap();
            }
            let full = records(&log, Cursor::START).await;
            drop(log);

            let tail = segments(source.path()).pop().unwrap();
            let len = std::fs::metadata(&tail).unwrap().len();
            let cut = if len == 0 { 0 } else { seed % (len + 1) };
            let tail_name = tail.file_name().unwrap().to_owned();

            let victim = tempfile::tempdir().unwrap();
            copy_dir(source.path(), victim.path());
            truncate(&victim.path().join(&tail_name), cut);

            let log = Journal::open(victim.path()).await.unwrap();
            let segment = log.head().segment;
            let want: Vec<_> = full
                .into_iter()
                .filter(|(at, _, payload)| {
                    at.segment < segment || at.offset + 10 + payload.len() as u64 <= cut
                })
                .collect();
            assert_eq!(records(&log, Cursor::START).await, want);
        });
    }
}
