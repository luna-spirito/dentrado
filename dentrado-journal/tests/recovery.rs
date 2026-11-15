//! Opening, declaring, rotating: the log when nothing is wrong.
//!
//! Damage has its own file — this one is about the shape of a healthy log and
//! the states a crash alone can produce.

use dentrado_journal::{Conflict, Cursor, Decl, Error, Frame, Journal, Options, SchemaId};

mod common;
use common::{COUNTER, COUNTER_SHAPE, counter, records, run, segments};

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
fn a_failed_write_poisons_the_journal() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let options = Options { rotate_bytes: 128 };
        let mut log = Journal::open_with(&path, options).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, &[0u8; 64])]).await.unwrap();

        // Rotation needs a new file, and the directory is no longer there.
        std::fs::remove_dir_all(&path).unwrap();
        let failed = log.append(&[Frame::new(schema, &[1u8; 64])]).await;
        assert!(failed.is_err(), "the rotation cannot succeed: {failed:?}");

        // A half-applied write is not retried on the same handle.
        let refused = log.append(&[Frame::new(schema, &[2u8; 64])]).await;
        assert!(matches!(refused, Err(Error::Poisoned)), "{refused:?}");
        assert!(matches!(log.sync().await, Err(Error::Poisoned)));
        assert!(matches!(log.close().await, Err(Error::Poisoned)));
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
fn a_second_writer_is_refused() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let _first = Journal::open(dir.path()).await.unwrap();
        let second = Journal::open(dir.path()).await.map(|_| ());
        assert!(
            matches!(second, Err(Error::Locked)),
            "one writer per directory: {second:?}"
        );
    });
}
