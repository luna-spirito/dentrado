//! The acked head: which loss is damage, which is an unfinished append.
//!
//! Everything here is a probe of one line — damage at or below the acked head
//! refuses to open, anything past it may be cut — and of the seals that make
//! the line decidable.

use dentrado_journal::segment;
use dentrado_journal::{Cursor, Decl, Error, Frame, Journal, Options};

mod common;
use common::{counter, flip, payloads, records, run, segments, truncate, write_at};

#[test]
fn the_segment_count_tracks_the_files_on_disk() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open_with(dir.path(), Options { rotate_bytes: 128 })
            .await
            .unwrap();
        let schema = counter(&mut log).await;
        for round in 0..20u8 {
            log.append(&[Frame::new(schema, &[round; 16])])
                .await
                .unwrap();
        }
        assert!(log.segments() > 1, "the writes crossed a rotation");
        assert_eq!(log.segments() as usize, segments(dir.path()).len());
        log.close().await.unwrap();
        drop(log);

        let log = Journal::open(dir.path()).await.unwrap();
        assert_eq!(
            log.segments() as usize,
            segments(dir.path()).len(),
            "a reopen counts the same files"
        );
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

#[test]
fn a_record_in_a_later_segment_may_name_a_lost_declaration() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let options = Options { rotate_bytes: 128 };
        let mut log = Journal::open_with(dir.path(), options).await.unwrap();
        let schema = counter(&mut log).await;
        for round in 0..3u8 {
            log.append(&[Frame::new(schema, &[round; 16])])
                .await
                .unwrap();
        }
        log.sync().await.unwrap();
        let decls = std::fs::metadata(&segments(dir.path())[0]).unwrap().len();

        // A declaration and a record that names it, in the *next* segment.
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
        truncate(&segments(dir.path())[0], decls);

        let log = Journal::open(dir.path()).await.unwrap();
        assert!(log.truncated() > 0, "the unacked record is cut");
        let seen = payloads(&log).await;
        assert_eq!(seen.len(), 3, "the acked records are untouched");
    });
}

#[test]
fn a_zeroed_declaration_header_beside_data_is_damage() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut log = Journal::open(dir.path()).await.unwrap();
        let schema = counter(&mut log).await;
        log.append(&[Frame::new(schema, b"unacked")]).await.unwrap();
        drop(log);

        // No sync, so nothing here is promised — and yet the data segments
        // cannot be interpreted without the declarations, so this is damage.
        let decls = segments(dir.path())[0].clone();
        write_at(&decls, 0, &[0u8; segment::HEADER_LEN]);

        let opened = Journal::open(dir.path()).await.map(|_| ());
        assert!(
            matches!(opened, Err(Error::Corrupt { .. })),
            "declarations behind data are not recreated: {opened:?}"
        );
    });
}

#[test]
fn a_torn_creation_header_is_forgiven() {
    run(async {
        for torn in [0u8, 2] {
            let dir = tempfile::tempdir().unwrap();
            let mut log = Journal::open(dir.path()).await.unwrap();
            let schema = counter(&mut log).await;
            log.append(&[Frame::new(schema, b"acked")]).await.unwrap();
            log.close().await.unwrap();
            drop(log);

            // A crash mid-`create`: either the version never landed (`0`), or
            // the segment id did not (`2` leaves it naming segment zero).
            let mut header = vec![0u8; segment::HEADER_LEN];
            header[..4].copy_from_slice(b"DJNL");
            let version = if torn == 0 { 0 } else { segment::LOG_VERSION };
            header[4..6].copy_from_slice(&version.to_le_bytes());
            header[6..8].copy_from_slice(&(segment::HEADER_LEN as u16).to_le_bytes());
            let named: u64 = if torn == 0 { 2 } else { 0 };
            header[8..16].copy_from_slice(&named.to_le_bytes());
            std::fs::write(dir.path().join(segment::file_name(2)), header).unwrap();

            let log = Journal::open(dir.path()).await;
            assert!(log.is_ok(), "torn create {torn} must be reused: {log:?}");
        }
    });
}
