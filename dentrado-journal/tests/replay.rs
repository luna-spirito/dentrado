//! Replay: order, the visitor, the resume cursor, and the scrub.

use std::ops::ControlFlow;

use dentrado_journal::{Cursor, Error, Frame, Journal, Options, ReplayError, SchemaId};

mod common;
use common::{counter, flip, records, run, segments};

#[test]
fn appends_replay_in_order() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(log.head(), Cursor::START);
        let schema = counter(&mut log).await;
        let head = log
            .append(&[Frame::new(schema, b"one"), Frame::new(schema, b"two")])
            .await
            .unwrap();
        log.sync().await.unwrap();
        assert_eq!(log.head(), head);

        let seen = records(&log, Cursor::START).await;
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].1, SchemaId::DECL);
        assert_eq!(seen[1], (seen[1].0, schema, b"one".to_vec()));
        assert_eq!(seen[2].2, b"two");
        assert!(seen[0].0 < seen[1].0 && seen[1].0 < seen[2].0);
    });
}

#[test]
fn a_visitor_may_await_on_the_record() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        for payload in [&b"one"[..], b"two", b"three"] {
            log.append(&[Frame::new(schema, payload)]).await.unwrap();
        }
        log.sync().await.unwrap();

        let sink = dir.path().join("sink");
        let mut file = compio::fs::File::create(&sink).await.unwrap();
        let mut at = 0u64;
        log.replay_from(Cursor::START, async |_, frame: Frame<'_>| {
            use compio::io::AsyncWriteAtExt;

            if frame.schema.is_user() {
                let compio::buf::BufResult(result, _) =
                    file.write_all_at(frame.payload.to_vec(), at).await;
                result.unwrap();
                at += frame.payload.len() as u64;
            }
            ControlFlow::Continue(())
        })
        .await
        .unwrap();
        drop(file);
        assert_eq!(std::fs::read(&sink).unwrap(), b"onetwothree");
    });
}

#[test]
fn a_replay_resumes_where_the_visitor_stopped() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        for payload in [&b"a"[..], b"b", b"c", b"d"] {
            log.append(&[Frame::new(schema, payload)]).await.unwrap();
        }
        let all = records(&log, Cursor::START).await;

        let mut seen = 0;
        let stopped = log
            .replay_from(Cursor::START, async |_, _| {
                seen += 1;
                if seen == 3 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .await
            .unwrap();
        assert_eq!(seen, 3);
        assert_eq!(records(&log, stopped).await, all[3..].to_vec());

        // A replay that runs to the end lands on the head.
        assert_eq!(records(&log, Cursor::START).await.len(), 5);
        let end = log
            .replay_from(Cursor::START, async |_, _| ControlFlow::Continue(()))
            .await
            .unwrap();
        assert_eq!(end, log.head());
    });
}

#[test]
fn a_failing_visitor_keeps_its_own_error() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        for payload in [&b"ok"[..], b"poison", b"never"] {
            log.append(&[Frame::new(schema, payload)]).await.unwrap();
        }

        let mut seen = Vec::new();
        let out: Result<Cursor, ReplayError<&str>> = log
            .try_replay_from(Cursor::START, async |_, frame: Frame<'_>| {
                if frame.schema.is_user() {
                    seen.push(frame.payload.to_vec());
                }
                if frame.payload == b"poison" {
                    return Err("poisoned");
                }
                Ok(ControlFlow::Continue(()))
            })
            .await;
        assert!(
            matches!(out, Err(ReplayError::Visit("poisoned"))),
            "{out:?}"
        );
        assert_eq!(
            seen,
            [b"ok".to_vec(), b"poison".to_vec()],
            "no more records"
        );
    });
}

#[test]
fn verify_notices_damage_that_open_does_not() {
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
        log.verify().await.unwrap();
        drop(log);

        let files = segments(dir.path());
        flip(&files[1], 32);
        let log = Journal::open(dir.path()).await.unwrap();
        assert!(matches!(log.verify().await, Err(Error::Corrupt { .. })));
    });
}
