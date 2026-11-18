//! The reserved identity record: created once, durable, and stable across
//! reopens.
//!
//! The identity is what a cache materialization records as "these bodies belong
//! to this log". It lives in segment zero, so it is read before any data and
//! writing it late never moves the data head.

use dentrado_journal::{Cursor, Frame, Journal, SchemaId};

mod common;
use common::{counter, records, run};

#[test]
fn identity_is_created_once_and_survives_reopen() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let id = log.identity().await.unwrap();

        // A second call is a lookup, not a second record.
        assert_eq!(log.identity().await.unwrap(), id);
        log.verify().await.unwrap();

        let seen = records(&log, Cursor::START).await;
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, SchemaId::IDENTITY);
        assert_eq!(seen[0].2.as_slice(), id.as_slice());

        drop(log);
        let mut log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(log.identity().await.unwrap(), id);
        assert_eq!(records(&log, Cursor::START).await.len(), 1);
    });
}

#[test]
fn two_logs_have_different_identities() {
    run(async {
        let a_dir = tempfile::tempdir().unwrap();
        let b_dir = tempfile::tempdir().unwrap();
        let mut a = Journal::open(a_dir.path()).await.unwrap();
        let mut b = Journal::open(b_dir.path()).await.unwrap();
        assert_ne!(a.identity().await.unwrap(), b.identity().await.unwrap());
    });
}

#[test]
fn a_late_identity_does_not_move_the_data_head() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"x")]).await.unwrap();
        log.sync().await.unwrap();
        let head = log.head();

        let id = log.identity().await.unwrap();
        assert_eq!(log.head(), head, "the identity lives before the data");

        let seen = records(&log, Cursor::START).await;
        let at = seen
            .iter()
            .position(|(_, schema, _)| *schema == SchemaId::IDENTITY)
            .expect("the identity is in the stream");
        assert_eq!(seen[at].2.as_slice(), id.as_slice());
        assert!(seen[at].0 < head);
        log.verify().await.unwrap();

        drop(log);
        let mut log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(log.identity().await.unwrap(), id);
        assert_eq!(log.head(), head);
    });
}
