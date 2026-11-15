//! Damage: what a segment may lose, and what must never be forgiven.
//!
//! Torn appends and rotted bytes look the same from inside a segment, so the
//! rules here are about position — sealed or not, the segment's own frames —
//! and the acked head has its own file.

use std::ops::ControlFlow;

use dentrado_journal::segment;
use dentrado_journal::{Cursor, Error, Frame, Journal, Options};
use proptest::prelude::*;

mod common;
use common::{copy_dir, counter, flip, payloads, records, run, segments, truncate, write_at};

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
fn a_cut_unacked_tail_is_repaired() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"kept")]).await.unwrap();
        log.close().await.unwrap();
        log.append(&[Frame::new(schema, b"new")]).await.unwrap();
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

#[test]
fn a_payload_that_looks_like_a_footer_is_not_a_footer() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        // Exactly the last 44 bytes of the file, starting with the magic.
        let mut payload = [0u8; 44];
        payload[..4].copy_from_slice(b"DJNF");
        log.append(&[Frame::new(schema, &payload)]).await.unwrap();
        log.sync().await.unwrap();
        drop(log);

        let log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(payloads(&log).await, [payload.to_vec()]);
        assert_eq!(log.truncated(), 0, "nothing was cut from a healthy log");
    });
}

#[test]
fn a_torn_footer_is_not_damage() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        for payload in [&b"a"[..], b"b"] {
            log.append(&[Frame::new(schema, payload)]).await.unwrap();
        }
        log.close().await.unwrap();
        drop(log);

        // A crash while the footer was written: the magic landed, nothing else.
        let file = segments(dir.path()).pop().unwrap();
        let len = std::fs::metadata(&file).unwrap().len();
        let mut torn = [0u8; segment::FOOTER_LEN];
        torn[..4].copy_from_slice(b"DJNF");
        write_at(&file, len - torn.len() as u64, &torn);

        let mut log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(payloads(&log).await, [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(log.truncated(), segment::FOOTER_LEN as u64);

        // The seal is gone, the records are not: it may be sealed again.
        log.close().await.unwrap();
        drop(log);
        let log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(payloads(&log).await, [b"a".to_vec(), b"b".to_vec()]);
    });
}

#[test]
fn a_flip_in_the_segment_being_written_is_damage() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"acked")]).await.unwrap();
        log.sync().await.unwrap();

        // The handle holds the directory's lock: nobody else is writing, so a
        // byte that changes under us is rot, not a concurrent append.
        let files = segments(dir.path());
        flip(&files[1], 32 + 4 + 2);

        assert!(
            matches!(log.verify().await, Err(Error::Corrupt { .. })),
            "verify must not forgive damage in the segment it is writing"
        );
    });
}

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
