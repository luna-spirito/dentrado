//! Damage: which loss is repaired, which is refused, and on what evidence.
//!
//! The dividing line is the acked head: bytes the log promised must be there,
//! bytes it was still writing may be gone. Everything here is a probe of that
//! line from one side or the other.

use std::ops::ControlFlow;

use dentrado_journal::{Cursor, Decl, Error, Frame, Journal, Options};
use proptest::prelude::*;

mod common;
use common::{copy_dir, counter, flip, records, run, segments, truncate};

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
fn a_cut_acked_tail_is_damage() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"acked")]).await.unwrap();
        log.sync().await.unwrap();
        drop(log);

        let files = segments(dir.path());
        let len = std::fs::metadata(&files[1]).unwrap().len();
        truncate(&files[1], len / 2);

        let opened = Journal::open(dir.path()).await.map(|_| ());
        assert!(
            matches!(opened, Err(Error::Corrupt { .. })),
            "bytes that were synced are promised, not optional: {opened:?}"
        );
    });
}

#[test]
fn damage_in_an_acked_tail_is_damage() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        for payload in [&b"a"[..], b"b", b"c", b"d"] {
            log.append(&[Frame::new(schema, payload)]).await.unwrap();
        }
        log.sync().await.unwrap();
        drop(log);

        let files = segments(dir.path());
        flip(&files[1], 32 + 4 + 2 + 1);

        let opened = Journal::open(dir.path()).await.map(|_| ());
        assert!(
            matches!(opened, Err(Error::Corrupt { .. })),
            "rot between acked records is rot, not a short segment: {opened:?}"
        );
    });
}

#[test]
fn a_rotted_header_of_an_acked_segment_is_damage() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"acked")]).await.unwrap();
        log.sync().await.unwrap();
        drop(log);

        let files = segments(dir.path());
        flip(&files[1], 0);

        let opened = Journal::open(dir.path()).await.map(|_| ());
        assert!(
            matches!(opened, Err(Error::Corrupt { .. })),
            "a header that was acked is not a crashed create: {opened:?}"
        );
    });
}

#[test]
fn a_crashed_create_past_the_acked_head_is_forgiven() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"acked")]).await.unwrap();
        log.close().await.unwrap();
        drop(log);

        // A crash between creating the next segment and writing its header.
        let next = dir.path().join(dentrado_journal::segment::file_name(2));
        std::fs::write(&next, [0u8; 32]).unwrap();

        let log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(log.head().segment, 2, "the empty segment is reused");
        assert_eq!(
            records(&log, Cursor::START)
                .await
                .into_iter()
                .filter(|(_, schema, _)| schema.is_user())
                .count(),
            1
        );
    });
}

#[test]
fn a_deleted_declaration_segment_is_damage() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"acked")]).await.unwrap();
        log.sync().await.unwrap();
        drop(log);

        std::fs::remove_file(&segments(dir.path())[0]).unwrap();

        let opened = Journal::open(dir.path()).await.map(|_| ());
        assert!(
            matches!(opened, Err(Error::Corrupt { .. })),
            "the declarations were acked, so their segment cannot vanish: {opened:?}"
        );
    });
}

#[test]
fn an_acked_head_that_names_a_missing_segment_is_damage() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"acked")]).await.unwrap();
        log.sync().await.unwrap();
        drop(log);

        let files = segments(dir.path());
        std::fs::remove_file(&files[1]).unwrap();

        let opened = Journal::open(dir.path()).await.map(|_| ());
        assert!(
            matches!(opened, Err(Error::Corrupt { .. })),
            "the log promised a segment that is gone: {opened:?}"
        );
    });
}

#[test]
fn a_record_that_names_an_unacked_declaration_is_dropped() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let kept = counter(&mut log).await;
        log.append(&[Frame::new(kept, b"kept")]).await.unwrap();
        log.sync().await.unwrap();

        // The declaration segment holds exactly the acked declarations now.
        let files = segments(dir.path());
        let decls = std::fs::metadata(&files[0]).unwrap().len();

        // A declaration and a record that names it, neither of them synced.
        let lost = log
            .intern(Decl {
                name: "example/lost",
                shape: "u8",
            })
            .await
            .unwrap();
        log.append(&[Frame::new(lost, b"lost")]).await.unwrap();
        drop(log);

        // The crash kept the record but lost the declaration it names.
        truncate(&files[0], decls);

        let log = Journal::open(dir.path()).await.unwrap();
        assert!(log.truncated() > 0, "the unacked record is cut");
        let payloads: Vec<_> = records(&log, Cursor::START)
            .await
            .into_iter()
            .filter(|(_, schema, _)| schema.is_user())
            .map(|(_, _, payload)| payload)
            .collect();
        assert_eq!(payloads, [b"kept".to_vec()]);
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
