use kolorinko::{
    fadeno::{
        bridge::FadenoModule,
        compiler::{compile_file, find_binary},
        types::*,
    },
    types::*,
};

mod common;
use common::wire_event;

use crate::common::WikiTestCluster;

fn setup() -> Option<FadenoModule> {
    let binary = find_binary()?;
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fad/mulsum4");
    let output = compile_file(&binary, &path)
        .ignore_type_error()
        .expect("mulsum4 compilation failed");
    Some(FadenoModule::new(output.bytecode).expect("mulsum4 bootstrap failed"))
}

fn full_setup() -> Option<WikiTestCluster> {
    let mut module = setup()?;

    // Ensure tag IDs for the MulSumEntry body record fields { .a, .b }.
    let tag_a = module.ensure_tag_id(b"a");
    let tag_b = module.ensure_tag_id(b"b");
    let _ = module.ensure_tag_set(&[tag_a, tag_b]);

    Some(WikiTestCluster::start(&[2, 3, 4], module))
}

#[test]
fn mulsum_vm() {
    let module = if let Some(m) = setup() {
        m
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let tags = module.tags();
    let exports = module.exports();

    let entry_id = match tags.record_get(exports, b"MulSumEntry") {
        Some(LocValue::KolEventTypeId(id)) => id,
        other => panic!("MulSumEntry: expected KolEventTypeId, got {other:?}"),
    };
    let mulsum_fn = tags
        .record_get(exports, b"mulsum")
        .expect("missing mulsum")
        .clone();

    assert_eq!(entry_id.0, 0);
    assert!(matches!(mulsum_fn, LocValue::Closure { .. }));
}

#[test]
fn mulsum_engine() {
    let mut tc = if let Some(c) = full_setup() {
        c
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let entry_mt = tc.msg_type(b"MulSumEntry");
    let mulsum_fn = tc
        .tags()
        .record_get(tc.module().exports(), b"mulsum")
        .expect("missing mulsum")
        .clone();

    let bucket: i64 = 0;

    // Register the bucket as a valid group for MulSumEntry events.
    tc.register_group(entry_mt, LocValue::Num(bucket));

    let gear = tc.build_gear(mulsum_fn, vec![LocValue::Num(bucket)]);

    let alice_pk = SenderPk([42u8; 32]);
    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let alice = tc.add_user(alice_pk, alice_uid);

    // Post two events: (3*4) + (5*2) = 12 + 10 = 22
    let body1 = tc
        .tags()
        .make_record(&[(b"a", LocValue::Num(3)), (b"b", LocValue::Num(4))]);
    let body2 = tc
        .tags()
        .make_record(&[(b"a", LocValue::Num(5)), (b"b", LocValue::Num(2))]);

    tc.post_events(
        vec![
            wire_event(alice, 0, entry_mt, LocValue::Num(bucket), body1),
            wire_event(alice, 1, entry_mt, LocValue::Num(bucket), body2),
        ],
        1,
    );

    let output = tc.run_gear(gear.clone());
    let sum = match output {
        LocValue::Num(n) => n,
        _ => 0,
    };
    assert_eq!(sum, 22);

    // Post a third event: (7*1) = 7, total = 29
    let body3 = tc
        .tags()
        .make_record(&[(b"a", LocValue::Num(7)), (b"b", LocValue::Num(1))]);

    tc.post_events(
        vec![wire_event(alice, 2, entry_mt, LocValue::Num(bucket), body3)],
        2,
    );

    let output = tc.run_gear(gear);
    let sum = match output {
        LocValue::Num(n) => n,
        _ => 0,
    };
    assert_eq!(sum, 29);
}
