use std::sync::Arc;

use kolorinko::{
    fadeno::{
        bridge::FadenoModule,
        compiler::{compile_file, find_binary},
        types::*,
    },
    types::*,
};

mod common;
use common::{wire_event, FadenoTestCluster};

fn setup() -> Option<Arc<FadenoModule>> {
    let binary = find_binary()?;
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fad/wiki");
    let output = compile_file(&binary, &path)
        .ignore_type_error()
        .expect("wiki compilation failed");
    let module = FadenoModule::new(output.bytecode).expect("wiki bootstrap failed");
    Some(Arc::new(module))
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

    let mut tc = FadenoTestCluster::start(&[2, 3, 4], module.clone());
    let invite_mt = tc.msg_type(b"Invite");
    let invites_count = tc
        .tags()
        .record_get(tc.module().exports(), b"invites_count")
        .expect("missing invites_count")
        .clone();

    let alice = tc.add_user(
        SenderPk([42u8; 32]),
        UserId {
            id: 1,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );
    let bob = tc.add_user(
        SenderPk([2u8; 32]),
        UserId {
            id: 2,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );
    let carol = tc.add_user(
        SenderPk([3u8; 32]),
        UserId {
            id: 3,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );
    let dave = tc.add_user(
        SenderPk([10u8; 32]),
        UserId {
            id: 10,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );

    let b0 = tc.add_seed_branch(invite_mt, alice);
    let gear_0 = tc.build_gear(
        invites_count.clone(),
        vec![LocValue::KolDataId(LocDataId(b0.0))],
    );

    tc.post_events(
        vec![
            wire_event(
                alice,
                0,
                invite_mt,
                LocValue::KolDataId(LocDataId(b0.0)),
                LocValue::KolUserId(LocUserId(bob.0 as u64)),
            ),
            wire_event(
                alice,
                1,
                invite_mt,
                LocValue::KolDataId(LocDataId(b0.0)),
                LocValue::KolUserId(LocUserId(carol.0 as u64)),
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
            LocValue::KolDataId(LocDataId(b0.0)),
            LocValue::KolUserId(LocUserId(dave.0 as u64)),
        )],
        2,
    );

    let output = tc.run_gear(gear_0.clone());
    let count = match output {
        LocValue::Num(n) => n,
        _ => 0,
    };
    assert_eq!(count, 3);

    let b1 = tc.add_seed_branch(invite_mt, alice);
    let gear_1 = tc.build_gear(invites_count, vec![LocValue::KolDataId(LocDataId(b1.0))]);

    tc.post_events(
        vec![wire_event(
            alice,
            3,
            invite_mt,
            LocValue::KolDataId(LocDataId(b1.0)),
            LocValue::KolUserId(LocUserId(dave.0 as u64)),
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
