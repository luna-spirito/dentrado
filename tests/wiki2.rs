use std::sync::Arc;

use kolorinko::{
    core::{
        gear::Runtime,
        loc_ctx::{EventContext, LocCtx, StoredEvent},
    },
    fadeno::{
        bridge::{FadenoModule, FadenoRuntime},
        compiler::{compile_file, find_binary},
        types::*,
    },
    types::*,
    utils::{
        state_graph::StateGraphOut,
        text::{AnchorPos, TextAgg, TextUpd, ROOT_ANCHOR},
    },
    wire::WireLocCtxBuilder,
};

mod common;
use common::wire_event;

use crate::common::WikiTestCluster;

fn setup_wiki2() -> Option<FadenoModule> {
    let binary = find_binary()?;
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fad/wiki2");
    let output = compile_file(&binary, &path)
        .ignore_type_error()
        .expect("wiki2 compilation failed");
    let module = match FadenoModule::new(output.bytecode) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("wiki2 bootstrap failed: {e:?}");
            return None;
        }
    };
    Some(module)
}

fn extract_invited_pairs(output: LocValue) -> Vec<(LocUserId, bool)> {
    let sg = match output {
        LocValue::KolStateGraphOut(sg) => sg,
        other => panic!("expected KolStateGraphOut, got {other:?}"),
    };
    let mut result = Vec::new();
    for (key, timeline) in sg.iter() {
        let uid = match key {
            LocValue::KolUserId(id) => *id,
            other => panic!("expected KolUserId key, got {other:?}"),
        };
        if let Some((_, b_val)) = timeline.last() {
            if let LocValue::Bool(b) = b_val {
                result.push((uid, *b));
            }
        }
    }
    result
}

fn extract_text_sg(
    output: &LocValue,
    tags: &TagRegistry,
) -> Box<StateGraphOut<LocValue, LocValue>> {
    let text_out = tags.record_get(output, b"text").expect("missing .text");
    match text_out {
        LocValue::KolStateGraphOut(sg) => sg,
        other => panic!("expected KolStateGraphOut for .text, got {other:?}"),
    }
}

fn has_non_empty_text(sg: &StateGraphOut<LocValue, LocValue>) -> bool {
    sg.iter().any(|(_, timeline)| {
        timeline
            .iter()
            .any(|(_, v)| matches!(v, LocValue::KolTextAgg(ta) if ta.clone() != TextAgg::default()))
    })
}

#[test]
fn invited_simple_e2e() {
    let module = if let Some(m) = setup_wiki2() {
        m
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let mut tc = WikiTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let tags = tc.tags().clone();
    let exports = tc.module().exports().clone();

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

    let alice = tc.add_user(SenderPk([1u8; 32]), alice_uid);
    let bob = tc.add_user(SenderPk([2u8; 32]), bob_uid);
    let carol = tc.add_user(SenderPk([3u8; 32]), carol_uid);

    let alice_loc_uid = tc.mk_loc_user(alice_uid);
    let b0 = tc.add_seed_branch(invite_mt, alice_loc_uid);

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
            wire_event(
                bob,
                2,
                invite_mt,
                LocValue::KolDataId(b0),
                tc.kol_user_id(carol_uid),
            ),
        ],
        1,
    );

    let invited_closure = tags
        .record_get(&exports, b"invited")
        .expect("missing invited export")
        .clone();
    let gear = tc.build_gear(invited_closure, vec![LocValue::KolDataId(b0)]);
    let output = tc.run_gear(gear);
    let pairs = extract_invited_pairs(output);
    let invited_count = pairs.iter().filter(|(_, b)| *b).count();

    assert_eq!(
        invited_count, 2,
        "expected 2 explicitly invited users, got {:?}",
        pairs
    );
    assert!(
        pairs.iter().all(|(_, b)| *b),
        "all should be invited, got {:?}",
        pairs
    );
}

#[test]
fn doc_content_same_core_e2e() {
    let module = if let Some(m) = setup_wiki2() {
        m
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let mut tc = WikiTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let attach_mt = tc.msg_type(b"Attach");
    let tags = tc.tags().clone();
    let exports = tc.module().exports().clone();

    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let bob_uid = UserId {
        id: 2,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let eve_uid = UserId {
        id: 3,
        identity_server_pk: IdentityServerPk([0; 32]),
    };

    let alice = tc.add_user(SenderPk([1u8; 32]), alice_uid);
    let bob = tc.add_user(SenderPk([2u8; 32]), bob_uid);
    let eve = tc.add_user(SenderPk([3u8; 32]), eve_uid);

    let alice_loc_uid = tc.mk_loc_user(alice_uid);
    let b0 = tc.add_seed_branch(invite_mt, alice_loc_uid);

    tc.post_events(
        vec![wire_event(
            alice,
            0,
            invite_mt,
            LocValue::KolDataId(b0),
            tc.kol_user_id(bob_uid),
        )],
        1,
    );

    let doc_id: u64 = 1;

    let text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Hello from Bob".to_string()],
    );
    let bob_attach_body = tags.make_record(&[
        (b"branch", LocValue::KolDataId(b0)),
        (b"is_merge", LocValue::Bool(false)),
        (b"edit", LocValue::KolTextUpd(text_upd)),
    ]);

    tc.post_events(
        vec![wire_event(
            bob,
            1,
            attach_mt,
            LocValue::Num(doc_id as i64),
            bob_attach_body,
        )],
        2,
    );

    let eve_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Eve was here".to_string()],
    );
    let eve_attach_body = tags.make_record(&[
        (b"branch", LocValue::KolDataId(b0)),
        (b"is_merge", LocValue::Bool(false)),
        (b"edit", LocValue::KolTextUpd(eve_text_upd)),
    ]);

    tc.post_events(
        vec![wire_event(
            eve,
            2,
            attach_mt,
            LocValue::Num(doc_id as i64),
            eve_attach_body,
        )],
        3,
    );

    let invited_closure = tags
        .record_get(&exports, b"invited")
        .expect("missing invited export")
        .clone();
    let _invited_output = tc.build_and_run_gear(invited_closure, vec![LocValue::KolDataId(b0)]);

    let doc_content_closure = tags
        .record_get(&exports, b"doc_content")
        .expect("missing doc_content export")
        .clone();
    let output = tc.build_and_run_gear(doc_content_closure, vec![LocValue::Num(doc_id as i64)]);

    let sg = extract_text_sg(&output, &tags);

    let entries: Vec<_> = sg.iter().collect();
    assert_eq!(
        entries.len(),
        1,
        "expected 1 branch in text output, got {}",
        entries.len()
    );

    let branch_key = LocValue::KolDataId(b0);
    assert_eq!(entries[0].0, &branch_key, "branch key mismatch");

    let timeline = entries[0].1;
    let (_, text_agg_val) = timeline.last().expect("timeline should not be empty");
    let LocValue::KolTextAgg(text_agg) = text_agg_val else {
        panic!("expected KolTextAgg, got {text_agg_val:?}");
    };

    assert_ne!(
        text_agg,
        &TextAgg::default(),
        "TextAgg should not be empty — Bob's edit should have been applied"
    );
}

#[test]
fn doc_content_cross_core_e2e() {
    let module = if let Some(m) = setup_wiki2() {
        m
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let mut tc = WikiTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let attach_mt = tc.msg_type(b"Attach");
    let tags = tc.tags().clone();
    let exports = tc.module().exports().clone();

    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let bob_uid = UserId {
        id: 2,
        identity_server_pk: IdentityServerPk([0; 32]),
    };

    let alice = tc.add_user(SenderPk([1u8; 32]), alice_uid);
    let bob = tc.add_user(SenderPk([2u8; 32]), bob_uid);

    let alice_loc_uid = tc.mk_loc_user(alice_uid);
    let b0 = tc.add_seed_branch(invite_mt, alice_loc_uid);

    let invited_closure = tags
        .record_get(&exports, b"invited")
        .expect("missing invited export")
        .clone();
    let invited_gear = tc.build_gear(invited_closure, vec![LocValue::KolDataId(b0)]);
    let invited_core = {
        let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear.clone());
        FadenoRuntime::route_group(invited_gear_wire.group(), &invited_wire_ctx)
            .unwrap()
            .route(2)
    };

    let doc_id = tc.find_cross_core_doc_id(invited_core, 2);
    eprintln!("found doc_id={doc_id} (invited → core {invited_core})");

    tc.post_events(
        vec![wire_event(
            alice,
            0,
            invite_mt,
            LocValue::KolDataId(b0),
            tc.kol_user_id(bob_uid),
        )],
        5,
    );

    let text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Hello from Bob".to_string()],
    );
    let bob_attach_body = tags.make_record(&[
        (b"branch", LocValue::KolDataId(b0)),
        (b"is_merge", LocValue::Bool(false)),
        (b"edit", LocValue::KolTextUpd(text_upd)),
    ]);

    tc.post_events(
        vec![wire_event(
            bob,
            1,
            attach_mt,
            LocValue::Num(doc_id as i64),
            bob_attach_body,
        )],
        6,
    );

    let doc_content_closure = tags
        .record_get(&exports, b"doc_content")
        .expect("missing doc_content export")
        .clone();
    let doc_gear = tc.build_gear(doc_content_closure, vec![LocValue::Num(doc_id as i64)]);
    let (doc_gear_wire, doc_wire_ctx) = tc.remap_gear(doc_gear.clone());

    let doc_core = FadenoRuntime::route_group(doc_gear_wire.group(), &doc_wire_ctx)
        .unwrap()
        .route(2);
    assert_ne!(
        invited_core, doc_core,
        "gears must be on different cores for cross-core test"
    );

    let output1 = tc.run_gear(doc_gear.clone());
    let sg1 = extract_text_sg(&output1, &tags);
    assert!(
        !has_non_empty_text(&sg1),
        "first run should have no real text (placeholder invited), but found non-empty TextAgg"
    );

    std::thread::sleep(std::time::Duration::from_millis(10));

    let output2 = tc.run_gear(doc_gear);
    let sg2 = extract_text_sg(&output2, &tags);
    assert!(
        has_non_empty_text(&sg2),
        "second run should have real text (populated secondary cache), but found none"
    );
}
