use std::num::NonZero;

use dentrado::{
    core::{
        core_ctx::{Core, GearCtx},
        gear::{GearInput, GearMeta, IsRuntime},
        loc_ctx::{EventContext, EventStore},
    },
    types::*,
    utils::{
        state_graph::{DeltaList, HandlerCtx, SGBucketId, SGEventId, StateGraph, Timeline},
        text::{AnchorAgg, AnchorPos, ROOT_ANCHOR, TextAgg, TextUpd},
    },
    wire::WireEventBody,
};
use im::OrdMap;

mod common;
use common::TestCluster;

pub const MSG_INVITE: LocMsgTypeId = LocMsgTypeId(1);
pub const MSG_ATTACH: LocMsgTypeId = LocMsgTypeId(2);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Wiki2Gear {
    Invited { branch: LocDataId },
    DocContent { doc: Id },
}

impl Localizable for Wiki2Gear {
    fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        match self {
            Self::Invited { branch } => Ok(Self::Invited {
                branch: branch.localize(remapper)?,
            }),
            Self::DocContent { doc } => Ok(Self::DocContent { doc }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wiki2GearOut {
    Invited(Timeline<LocUserId, bool>),
    DocContent {
        anchors: AnchorAgg,
        text: Timeline<LocDataId, TextAgg>,
    },
}

impl Localizable for Wiki2GearOut {
    fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        match self {
            Self::Invited(timeline) => Ok(Self::Invited(timeline.localize(remapper)?)),
            Self::DocContent { anchors, text } => Ok(Self::DocContent {
                anchors: anchors.localize(remapper)?,
                text: text.localize(remapper)?,
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Wiki2Body {
    Invite(LocUserId),
    Attach(AttachBody),
}

#[derive(Clone, Debug)]
pub struct AttachBody {
    pub branch: LocDataId,
    pub payload: UpdatePayload,
}

#[derive(Clone, Debug)]
pub enum UpdatePayload {
    Edit { edit: TextUpd },
    Merge { from: LocDataId },
}

impl Localizable for Wiki2Body {
    fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        match self {
            Self::Invite(uid) => Ok(Self::Invite(uid.localize(remapper)?)),
            Self::Attach(body) => Ok(Self::Attach(body.localize(remapper)?)),
        }
    }
}

impl Localizable for AttachBody {
    fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(AttachBody {
            branch: self.branch.localize(remapper)?,
            payload: self.payload.localize(remapper)?,
        })
    }
}

impl Localizable for UpdatePayload {
    fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        match self {
            Self::Edit { edit } => Ok(Self::Edit {
                edit: edit.localize(remapper)?,
            }),
            Self::Merge { from } => Ok(Self::Merge {
                from: from.localize(remapper)?,
            }),
        }
    }
}

impl Wiki2Body {
    pub fn unwrap_invite(&self) -> LocUserId {
        match self {
            Self::Invite(uid) => *uid,
            _ => panic!("Expected Invite"),
        }
    }

    pub fn unwrap_attach(&self) -> &AttachBody {
        match self {
            Self::Attach(body) => body,
            _ => panic!("Expected Attach"),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct BranchData {
    pub creator: LocUserId,
    pub created_at: i64,
}

impl Localizable for BranchData {
    fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(BranchData {
            creator: self.creator.localize(remapper)?,
            created_at: self.created_at,
        })
    }
}

#[derive(Debug, Clone)]
pub struct InvitedCache {
    pub processed_added: usize,
    pub processed_removed: usize,
    pub sg: StateGraph<(), (), (), LocUserId, bool>,
}

#[derive(Debug, Clone)]
pub struct DocContentCache {
    pub processed_added: usize,
    pub processed_removed: usize,
    pub anchors: AnchorAgg,
    pub sg: StateGraph<LocDataId, LocUserId, bool, LocDataId, TextAgg>,
}

#[derive(Debug, Clone)]
pub enum Wiki2Cache {
    Invited(InvitedCache),
    DocContent(DocContentCache),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Wiki2Group {
    Branch(LocDataId),
    Doc(Id),
}

impl Localizable for Wiki2Group {
    fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        match self {
            Self::Branch(b) => Ok(Self::Branch(b.localize(remapper)?)),
            Self::Doc(d) => Ok(Self::Doc(d)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Wiki2Runtime;

impl IsRuntime for Wiki2Runtime {
    type GearId = Wiki2Gear;
    type GearOut = Wiki2GearOut;
    type Module = ();
    type Group = Wiki2Group;
    type Body = Wiki2Body;
    type Data = BranchData;
    type GearCache = Wiki2Cache;

    fn hash_data(
        data: &Self::Data,
        resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError> {
        let resolved_creator = resolver.resolve_user(data.creator)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&resolved_creator.id.to_le_bytes());
        hasher.update(&resolved_creator.identity_server_pk.0);
        hasher.update(&data.created_at.to_le_bytes());
        Ok(*hasher.finalize().as_bytes())
    }

    fn route_group(
        group: &Self::Group,
        resolver: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, GroupRouteError> {
        let mut hasher = blake3::Hasher::new();
        match group {
            Wiki2Group::Branch(did) => {
                let resolved = resolver.resolve_data(*did)?;
                hasher.update(&resolved.timestamp.to_le_bytes());
                hasher.update(&resolved.hash);
            }
            Wiki2Group::Doc(doc_id) => {
                hasher.update(&doc_id.0.to_le_bytes());
            }
        }
        Ok(GlobalCoreId(u32::from_le_bytes(
            hasher.finalize().as_bytes()[..4].try_into().unwrap(),
        )))
    }

    fn meta(gear: &Self::GearId) -> GearMeta<Self> {
        match gear {
            Wiki2Gear::Invited { branch } => GearMeta::Event {
                msg_type: MSG_INVITE,
                group: Wiki2Group::Branch(*branch),
            },
            Wiki2Gear::DocContent { doc } => GearMeta::Event {
                msg_type: MSG_ATTACH,
                group: Wiki2Group::Doc(*doc),
            },
        }
    }

    fn make_cache(gear: &Self::GearId) -> Self::GearCache {
        match gear {
            Wiki2Gear::Invited { .. } => Wiki2Cache::Invited(InvitedCache {
                processed_added: 0,
                processed_removed: 0,
                sg: StateGraph::new(),
            }),
            Wiki2Gear::DocContent { .. } => Wiki2Cache::DocContent(DocContentCache {
                processed_added: 0,
                processed_removed: 0,
                anchors: AnchorAgg::new(),
                sg: StateGraph::new(),
            }),
        }
    }

    async fn run_step(
        ctx: &mut GearCtx<Self>,
        input: GearInput,
        cache: &mut Self::GearCache,
    ) -> Self::GearOut {
        let GearInput::Events(group) = input else {
            return match cache {
                Wiki2Cache::Invited(c) => Wiki2GearOut::Invited(c.sg.as_writes()),
                Wiki2Cache::DocContent(c) => Wiki2GearOut::DocContent {
                    anchors: c.anchors.clone(),
                    text: c.sg.as_writes(),
                },
            };
        };
        match (ctx.gear(), cache) {
            (Wiki2Gear::Invited { branch }, Wiki2Cache::Invited(c)) => {
                let Some((added_ids, removed_ids)) =
                    ctx.query_events(group, (c.processed_added, c.processed_removed), |a, r| {
                        (a.to_vec(), r.to_vec())
                    })
                else {
                    return Wiki2GearOut::Invited(c.sg.as_writes());
                };

                let core = ctx.core();
                let store = core.group_store(group);
                let (_, branch_data) = store.data(*branch).expect("Branch data not found");
                let creator = branch_data.creator;

                let mut handler = async move |invitee: &LocUserId,
                                              hctx: &mut HandlerCtx<
                    (),
                    (),
                    (),
                    Self,
                    LocUserId,
                    bool,
                    _,
                >| {
                    let event_id = hctx.event_id;
                    let stored = hctx
                        .store()
                        .stored_event(event_id.local_id())
                        .expect("stored event not found");
                    let sender_sid = stored.sender;
                    let sender_uid = hctx
                        .store()
                        .sender_user(sender_sid)
                        .expect("sender user not found");

                    let sender_invited = if sender_uid == creator {
                        true
                    } else {
                        hctx.query(&sender_uid).unwrap_or(false)
                    };

                    if sender_invited {
                        hctx.update(*invitee, true);
                    }
                };

                let event_resolver = |local_id: GroupEventId| {
                    let stored = store
                        .stored_event(local_id)
                        .expect("stored event not found");
                    let sg_id = SGEventId::new(
                        SGBucketId {
                            timestamp: stored.timestamp,
                        },
                        local_id,
                    );
                    (sg_id, stored.body.unwrap_invite())
                };

                let mut dep_resolver = async |_: &()| -> Timeline<(), ()> { Timeline::new() };

                let added_len = added_ids.len();
                let removed_len = removed_ids.len();

                c.sg.apply(
                    &mut handler,
                    &event_resolver,
                    &mut dep_resolver,
                    &store,
                    &DeltaList {
                        removed: removed_ids,
                        added: added_ids,
                    },
                )
                .await;

                c.processed_added += added_len;
                c.processed_removed += removed_len;

                Wiki2GearOut::Invited(c.sg.as_writes())
            }
            (Wiki2Gear::DocContent { .. }, Wiki2Cache::DocContent(c)) => {
                let Some((added_ids, removed_ids)) =
                    ctx.query_events(group, (c.processed_added, c.processed_removed), |a, r| {
                        (a.to_vec(), r.to_vec())
                    })
                else {
                    return Wiki2GearOut::DocContent {
                        anchors: c.anchors.clone(),
                        text: c.sg.as_writes(),
                    };
                };

                let core = ctx.core();
                let store = core.group_store(group);

                // Update anchors
                for &eid in &added_ids {
                    let stored = store.stored_event(eid).expect("event not found");
                    let attach_body = stored.body.unwrap_attach();
                    match &attach_body.payload {
                        UpdatePayload::Edit { edit } => {
                            let sender_event_id = LocSenderEventId(stored.sender, stored.tx_id);
                            c.anchors = c.anchors.clone().apply(sender_event_id, edit, &store);
                        }
                        UpdatePayload::Merge { .. } => {}
                    }
                }

                // `dep_resolver` is now async; pull the Invited timeline via
                // `secondary_get` (awaits the dep if cold).
                let mut dep_resolver = async |branch: &LocDataId| -> Timeline<LocUserId, bool> {
                    match ctx
                        .secondary_get(Wiki2Gear::Invited { branch: *branch })
                        .await
                    {
                        Wiki2GearOut::Invited(timeline) => timeline,
                        _ => panic!("Expected Invited output"),
                    }
                };

                let mut handler = async |event_body: &AttachBody,
                                         hctx: &mut HandlerCtx<
                    LocDataId,
                    LocUserId,
                    bool,
                    Self,
                    LocDataId,
                    TextAgg,
                    _,
                >| {
                    let event_id = hctx.event_id;
                    let stored = hctx
                        .store()
                        .stored_event(event_id.local_id())
                        .expect("stored event not found");
                    let sender_sid = stored.sender;
                    let sender_uid = hctx
                        .store()
                        .sender_user(sender_sid)
                        .expect("sender user not found");

                    let branch = event_body.branch;
                    let (_, branch_data) =
                        hctx.store().data(branch).expect("Branch data not found");
                    let creator = branch_data.creator;

                    let is_invited = if sender_uid == creator {
                        true
                    } else {
                        hctx.dep_query(&branch, &sender_uid).await.unwrap_or(false)
                    };

                    if is_invited {
                        let curr_text_agg = hctx.query(&branch).unwrap_or_default();
                        let next_text_agg = match &event_body.payload {
                            UpdatePayload::Merge { from } => {
                                let from_text_agg = hctx.query(from).unwrap_or_default();
                                curr_text_agg.merge(&from_text_agg)
                            }
                            UpdatePayload::Edit { edit } => {
                                let sender_event_id = LocSenderEventId(stored.sender, stored.tx_id);
                                curr_text_agg.apply(sender_event_id, edit)
                            }
                        };
                        hctx.update(branch, next_text_agg);
                    }
                };

                let event_resolver = |local_id: GroupEventId| {
                    let stored = store
                        .stored_event(local_id)
                        .expect("stored event not found");
                    let sg_id = SGEventId::new(
                        SGBucketId {
                            timestamp: stored.timestamp,
                        },
                        local_id,
                    );
                    (sg_id, stored.body.unwrap_attach().clone())
                };

                let added_len = added_ids.len();
                let removed_len = removed_ids.len();

                c.sg.apply(
                    &mut handler,
                    &event_resolver,
                    &mut dep_resolver,
                    &store,
                    &DeltaList {
                        removed: removed_ids,
                        added: added_ids,
                    },
                )
                .await;

                c.processed_added += added_len;
                c.processed_removed += removed_len;

                Wiki2GearOut::DocContent {
                    anchors: c.anchors.clone(),
                    text: c.sg.as_writes(),
                }
            }
            _ => panic!("Mismatched gear and cache"),
        }
    }
}

fn add_seed_branch(tc: &mut TestCluster<Wiki2Runtime>, creator_uid: LocUserId) -> LocDataId {
    let b0 = tc.add_data(BranchData {
        creator: creator_uid,
        created_at: 1,
    });
    tc.loc_ctx.mk_loc_group(MSG_INVITE, Wiki2Group::Branch(b0));
    b0
}

fn make_invite_event(
    sender: LocSenderId,
    tx_id: u32,
    branch_did: LocDataId,
    invitee_uid: LocUserId,
) -> WireEventBody<Wiki2Group, Wiki2Body> {
    WireEventBody {
        sender,
        tx_id,
        msg_type: MSG_INVITE,
        group: Wiki2Group::Branch(branch_did),
        body: Wiki2Body::Invite(invitee_uid),
    }
}

fn make_attach_edit_event(
    sender: LocSenderId,
    tx_id: u32,
    doc_id: u64,
    branch_did: LocDataId,
    text_upd: TextUpd,
) -> WireEventBody<Wiki2Group, Wiki2Body> {
    WireEventBody {
        sender,
        tx_id,
        msg_type: MSG_ATTACH,
        group: Wiki2Group::Doc(Id(doc_id)),
        body: Wiki2Body::Attach(AttachBody {
            branch: branch_did,
            payload: UpdatePayload::Edit { edit: text_upd },
        }),
    }
}

fn make_attach_fork_event(
    sender: LocSenderId,
    tx_id: u32,
    doc_id: u64,
    child_branch_did: LocDataId,
    parent_branch_did: LocDataId,
) -> WireEventBody<Wiki2Group, Wiki2Body> {
    WireEventBody {
        sender,
        tx_id,
        msg_type: MSG_ATTACH,
        group: Wiki2Group::Doc(Id(doc_id)),
        body: Wiki2Body::Attach(AttachBody {
            branch: child_branch_did,
            payload: UpdatePayload::Merge {
                from: parent_branch_did,
            },
        }),
    }
}

fn extract_invited_pairs(output: Wiki2GearOut) -> Vec<(LocUserId, bool)> {
    let timeline = match output {
        Wiki2GearOut::Invited(tl) => tl,
        other => panic!("expected Invited timeline, got {other:?}"),
    };
    let mut result = Vec::new();
    for (key, sg_map) in timeline.iter() {
        if let Some((_, b)) = sg_map.last() {
            result.push((*key, *b));
        }
    }
    result
}

fn extract_text_sg(output: &Wiki2GearOut) -> Box<Timeline<LocDataId, TextAgg>> {
    match output {
        Wiki2GearOut::DocContent { text, .. } => Box::new(text.clone()),
        other => panic!("expected DocContent, got {other:?}"),
    }
}

fn extract_doc_text(output: &Wiki2GearOut, branch_did: LocDataId) -> Option<String> {
    let (anchors, text) = match output {
        Wiki2GearOut::DocContent { anchors, text } => (anchors, text),
        other => panic!("expected DocContent, got {other:?}"),
    };
    let timeline = text.iter().find(|(k, _)| **k == branch_did);
    let (_, text_agg) = timeline.and_then(|(_, tl)| tl.last())?;
    Some(text_agg.get_text(anchors))
}

fn count_branches(sg: &Timeline<LocDataId, TextAgg>) -> usize {
    sg.iter().count()
}

fn find_cross_core_doc_id(
    tc: &TestCluster<Wiki2Runtime>,
    invited_core: u32,
    num_cores: u32,
) -> u64 {
    (1..10_000)
        .find(|&d| {
            let doc_gear = Wiki2Gear::DocContent { doc: Id(d) };
            let (doc_gear_wire, wc) = tc.remap_gear(doc_gear);

            let gear_core =
                Wiki2Runtime::route_group(Wiki2Runtime::meta(&doc_gear_wire).group(), &wc)
                    .unwrap()
                    .route(NonZero::new(num_cores).unwrap());
            if gear_core == invited_core {
                return false;
            }
            let event_core = Wiki2Runtime::route_group(&Wiki2Group::Doc(Id(d)), &wc)
                .unwrap()
                .route(NonZero::new(num_cores).unwrap());
            event_core == gear_core
        })
        .expect("should find a suitable doc_id for cross-core routing")
}

fn find_same_core_doc_id(tc: &TestCluster<Wiki2Runtime>, invited_core: u32, num_cores: u32) -> u64 {
    (1..10_000)
        .find(|&d| {
            let doc_gear = Wiki2Gear::DocContent { doc: Id(d) };
            let (doc_gear_wire, wc) = tc.remap_gear(doc_gear);

            let gear_core =
                Wiki2Runtime::route_group(Wiki2Runtime::meta(&doc_gear_wire).group(), &wc)
                    .unwrap()
                    .route(NonZero::new(num_cores).unwrap());
            if gear_core != invited_core {
                return false;
            }
            let event_core = Wiki2Runtime::route_group(&Wiki2Group::Doc(Id(d)), &wc)
                .unwrap()
                .route(NonZero::new(num_cores).unwrap());
            event_core == gear_core
        })
        .expect("should find a suitable doc_id for same-core routing")
}

#[test]
fn invited_simple_e2e() {
    let mut tc: TestCluster<Wiki2Runtime> = TestCluster::start(&[2, 3, 4], ());

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

    let alice_loc_uid = tc.loc_ctx.mk_loc_user(alice_uid);
    let bob_loc_uid = tc.loc_ctx.mk_loc_user(bob_uid);
    let carol_loc_uid = tc.loc_ctx.mk_loc_user(carol_uid);

    let b0 = add_seed_branch(&mut tc, alice_loc_uid);

    tc.post_events(
        vec![
            make_invite_event(alice, 0, b0, bob_loc_uid),
            make_invite_event(alice, 1, b0, carol_loc_uid),
            make_invite_event(bob, 2, b0, carol_loc_uid),
        ],
        1,
    );

    let gear = Wiki2Gear::Invited { branch: b0 };
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
    let mut tc: TestCluster<Wiki2Runtime> = TestCluster::start(&[2, 3, 4], ());

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

    let alice_loc_uid = tc.loc_ctx.mk_loc_user(alice_uid);
    let bob_loc_uid = tc.loc_ctx.mk_loc_user(bob_uid);

    let b0 = add_seed_branch(&mut tc, alice_loc_uid);

    tc.post_events(vec![make_invite_event(alice, 0, b0, bob_loc_uid)], 1);

    let invited_gear = Wiki2Gear::Invited { branch: b0 };
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = Wiki2Runtime::route_group(
        Wiki2Runtime::meta(&invited_gear_wire).group(),
        &invited_wire_ctx,
    )
    .unwrap()
    .route(NonZero::new(2).unwrap());

    let doc_id = find_same_core_doc_id(&tc, invited_core, 2);
    tc.loc_ctx
        .mk_loc_group(MSG_ATTACH, Wiki2Group::Doc(Id(doc_id)));

    let text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Hello from Bob".to_string()],
    );

    tc.post_events(
        vec![make_attach_edit_event(bob, 1, doc_id, b0, text_upd)],
        2,
    );

    let eve_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Eve was here".to_string()],
    );

    tc.post_events(
        vec![make_attach_edit_event(eve, 2, doc_id, b0, eve_text_upd)],
        3,
    );

    // Run invited first to make sure secondary dep is resolved when needed
    let invited_gear = Wiki2Gear::Invited { branch: b0 };
    let _invited_output = tc.run_gear_on(0, invited_gear);

    let doc_content_gear = Wiki2Gear::DocContent { doc: Id(doc_id) };
    let output = tc.run_gear_on(0, doc_content_gear);

    let text = extract_doc_text(&output, b0);
    assert_eq!(
        text,
        Some("Hello from Bob".to_string()),
        "document text mismatch"
    );
}

#[test]
fn doc_content_cross_core_e2e() {
    let mut tc: TestCluster<Wiki2Runtime> = TestCluster::start(&[2, 3, 4], ());

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

    let alice_loc_uid = tc.loc_ctx.mk_loc_user(alice_uid);
    let bob_loc_uid = tc.loc_ctx.mk_loc_user(bob_uid);

    let b0 = add_seed_branch(&mut tc, alice_loc_uid);

    let invited_gear = Wiki2Gear::Invited { branch: b0 };
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = Wiki2Runtime::route_group(
        Wiki2Runtime::meta(&invited_gear_wire).group(),
        &invited_wire_ctx,
    )
    .unwrap()
    .route(NonZero::new(2).unwrap());

    let doc_id = find_cross_core_doc_id(&tc, invited_core, 2);
    tc.loc_ctx
        .mk_loc_group(MSG_ATTACH, Wiki2Group::Doc(Id(doc_id)));
    eprintln!("found doc_id={doc_id} (invited → core {invited_core})");

    tc.post_events(vec![make_invite_event(alice, 0, b0, bob_loc_uid)], 1);

    let text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Hello from Bob".to_string()],
    );

    tc.post_events(
        vec![make_attach_edit_event(bob, 1, doc_id, b0, text_upd)],
        2,
    );

    let doc_gear = Wiki2Gear::DocContent { doc: Id(doc_id) };
    let (doc_gear_wire, doc_wire_ctx) = tc.remap_gear(doc_gear.clone());
    let doc_core =
        Wiki2Runtime::route_group(Wiki2Runtime::meta(&doc_gear_wire).group(), &doc_wire_ctx)
            .unwrap()
            .route(NonZero::new(2).unwrap());
    assert_ne!(
        invited_core, doc_core,
        "gears must be on different cores for cross-core test"
    );

    let output1 = tc.run_gear_on(0, doc_gear.clone());
    let text1 = extract_doc_text(&output1, b0);
    assert_eq!(
        text1,
        Some("Hello from Bob".to_string()),
        "first run: cross-core secondary_get awaits the invited dep, so Bob's text is visible"
    );

    let output2 = tc.run_gear_on(0, doc_gear);
    let text2 = extract_doc_text(&output2, b0);
    assert_eq!(
        text2,
        Some("Hello from Bob".to_string()),
        "second run: idempotent — invited dep still resolved"
    );
}

#[test]
fn retroactive_invite_cross_core_e2e() {
    let mut tc: TestCluster<Wiki2Runtime> = TestCluster::start(&[2, 3, 4], ());

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

    let alice_loc_uid = tc.loc_ctx.mk_loc_user(alice_uid);
    let bob_loc_uid = tc.loc_ctx.mk_loc_user(bob_uid);
    let carol_loc_uid = tc.loc_ctx.mk_loc_user(carol_uid);
    let dave_loc_uid = tc.loc_ctx.mk_loc_user(dave_uid);

    let b0 = add_seed_branch(&mut tc, alice_loc_uid);

    let invited_gear = Wiki2Gear::Invited { branch: b0 };
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = Wiki2Runtime::route_group(
        Wiki2Runtime::meta(&invited_gear_wire).group(),
        &invited_wire_ctx,
    )
    .unwrap()
    .route(NonZero::new(2).unwrap());

    let doc_id = find_cross_core_doc_id(&tc, invited_core, 2);
    tc.loc_ctx
        .mk_loc_group(MSG_ATTACH, Wiki2Group::Doc(Id(doc_id)));

    tc.post_events(vec![make_invite_event(alice, 1, b0, bob_loc_uid)], 2);

    let bob_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Hello from Bob".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(bob, 2, doc_id, b0, bob_text_upd)],
        3,
    );

    let carol_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Carol was here".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(carol, 3, doc_id, b0, carol_text_upd)],
        4,
    );

    tc.post_events(vec![make_invite_event(alice, 4, b0, carol_loc_uid)], 5);

    tc.post_events(vec![make_invite_event(bob, 5, b0, dave_loc_uid)], 6);

    let dave_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Dave says hi".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(dave, 6, doc_id, b0, dave_text_upd)],
        7,
    );

    let eve_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Eve snoops".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(eve, 7, doc_id, b0, eve_text_upd)],
        8,
    );

    let doc_gear = Wiki2Gear::DocContent { doc: Id(doc_id) };
    let (doc_gear_wire, doc_wire_ctx) = tc.remap_gear(doc_gear.clone());
    let doc_core =
        Wiki2Runtime::route_group(Wiki2Runtime::meta(&doc_gear_wire).group(), &doc_wire_ctx)
            .unwrap()
            .route(NonZero::new(2).unwrap());
    assert_ne!(invited_core, doc_core, "gears must be on different cores");

    let output1 = tc.run_gear_on(0, doc_gear.clone());
    let text1 = extract_doc_text(&output1, b0);
    assert_eq!(
        text1,
        Some("Dave says hiHello from Bob".to_string()),
        "run 1: cross-core secondary_get awaits invited dep; invited users' edits appear (RGA: higher tx_id first)"
    );

    let output2 = tc.run_gear_on(0, doc_gear.clone());
    let text2 = extract_doc_text(&output2, b0);
    assert_eq!(text2, text1, "run 2: output should be stable");

    let output3 = tc.run_gear_on(0, doc_gear);
    let text3 = extract_doc_text(&output3, b0);
    assert_eq!(text3, text2, "run 3: output should be stable");
}

#[test]
fn text_agg_merge_cross_core_e2e() {
    let mut tc: TestCluster<Wiki2Runtime> = TestCluster::start(&[2, 3, 4], ());

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

    let alice_loc_uid = tc.loc_ctx.mk_loc_user(alice_uid);
    let carol_loc_uid = tc.loc_ctx.mk_loc_user(carol_uid);

    let b0 = add_seed_branch(&mut tc, alice_loc_uid);
    let b1 = add_seed_branch(&mut tc, carol_loc_uid);

    let invited_gear = Wiki2Gear::Invited { branch: b0 };
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = Wiki2Runtime::route_group(
        Wiki2Runtime::meta(&invited_gear_wire).group(),
        &invited_wire_ctx,
    )
    .unwrap()
    .route(NonZero::new(2).unwrap());

    let doc_id = find_cross_core_doc_id(&tc, invited_core, 2);
    tc.loc_ctx
        .mk_loc_group(MSG_ATTACH, Wiki2Group::Doc(Id(doc_id)));

    let alice_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["AAA".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(alice, 2, doc_id, b0, alice_text_upd)],
        11,
    );

    let carol_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["BBB".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(carol, 3, doc_id, b1, carol_text_upd)],
        12,
    );

    tc.post_events(vec![make_attach_fork_event(alice, 4, doc_id, b0, b1)], 13);

    let eve_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Eve ignored".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(eve, 5, doc_id, b1, eve_text_upd)],
        14,
    );

    // Warm up the dependency cache by running invited for both branches
    let invited_gear_b0 = Wiki2Gear::Invited { branch: b0 };
    let _invited_output_b0 = tc.run_gear(invited_gear_b0);
    let invited_gear_b1 = Wiki2Gear::Invited { branch: b1 };
    let _invited_output_b1 = tc.run_gear(invited_gear_b1);

    let doc_gear = Wiki2Gear::DocContent { doc: Id(doc_id) };
    let (doc_gear_wire, doc_wire_ctx) = tc.remap_gear(doc_gear.clone());
    let doc_core =
        Wiki2Runtime::route_group(Wiki2Runtime::meta(&doc_gear_wire).group(), &doc_wire_ctx)
            .unwrap()
            .route(NonZero::new(2).unwrap());
    assert_ne!(invited_core, doc_core, "gears must be on different cores");

    let output1 = tc.run_gear_on(0, doc_gear);
    let text1_b0 = extract_doc_text(&output1, b0);
    let text1_b1 = extract_doc_text(&output1, b1);
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
    let mut tc: TestCluster<Wiki2Runtime> = TestCluster::start(&[2, 3, 4], ());

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

    let alice_loc_uid = tc.loc_ctx.mk_loc_user(alice_uid);
    let bob_loc_uid = tc.loc_ctx.mk_loc_user(bob_uid);
    let carol_loc_uid = tc.loc_ctx.mk_loc_user(carol_uid);
    let dave_loc_uid = tc.loc_ctx.mk_loc_user(dave_uid);

    let b0 = add_seed_branch(&mut tc, alice_loc_uid);

    let invited_gear = Wiki2Gear::Invited { branch: b0 };
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = Wiki2Runtime::route_group(
        Wiki2Runtime::meta(&invited_gear_wire).group(),
        &invited_wire_ctx,
    )
    .unwrap()
    .route(NonZero::new(2).unwrap());

    let doc_id = find_cross_core_doc_id(&tc, invited_core, 2);
    tc.loc_ctx
        .mk_loc_group(MSG_ATTACH, Wiki2Group::Doc(Id(doc_id)));

    tc.post_events(
        vec![
            make_invite_event(alice, 1, b0, bob_loc_uid),
            make_invite_event(alice, 2, b0, carol_loc_uid),
            make_invite_event(alice, 3, b0, dave_loc_uid),
        ],
        16,
    );

    let alice_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Hello".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(alice, 3, doc_id, b0, alice_text_upd)],
        17,
    );

    let bob_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["World".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(bob, 4, doc_id, b0, bob_text_upd)],
        18,
    );

    let carol_text_upd = TextUpd::new(vec![AnchorPos::new(ROOT_ANCHOR, 0)], vec!["!".to_string()]);
    tc.post_events(
        vec![make_attach_edit_event(carol, 5, doc_id, b0, carol_text_upd)],
        19,
    );

    let dave_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec![" [Dave]".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(dave, 6, doc_id, b0, dave_text_upd)],
        20,
    );

    let eve_text_upd = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["[ignored]".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(eve, 7, doc_id, b0, eve_text_upd)],
        21,
    );

    let doc_gear = Wiki2Gear::DocContent { doc: Id(doc_id) };
    let (doc_gear_wire, doc_wire_ctx) = tc.remap_gear(doc_gear.clone());
    let doc_core =
        Wiki2Runtime::route_group(Wiki2Runtime::meta(&doc_gear_wire).group(), &doc_wire_ctx)
            .unwrap()
            .route(NonZero::new(2).unwrap());
    assert_ne!(invited_core, doc_core, "gears must be on different cores");

    let output1 = tc.run_gear_on(0, doc_gear.clone());
    let text1 = extract_doc_text(&output1, b0);
    assert_eq!(
        text1,
        Some(" [Dave]!WorldHello".to_string()),
        "run 1: cross-core secondary_get awaits invited dep; all invited users' edits (RGA: higher tx_id first), Eve excluded"
    );

    let output2 = tc.run_gear_on(0, doc_gear.clone());
    let sg2 = extract_text_sg(&output2);
    assert_eq!(
        count_branches(&sg2),
        1,
        "run 2: exactly one branch should have entries"
    );

    let text2 = extract_doc_text(&output2, b0);
    assert_eq!(text2, text1, "run 2: output should be stable");

    let output3 = tc.run_gear_on(0, doc_gear);
    let text3 = extract_doc_text(&output3, b0);
    assert_eq!(text3, text2, "run 3: output should be stable");
}

#[test]
fn retroactive_invite_point_in_time_same_core_e2e() {
    let mut tc: TestCluster<Wiki2Runtime> = TestCluster::start(&[2, 3, 4], ());

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

    let alice_loc_uid = tc.loc_ctx.mk_loc_user(alice_uid);
    let bob_loc_uid = tc.loc_ctx.mk_loc_user(bob_uid);

    let b0 = add_seed_branch(&mut tc, alice_loc_uid);

    let invited_gear = Wiki2Gear::Invited { branch: b0 };
    let (invited_gear_wire, invited_wire_ctx) = tc.remap_gear(invited_gear);
    let invited_core = Wiki2Runtime::route_group(
        Wiki2Runtime::meta(&invited_gear_wire).group(),
        &invited_wire_ctx,
    )
    .unwrap()
    .route(NonZero::new(2).unwrap());

    let doc_id = find_same_core_doc_id(&tc, invited_core, 2);
    tc.loc_ctx
        .mk_loc_group(MSG_ATTACH, Wiki2Group::Doc(Id(doc_id)));

    let bob_text_upd_1 = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Bob before invite".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(bob, 1, doc_id, b0, bob_text_upd_1)],
        23,
    );

    tc.post_events(vec![make_invite_event(alice, 2, b0, bob_loc_uid)], 24);

    let bob_text_upd_2 = TextUpd::new(
        vec![AnchorPos::new(ROOT_ANCHOR, 0)],
        vec!["Bob after invite".to_string()],
    );
    tc.post_events(
        vec![make_attach_edit_event(bob, 3, doc_id, b0, bob_text_upd_2)],
        25,
    );

    // Warm up the dependency cache
    let invited_gear = Wiki2Gear::Invited { branch: b0 };
    let _invited_output = tc.run_gear_on(0, invited_gear);

    let doc_content_gear = Wiki2Gear::DocContent { doc: Id(doc_id) };
    let output = tc.run_gear_on(0, doc_content_gear);

    let text = extract_doc_text(&output, b0);
    assert_eq!(
        text,
        Some("Bob after invite".to_string()),
        "only Bob's post-invite edit should appear; pre-invite edit excluded"
    );
}
