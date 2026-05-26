use std::sync::Arc;

use kolorinko::{
    core::gear::Runtime,
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
    wire::WireEventBody,
};

mod common;
use common::FadenoTestCluster;

fn setup_wiki2() -> Option<Arc<FadenoModule>> {
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
    Some(Arc::new(module))
}

fn make_invite_event(
    sender: LocSenderId,
    tx_id: u32,
    invite_mt: LocMsgTypeId,
    branch_did: LocDataId,
    invitee_uid: LocUserId,
) -> WireEventBody<LocValue, LocValue> {
    WireEventBody {
        sender,
        tx_id,
        msg_type: invite_mt,
        group: LocValue::KolDataId(branch_did),
        body: LocValue::KolUserId(invitee_uid),
    }
}

fn make_attach_edit_event(
    sender: LocSenderId,
    tx_id: u32,
    attach_mt: LocMsgTypeId,
    doc_id: u64,
    branch_did: LocDataId,
    text_upd: TextUpd,
    tags: &TagRegistry,
) -> WireEventBody<LocValue, LocValue> {
    let branch_val = LocValue::KolDataId(branch_did);
    let body = tags.make_record(&[
        (b"branch", branch_val),
        (b"is_merge", LocValue::Bool(false)),
        (b"edit", LocValue::KolTextUpd(text_upd)),
    ]);
    WireEventBody {
        sender,
        tx_id,
        msg_type: attach_mt,
        group: LocValue::Num(doc_id as i64),
        body,
    }
}

fn make_attach_fork_event(
    sender: LocSenderId,
    tx_id: u32,
    attach_mt: LocMsgTypeId,
    doc_id: u64,
    child_branch_did: LocDataId,
    parent_branch_did: LocDataId,
    tags: &TagRegistry,
) -> WireEventBody<LocValue, LocValue> {
    let child_branch = LocValue::KolDataId(child_branch_did);
    let parent_branch = LocValue::KolDataId(parent_branch_did);
    let body = tags.make_record(&[
        (b"branch", child_branch),
        (b"is_merge", LocValue::Bool(true)),
        (b"from", parent_branch),
    ]);
    WireEventBody {
        sender,
        tx_id,
        msg_type: attach_mt,
        group: LocValue::Num(doc_id as i64),
        body,
    }
}

fn has_non_empty_text(sg: &StateGraphOut<LocValue, LocValue>) -> bool {
    sg.iter().any(|(_, timeline)| {
        timeline
            .iter()
            .any(|(_, v)| matches!(v, LocValue::KolTextAgg(ta) if ta.clone() != TextAgg::default()))
    })
}

fn count_branches_with_text(sg: &StateGraphOut<LocValue, LocValue>) -> usize {
    sg.iter()
        .filter(|(_, timeline)| {
            timeline.iter().any(
                |(_, v)| matches!(v, LocValue::KolTextAgg(ta) if ta.clone() != TextAgg::default()),
            )
        })
        .count()
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

fn extract_branch_text_agg(
    sg: &StateGraphOut<LocValue, LocValue>,
    branch_did: LocDataId,
) -> TextAgg {
    let branch_key = LocValue::KolDataId(branch_did);
    let timeline = sg
        .iter()
        .find(|(k, _)| **k == branch_key)
        .map(|(_, tl)| tl)
        .expect("branch should be in StateGraphOut");
    let (_, val) = timeline.last().expect("timeline not empty");
    match val {
        LocValue::KolTextAgg(ta) => (*ta).clone(),
        other => panic!("expected KolTextAgg, got {other:?}"),
    }
}

#[test]
fn retroactive_invite_cross_core_e2e() {
    let module = if let Some(m) = setup_wiki2() {
        m
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let mut tc = FadenoTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let attach_mt = tc.msg_type(b"Attach");
    let tags = tc.tags().clone();

    let alice = tc.add_user(
        SenderPk([1u8; 32]),
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
        SenderPk([4u8; 32]),
        UserId {
            id: 4,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );
    let eve = tc.add_user(
        SenderPk([5u8; 32]),
        UserId {
            id: 5,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );

    let b0 = tc.add_seed_branch(invite_mt, alice);

    let invited_gear = tc.build_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"invited")
            .expect("missing invited")
            .clone(),
        vec![LocValue::KolDataId(LocDataId(b0.0))],
    );
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = FadenoRuntime::route_group(invited_gear_wire.group(), &invited_wire_ctx)
        .unwrap()
        .route(2);

    let doc_id = tc.find_cross_core_doc_id(invited_core, 2);
    eprintln!("retroactive_invite: doc_id={doc_id}, invited_core={invited_core}");

    tc.post_events(
        vec![make_invite_event(alice, 1, invite_mt, b0, LocUserId(bob.0))],
        2,
    );

    let bob_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Hello from Bob".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            bob,
            2,
            attach_mt,
            doc_id,
            b0,
            bob_text_upd,
            &tags,
        )],
        3,
    );

    let carol_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Carol was here".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            carol,
            3,
            attach_mt,
            doc_id,
            b0,
            carol_text_upd,
            &tags,
        )],
        4,
    );

    tc.post_events(
        vec![make_invite_event(
            alice,
            4,
            invite_mt,
            b0,
            LocUserId(carol.0),
        )],
        5,
    );

    tc.post_events(
        vec![make_invite_event(bob, 5, invite_mt, b0, LocUserId(dave.0))],
        6,
    );

    let dave_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Dave says hi".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            dave,
            6,
            attach_mt,
            doc_id,
            b0,
            dave_text_upd,
            &tags,
        )],
        7,
    );

    let eve_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Eve snoops".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            eve,
            7,
            attach_mt,
            doc_id,
            b0,
            eve_text_upd,
            &tags,
        )],
        8,
    );

    let doc_gear = tc.build_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"doc_content")
            .expect("missing doc_content")
            .clone(),
        vec![LocValue::Num(doc_id as i64)],
    );
    let (doc_gear_wire, doc_wire_ctx) = tc.remap_gear(doc_gear.clone());

    let doc_core = FadenoRuntime::route_group(doc_gear_wire.group(), &doc_wire_ctx)
        .unwrap()
        .route(2);
    assert_ne!(invited_core, doc_core, "gears must be on different cores");

    let output1 = tc.run_gear(doc_gear.clone());
    let sg1 = extract_text_sg(&output1, &tags);
    assert!(
        !has_non_empty_text(&sg1),
        "run 1: should have no real text (placeholder invited), but found non-empty TextAgg"
    );

    std::thread::sleep(std::time::Duration::from_millis(50));

    let output2 = tc.run_gear(doc_gear.clone());
    let sg2 = extract_text_sg(&output2, &tags);

    assert!(
        has_non_empty_text(&sg2),
        "run 2: should have real text from Bob and Dave"
    );
    assert_eq!(
        count_branches_with_text(&sg2),
        1,
        "run 2: exactly one branch should have text"
    );

    let text_agg = extract_branch_text_agg(&sg2, b0);
    assert_ne!(
        text_agg,
        TextAgg::default(),
        "B0 TextAgg should not be empty — Bob and Dave's edits should be applied"
    );

    let output3 = tc.run_gear(doc_gear);
    let sg3 = extract_text_sg(&output3, &tags);
    assert_eq!(
        count_branches_with_text(&sg3),
        1,
        "run 3: output should be stable (no new dep changes)"
    );
}

#[test]
fn text_agg_merge_cross_core_e2e() {
    let module = if let Some(m) = setup_wiki2() {
        m
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let mut tc = FadenoTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let attach_mt = tc.msg_type(b"Attach");
    let tags = tc.tags().clone();

    let alice = tc.add_user(
        SenderPk([1u8; 32]),
        UserId {
            id: 1,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );
    let carol = tc.add_user(
        SenderPk([2u8; 32]),
        UserId {
            id: 2,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );
    let eve = tc.add_user(
        SenderPk([3u8; 32]),
        UserId {
            id: 3,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );

    let b0 = tc.add_seed_branch(invite_mt, alice);
    let b1 = tc.add_seed_branch(invite_mt, carol);

    let invited_gear = tc.build_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"invited")
            .expect("missing invited")
            .clone(),
        vec![LocValue::KolDataId(LocDataId(b0.0))],
    );
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = FadenoRuntime::route_group(invited_gear_wire.group(), &invited_wire_ctx)
        .unwrap()
        .route(2);

    let doc_id = tc.find_cross_core_doc_id(invited_core, 2);
    eprintln!("text_agg_merge: doc_id={doc_id}, invited_core={invited_core}");

    let alice_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["AAA".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            alice,
            2,
            attach_mt,
            doc_id,
            b0,
            alice_text_upd,
            &tags,
        )],
        11,
    );

    let carol_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["BBB".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            carol,
            3,
            attach_mt,
            doc_id,
            b1,
            carol_text_upd,
            &tags,
        )],
        12,
    );

    tc.post_events(
        vec![make_attach_fork_event(
            carol, 4, attach_mt, doc_id, b1, b0, &tags,
        )],
        13,
    );

    let eve_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Eve ignored".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            eve,
            5,
            attach_mt,
            doc_id,
            b1,
            eve_text_upd,
            &tags,
        )],
        14,
    );

    let doc_gear = tc.build_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"doc_content")
            .expect("missing doc_content")
            .clone(),
        vec![LocValue::Num(doc_id as i64)],
    );
    let (doc_gear_wire, doc_wire_ctx) = tc.remap_gear(doc_gear.clone());

    let doc_core = FadenoRuntime::route_group(doc_gear_wire.group(), &doc_wire_ctx)
        .unwrap()
        .route(2);
    assert_ne!(invited_core, doc_core, "gears must be on different cores");

    let output1 = tc.run_gear(doc_gear.clone());
    let sg1 = extract_text_sg(&output1, &tags);
    assert!(
        has_non_empty_text(&sg1),
        "run 1: should have text from branch creators even with placeholder invited"
    );

    std::thread::sleep(std::time::Duration::from_millis(50));

    let output2 = tc.run_gear(doc_gear.clone());
    let sg2 = extract_text_sg(&output2, &tags);

    assert!(
        has_non_empty_text(&sg2),
        "run 2: should have real text from invited users"
    );
    assert_eq!(
        count_branches_with_text(&sg2),
        2,
        "run 2: both B0 and B1 should have text"
    );

    let b0_text_agg = extract_branch_text_agg(&sg2, b0);
    let b1_text_agg = extract_branch_text_agg(&sg2, b1);

    assert_ne!(
        b0_text_agg,
        TextAgg::default(),
        "B0 TextAgg should not be empty"
    );
    assert_ne!(
        b1_text_agg,
        TextAgg::default(),
        "B1 TextAgg should not be empty"
    );

    assert_ne!(
        b1_text_agg, b0_text_agg,
        "B1's TextAgg should differ from B0's — merge must have combined both branches' content"
    );

    let output3 = tc.run_gear(doc_gear);
    let sg3 = extract_text_sg(&output3, &tags);
    assert_eq!(
        count_branches_with_text(&sg3),
        2,
        "run 3: output should be stable"
    );
}

#[test]
fn multi_user_doc_assembly_cross_core_e2e() {
    let module = if let Some(m) = setup_wiki2() {
        m
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let mut tc = FadenoTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let attach_mt = tc.msg_type(b"Attach");
    let tags = tc.tags().clone();

    let alice = tc.add_user(
        SenderPk([1u8; 32]),
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
        SenderPk([4u8; 32]),
        UserId {
            id: 4,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );
    let eve = tc.add_user(
        SenderPk([5u8; 32]),
        UserId {
            id: 5,
            identity_server_pk: IdentityServerPk([0; 32]),
        },
    );

    let b0 = tc.add_seed_branch(invite_mt, alice);

    let invited_gear = tc.build_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"invited")
            .expect("missing invited")
            .clone(),
        vec![LocValue::KolDataId(LocDataId(b0.0))],
    );
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = FadenoRuntime::route_group(invited_gear_wire.group(), &invited_wire_ctx)
        .unwrap()
        .route(2);

    let doc_id = tc.find_cross_core_doc_id(invited_core, 2);
    eprintln!("multi_user_doc_assembly: doc_id={doc_id}, invited_core={invited_core}");

    tc.post_events(
        vec![
            make_invite_event(alice, 1, invite_mt, b0, LocUserId(bob.0)), // Alice→Bob
            make_invite_event(alice, 2, invite_mt, b0, LocUserId(carol.0)), // Alice→Carol
            make_invite_event(alice, 3, invite_mt, b0, LocUserId(dave.0)), // Alice→Dave
        ],
        16,
    );

    let alice_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Hello".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            alice,
            3,
            attach_mt,
            doc_id,
            b0,
            alice_text_upd,
            &tags,
        )],
        17,
    );

    let bob_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["World".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            bob,
            4,
            attach_mt,
            doc_id,
            b0,
            bob_text_upd,
            &tags,
        )],
        18,
    );

    let carol_text_upd = TextUpd::new(vec![AnchorPos::new(ROOT_ANCHOR, 0)], vec!["!".to_string()]);
    tc.post_events(
        vec![make_attach_edit_event(
            carol,
            5,
            attach_mt,
            doc_id,
            b0,
            carol_text_upd,
            &tags,
        )],
        19,
    );

    let dave_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec![" [Dave]".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            dave,
            6,
            attach_mt,
            doc_id,
            b0,
            dave_text_upd,
            &tags,
        )],
        20,
    );

    let eve_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["[ignored]".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            eve,
            7,
            attach_mt,
            doc_id,
            b0,
            eve_text_upd,
            &tags,
        )],
        21,
    );

    let doc_gear = tc.build_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"doc_content")
            .expect("missing doc_content")
            .clone(),
        vec![LocValue::Num(doc_id as i64)],
    );
    let (doc_gear_wire, doc_wire_ctx) = tc.remap_gear(doc_gear.clone());

    let doc_core = FadenoRuntime::route_group(doc_gear_wire.group(), &doc_wire_ctx)
        .unwrap()
        .route(2);
    assert_ne!(invited_core, doc_core, "gears must be on different cores");

    let output1 = tc.run_gear(doc_gear.clone());
    let sg1 = extract_text_sg(&output1, &tags);
    assert!(
        has_non_empty_text(&sg1),
        "run 1: should have text from Alice (creator) even with placeholder invited"
    );
    assert_eq!(
        count_branches_with_text(&sg1),
        1,
        "run 1: only one branch (B0) should have text with placeholder invited"
    );

    std::thread::sleep(std::time::Duration::from_millis(50));

    let output2 = tc.run_gear(doc_gear.clone());
    let sg2 = extract_text_sg(&output2, &tags);

    assert!(
        has_non_empty_text(&sg2),
        "run 2: should have real text from invited users"
    );
    assert_eq!(
        count_branches_with_text(&sg2),
        1,
        "run 2: exactly one branch should have text"
    );

    let text_agg = extract_branch_text_agg(&sg2, b0);
    assert_ne!(
        text_agg,
        TextAgg::default(),
        "B0 TextAgg should not be empty — invited users' edits should be applied"
    );

    let anchors_out = tags
        .record_get(&output2, b"anchors")
        .expect("doc_content output missing .anchors");
    assert!(
        matches!(anchors_out, LocValue::KolAnchorAgg(_)),
        "expected KolAnchorAgg for .anchors, got {anchors_out:?}"
    );

    let output3 = tc.run_gear(doc_gear);
    let sg3 = extract_text_sg(&output3, &tags);
    assert_eq!(
        count_branches_with_text(&sg3),
        1,
        "run 3: output should be stable"
    );
}

#[test]
fn retroactive_invite_point_in_time_same_core_e2e() {
    let module = if let Some(m) = setup_wiki2() {
        m
    } else {
        eprintln!("skipping: fadeno-lang not found");
        return;
    };

    let tags = module.tags().clone();

    let mut tc = FadenoTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let attach_mt = tc.msg_type(b"Attach");

    let alice = tc.add_user(
        SenderPk([1u8; 32]),
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

    let b0 = tc.add_seed_branch(invite_mt, alice);

    let doc_id: u64 = 42;
    let bob_text_upd_1 = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Bob before invite".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            bob,
            1,
            attach_mt,
            doc_id,
            b0,
            bob_text_upd_1,
            &tags,
        )],
        23,
    );

    tc.post_events(
        vec![make_invite_event(alice, 2, invite_mt, b0, LocUserId(bob.0))],
        24,
    );

    let bob_text_upd_2 = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Bob after invite".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(
            bob,
            3,
            attach_mt,
            doc_id,
            b0,
            bob_text_upd_2,
            &tags,
        )],
        25,
    );

    let _invited_output = tc.build_and_run_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"invited")
            .expect("missing invited")
            .clone(),
        vec![LocValue::KolDataId(LocDataId(b0.0))],
    );

    let output = tc.build_and_run_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"doc_content")
            .expect("missing doc_content")
            .clone(),
        vec![LocValue::Num(doc_id as i64)],
    );
    let sg = extract_text_sg(&output, &tags);

    assert!(
        has_non_empty_text(&sg),
        "doc_content should have text from Bob's post-invite edit"
    );

    let text_agg = extract_branch_text_agg(&sg, b0);
    assert_ne!(
        text_agg,
        TextAgg::default(),
        "B0 TextAgg should not be empty — Bob's post-invite edit should be applied"
    );
}
