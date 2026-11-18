//! The `dentrado` records against a real journal: interned once, named in
//! segment zero, and decoded back from a replayed frame.
//!
//! This is the seam between P2 (shapes and record layouts) and P3 (the fold):
//! if these hold, a future fold can trust that a frame's bytes mean what the
//! declared shape says.

use std::{collections::HashSet, ops::ControlFlow};

use dentrado::{
    core::storage::journal::{
        record::{Event, LocData, LocGroup, LocSender, LocUser},
        schema::declare,
    },
    types::Shape,
};
use dentrado_journal::{Cursor, Frame, Journal, SchemaId};

fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new()
        .expect("a runtime")
        .block_on(f)
}

fn check<T: Shape>(journal: &Journal, id: SchemaId, name: &str) {
    let decl = journal.decls().get(id).expect("a declared record");
    assert_eq!(decl.name, name);
    assert_eq!(decl.shape, T::shape());
}

#[test]
fn every_record_interns_once_and_keeps_its_name() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).await.unwrap();

        let user = declare::<LocUser>(&mut journal, LocUser::NAME)
            .await
            .unwrap();
        let sender = declare::<LocSender>(&mut journal, LocSender::NAME)
            .await
            .unwrap();
        let group = declare::<LocGroup>(&mut journal, LocGroup::NAME)
            .await
            .unwrap();
        let data = declare::<LocData>(&mut journal, LocData::NAME)
            .await
            .unwrap();
        let event = declare::<Event<'_>>(&mut journal, Event::NAME)
            .await
            .unwrap();

        // A second visit is a lookup, not a second declaration.
        assert_eq!(
            declare::<LocUser>(&mut journal, LocUser::NAME)
                .await
                .unwrap(),
            user
        );
        assert_eq!(
            declare::<Event<'_>>(&mut journal, Event::NAME)
                .await
                .unwrap(),
            event
        );

        let ids: HashSet<_> = [user, sender, group, data, event].into_iter().collect();
        assert_eq!(ids.len(), 5, "one id per layout");

        drop(journal);

        // Declarations are durable: a new process reads the same ids/names.
        let journal = Journal::open(dir.path()).await.unwrap();
        check::<LocUser>(&journal, user, LocUser::NAME);
        check::<LocSender>(&journal, sender, LocSender::NAME);
        check::<LocGroup>(&journal, group, LocGroup::NAME);
        check::<LocData>(&journal, data, LocData::NAME);
        check::<Event<'_>>(&journal, event, Event::NAME);
    });
}

#[test]
fn an_event_frame_round_trips_through_replay() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).await.unwrap();
        let schema = declare::<Event<'_>>(&mut journal, Event::NAME)
            .await
            .unwrap();

        let event = Event {
            group: 7,
            sender: 11,
            tx_id: 3,
            timestamp: 1_700_000_000,
            source_node: 2,
            body: b"payload",
        };
        journal
            .append(&[Frame::new(schema, &event.encode())])
            .await
            .unwrap();
        journal.sync().await.unwrap();

        let mut seen = 0;
        journal
            .replay_from(Cursor::START, async |_, frame: Frame<'_>| {
                if frame.schema == schema {
                    let parsed = Event::decode(frame.payload);
                    assert_eq!(parsed.as_ref(), Some(&event));
                    seen += 1;
                }
                ControlFlow::Continue(())
            })
            .await
            .unwrap();
        assert_eq!(seen, 1, "the event replays exactly once");
    });
}
