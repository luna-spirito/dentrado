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
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fad/wiki");
    let output = compile_file(&binary, &path)
        .ignore_type_error()
        .expect("wiki compilation failed");
    Some(FadenoModule::new(output.bytecode).expect("wiki bootstrap failed"))
}

#[test]
fn wiki_vm() {
    let module = if let Some(m) = setup() {
        m
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let tags = module.tags();
    let exports = module.exports();

    let invite_id = match tags.record_get(exports, b"Invite") {
        Some(LocValue::KolEventTypeId(id)) => id,
        other => panic!("Invite: expected KolEventTypeId, got {other:?}"),
    };
    let invites_count = tags
        .record_get(exports, b"invites_count")
        .expect("missing invites_count")
        .clone();

    assert_eq!(invite_id.0, 0);
    assert!(matches!(invites_count, LocValue::Closure { .. }));
}

#[test]
fn wiki_engine() {
    let module = if let Some(m) = setup() {
        m
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let mut tc = WikiTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let invites_count = tc
        .tags()
        .record_get(tc.module().exports(), b"invites_count")
        .expect("missing invites_count")
        .clone();

    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let bob_uid = UserId {
        id: 2,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let carol_uid = UserId {
        id: 3,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let dave_uid = UserId {
        id: 10,
        identity_server_pk: IdentityServerPk([0; 32]),
    };

    let alice = tc.add_user(SenderPk([42u8; 32]), alice_uid);

    let alice_loc_uid = tc.mk_loc_user(alice_uid);
    let b0 = tc.add_seed_branch(invite_mt, alice_loc_uid);
    let gear_0 = tc.build_gear(invites_count.clone(), vec![LocValue::KolDataId(b0)]);

    tc.post_events(
        vec![
            wire_event(
                alice,
                0,
                invite_mt,
                LocValue::KolDataId(b0),
                tc.kol_user_id(bob_uid),
            ),
            wire_event(
                alice,
                1,
                invite_mt,
                LocValue::KolDataId(b0),
                tc.kol_user_id(carol_uid),
            ),
        ],
        1,
    );

    let output = tc.run_gear(gear_0.clone());
    let count = match output {
        LocValue::Num(n) => n,
        _ => 0,
    };
    assert_eq!(count, 2);

    tc.post_events(
        vec![wire_event(
            alice,
            2,
            invite_mt,
            LocValue::KolDataId(b0),
            tc.kol_user_id(dave_uid),
        )],
        2,
    );

    let output = tc.run_gear(gear_0.clone());
    let count = match output {
        LocValue::Num(n) => n,
        _ => 0,
    };
    assert_eq!(count, 3);

    let b1 = tc.add_seed_branch(invite_mt, alice_loc_uid);
    let gear_1 = tc.build_gear(invites_count, vec![LocValue::KolDataId(b1)]);

    tc.post_events(
        vec![wire_event(
            alice,
            3,
            invite_mt,
            LocValue::KolDataId(b1),
            tc.kol_user_id(dave_uid),
        )],
        4,
    );

    let output = tc.run_gear(gear_1);
    let count = match output {
        LocValue::Num(n) => n,
        _ => 0,
    };
    assert_eq!(count, 1, "branch 1 has 1 invite");

    let output = tc.run_gear(gear_0);
    let count = match output {
        LocValue::Num(n) => n,
        _ => 0,
    };
    assert_eq!(count, 3, "branch 0 still 3");
}
