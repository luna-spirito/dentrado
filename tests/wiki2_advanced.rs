use kolorinko::{
    core::{gear::Runtime, loc_ctx::LocCtx},
    fadeno::{
        bridge::{FadenoModule, FadenoRuntime},
        compiler::{compile_file, find_binary},
        types::*,
    },
    types::*,
    utils::{
        state_graph::StateGraphOut,
        text::{AnchorPos, TextUpd, ROOT_ANCHOR},
    },
    wire::WireEventBody,
};

mod common;
use common::WikiTestCluster;

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

fn extract_doc_text(
    output: &LocValue,
    tags: &TagRegistry,
    branch_did: LocDataId,
) -> Option<String> {
    let anchor_agg = match tags
        .record_get(output, b"anchors")
        .expect("missing .anchors")
    {
        LocValue::KolAnchorAgg(a) => a,
        other => panic!("expected KolAnchorAgg, got {other:?}"),
    };
    let sg = extract_text_sg(output, tags);
    let branch_key = LocValue::KolDataId(branch_did);
    let timeline = sg.iter().find(|(k, _)| **k == branch_key);
    let (_, val) = match timeline.and_then(|(_, tl)| tl.last()) {
        Some(v) => v,
        None => return None,
    };
    let text_agg = match val {
        LocValue::KolTextAgg(ta) => ta,
        other => panic!("expected KolTextAgg, got {other:?}"),
    };
    Some(text_agg.get_text(&anchor_agg))
}

fn count_branches(sg: &StateGraphOut<LocValue, LocValue>) -> usize {
    sg.iter().count()
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

#[test]
fn retroactive_invite_cross_core_e2e() {
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
        id: 4,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let eve_uid = UserId {
        id: 5,
        identity_server_pk: IdentityServerPk([0; 32]),
    };

    let alice = tc.add_user(SenderPk([1u8; 32]), alice_uid);
    let bob = tc.add_user(SenderPk([2u8; 32]), bob_uid);
    let carol = tc.add_user(SenderPk([3u8; 32]), carol_uid);
    let dave = tc.add_user(SenderPk([4u8; 32]), dave_uid);
    let eve = tc.add_user(SenderPk([5u8; 32]), eve_uid);

    let alice_loc_uid = tc.mk_loc_user(alice_uid);
    let bob_loc_uid = tc.mk_loc_user(bob_uid);
    let carol_loc_uid = tc.mk_loc_user(carol_uid);
    let dave_loc_uid = tc.mk_loc_user(dave_uid);

    let b0 = tc.add_seed_branch(invite_mt, alice_loc_uid);

    let invited_gear = tc.build_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"invited")
            .expect("missing invited")
            .clone(),
        vec![LocValue::KolDataId(b0)],
    );
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = FadenoRuntime::route_group(invited_gear_wire.group(), &invited_wire_ctx)
        .unwrap()
        .route(2);

    let doc_id = tc.find_cross_core_doc_id(invited_core, 2);
    eprintln!("retroactive_invite: doc_id={doc_id}, invited_core={invited_core}");

    tc.post_events(
        vec![make_invite_event(alice, 1, invite_mt, b0, bob_loc_uid)],
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
        vec![make_invite_event(alice, 4, invite_mt, b0, carol_loc_uid)],
        5,
    );

    tc.post_events(
        vec![make_invite_event(bob, 5, invite_mt, b0, dave_loc_uid)],
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

    let output1 = tc.run_gear_on(0, doc_gear.clone());
    let text1 = extract_doc_text(&output1, &tags, b0);
    assert_eq!(
        text1, None,
        "run 1: no text expected (placeholder invited, cross-core deps not yet resolved)"
    );

    let output2 = tc.run_gear_on(0, doc_gear.clone());
    let sg2 = extract_text_sg(&output2, &tags);
    let text2 = extract_doc_text(&output2, &tags, b0);
    assert_eq!(
        text2,
        Some("Dave says hiHello from Bob".to_string()),
        "run 2: invited users' edits appear (RGA: higher tx_id first)"
    );

    let output3 = tc.run_gear_on(0, doc_gear);
    let text3 = extract_doc_text(&output3, &tags, b0);
    assert_eq!(
        text3, text2,
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

    let mut tc = WikiTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let attach_mt = tc.msg_type(b"Attach");
    let tags = tc.tags().clone();

    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let carol_uid = UserId {
        id: 2,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let eve_uid = UserId {
        id: 3,
        identity_server_pk: IdentityServerPk([0; 32]),
    };

    let alice = tc.add_user(SenderPk([1u8; 32]), alice_uid);
    let carol = tc.add_user(SenderPk([2u8; 32]), carol_uid);
    let eve = tc.add_user(SenderPk([3u8; 32]), eve_uid);

    let alice_loc_uid = tc.mk_loc_user(alice_uid);
    let carol_loc_uid = tc.mk_loc_user(carol_uid);

    let b0 = tc.add_seed_branch(invite_mt, alice_loc_uid);
    let b1 = tc.add_seed_branch(invite_mt, carol_loc_uid);

    let invited_gear = tc.build_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"invited")
            .expect("missing invited")
            .clone(),
        vec![LocValue::KolDataId(b0)],
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
            alice, 4, attach_mt, doc_id, b0, b1, &tags,
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

    let output1 = tc.run_gear_on(0, doc_gear.clone());
    let sg1 = extract_text_sg(&output1, &tags);
    let text1_b0 = extract_doc_text(&output1, &tags, b0);
    let text1_b1 = extract_doc_text(&output1, &tags, b1);
    assert_eq!(
        text1_b0,
        Some("BBBAAA".to_string()),
        "run 1 B0: Alice (creator) edit present, Carol's BBB merged via fork"
    );
    assert_eq!(
        text1_b1,
        Some("BBB".to_string()),
        "run 1 B1: placeholder invited, but Carol (creator) edit visible"
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

    let mut tc = WikiTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let attach_mt = tc.msg_type(b"Attach");
    let tags = tc.tags().clone();

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
        id: 4,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let eve_uid = UserId {
        id: 5,
        identity_server_pk: IdentityServerPk([0; 32]),
    };

    let alice = tc.add_user(SenderPk([1u8; 32]), alice_uid);
    let bob = tc.add_user(SenderPk([2u8; 32]), bob_uid);
    let carol = tc.add_user(SenderPk([3u8; 32]), carol_uid);
    let dave = tc.add_user(SenderPk([4u8; 32]), dave_uid);
    let eve = tc.add_user(SenderPk([5u8; 32]), eve_uid);

    let alice_loc_uid = tc.mk_loc_user(alice_uid);
    let bob_loc_uid = tc.mk_loc_user(bob_uid);
    let carol_loc_uid = tc.mk_loc_user(carol_uid);
    let dave_loc_uid = tc.mk_loc_user(dave_uid);

    let b0 = tc.add_seed_branch(invite_mt, alice_loc_uid);

    let invited_gear = tc.build_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"invited")
            .expect("missing invited")
            .clone(),
        vec![LocValue::KolDataId(b0)],
    );
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = FadenoRuntime::route_group(invited_gear_wire.group(), &invited_wire_ctx)
        .unwrap()
        .route(2);

    let doc_id = tc.find_cross_core_doc_id(invited_core, 2);
    eprintln!("multi_user_doc_assembly: doc_id={doc_id}, invited_core={invited_core}");

    tc.post_events(
        vec![
            make_invite_event(alice, 1, invite_mt, b0, bob_loc_uid), // Alice→Bob
            make_invite_event(alice, 2, invite_mt, b0, carol_loc_uid), // Alice→Carol
            make_invite_event(alice, 3, invite_mt, b0, dave_loc_uid), // Alice→Dave
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

    let output1 = tc.run_gear_on(0, doc_gear.clone());
    let text1 = extract_doc_text(&output1, &tags, b0);
    assert_eq!(
        text1,
        Some("Hello".to_string()),
        "run 1: Alice (creator) edit present with placeholder invited"
    );

    let output2 = tc.run_gear_on(0, doc_gear.clone());
    let sg2 = extract_text_sg(&output2, &tags);
    assert_eq!(
        count_branches(&sg2),
        1,
        "run 2: exactly one branch should have entries"
    );

    let text2 = extract_doc_text(&output2, &tags, b0);
    assert_eq!(
        text2,
        Some(" [Dave]!WorldHello".to_string()),
        "run 2: all invited users' edits (RGA: higher tx_id first), Eve excluded"
    );

    let output3 = tc.run_gear_on(0, doc_gear);
    let text3 = extract_doc_text(&output3, &tags, b0);
    assert_eq!(text3, text2, "run 3: output should be stable");
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

    let mut tc = WikiTestCluster::start(&[2, 3, 4], module);
    let invite_mt = tc.msg_type(b"Invite");
    let attach_mt = tc.msg_type(b"Attach");

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
    let bob_loc_uid = tc.mk_loc_user(bob_uid);

    let b0 = tc.add_seed_branch(invite_mt, alice_loc_uid);

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
        vec![make_invite_event(alice, 2, invite_mt, b0, bob_loc_uid)],
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
        vec![LocValue::KolDataId(b0)],
    );

    let output = tc.build_and_run_gear(
        tc.tags()
            .record_get(tc.module().exports(), b"doc_content")
            .expect("missing doc_content")
            .clone(),
        vec![LocValue::Num(doc_id as i64)],
    );
    let text = extract_doc_text(&output, &tags, b0);
    assert_eq!(
        text,
        Some("Bob after invite".to_string()),
        "only Bob's post-invite edit should appear; pre-invite edit excluded"
    );
}
