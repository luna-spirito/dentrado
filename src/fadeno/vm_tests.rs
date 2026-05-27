use crate::{
    core::{
        core_ctx::Core,
        gear::{EmptyRuntime, Runtime},
        loc_ctx::{EventContext, StoredEvent},
    },
    fadeno::{
        compiler::{compile_file, find_binary, CompileOutput},
        types::*,
        vm::{self, VmContext},
    },
    types::{IdentityServerPk, LocMsgTypeId, SenderPk, UserId},
    wire::WireLocCtx,
};
use std::sync::Arc;

fn compile_test(name: &str) -> Option<CompileOutput> {
    let binary = find_binary()?;
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fadeno-lang/fad/test")
        .join(name);
    Some(
        compile_file(&binary, &path)
            .ignore_type_error()
            .expect("compilation failed"),
    )
}

fn make_tags() -> TagRegistry {
    TagRegistry::new(Vec::new(), Vec::new())
}

fn make_common(tags: &mut TagRegistry) -> vm::CommonTags {
    vm::CommonTags::ensure(tags)
}

#[test]
fn vm_simple_value() {
    let mut cr = Compiled {
        tags: make_tags(),
        constants: vec![LocValue::Num(42)],
        pool: vec![Instr::PushConst(0)],
        module_ranges: vec![InstrRange { start: 0, len: 1 }],
    };
    let common = make_common(&mut cr.tags);
    let exports = vm::init(&cr, &common).unwrap();
    assert_eq!(exports[0], LocValue::Num(42));
}

#[test]
fn vm_let_binding() {
    let mut cr = Compiled {
        tags: make_tags(),
        constants: vec![LocValue::Num(5)],
        pool: vec![
            Instr::PushConst(0),
            Instr::PushVar,
            Instr::Copy(0),
            Instr::PopVar,
        ],
        module_ranges: vec![InstrRange { start: 0, len: 4 }],
    };
    let common = make_common(&mut cr.tags);
    let exports = vm::init(&cr, &common).unwrap();
    assert_eq!(exports[0], LocValue::Num(5));
}

#[test]
fn vm_closure() {
    let mut cr = Compiled {
        tags: make_tags(),
        constants: vec![LocValue::Num(10)],
        pool: vec![
            Instr::Copy(0),
            Instr::PushConst(0),
            Instr::Closure {
                captures: 0,
                args: 1,
                body: InstrRange { start: 0, len: 1 },
            },
            Instr::App(1),
        ],
        module_ranges: vec![InstrRange { start: 1, len: 3 }],
    };
    let common = make_common(&mut cr.tags);
    let exports = vm::init(&cr, &common).unwrap();
    assert_eq!(exports[0], LocValue::Num(10));
}

#[test]
fn vm_if_else() {
    let mut cr = Compiled {
        tags: make_tags(),
        constants: vec![LocValue::Num(1), LocValue::Num(0), LocValue::Bool(true)],
        pool: vec![
            Instr::PushConst(0),
            Instr::PushConst(1),
            Instr::PushConst(2),
            Instr::IfElse {
                then_: InstrRange { start: 0, len: 1 },
                else_: InstrRange { start: 1, len: 1 },
            },
        ],
        module_ranges: vec![InstrRange { start: 2, len: 2 }],
    };
    let common = make_common(&mut cr.tags);
    let exports = vm::init(&cr, &common).unwrap();
    assert_eq!(exports[0], LocValue::Num(1));
}

#[test]
fn vm_list() {
    let mut cr = Compiled {
        tags: make_tags(),
        constants: vec![LocValue::Num(3), LocValue::Num(4), LocValue::Num(5)],
        pool: vec![
            Instr::PushConst(0),
            Instr::PushConst(1),
            Instr::PushConst(2),
            Instr::MkList(3),
        ],
        module_ranges: vec![InstrRange { start: 0, len: 4 }],
    };
    let common = make_common(&mut cr.tags);
    let exports = vm::init(&cr, &common).unwrap();
    match &exports[0] {
        LocValue::List(vs) => {
            assert_eq!(vs.len(), 3);
            assert_eq!(vs[0], LocValue::Num(3));
            assert_eq!(vs[1], LocValue::Num(4));
            assert_eq!(vs[2], LocValue::Num(5));
        }
        other => panic!("expected List, got: {other}"),
    }
}

#[test]
fn compile_id() {
    let _ = compile_test("id");
}

#[test]
fn loop_fac_6_is_720() {
    let child = std::thread::Builder::new()
        .stack_size(4 * 1024 * 1024)
        .spawn(|| {
            let output = compile_test("loop").expect("loop.fad compilation failed");
            let mut bytecode = output.bytecode;
            let common = vm::CommonTags::ensure(&mut bytecode.tags);
            let exports = vm::init(&bytecode, &common).expect("VM execution failed");
            let result = exports.last().expect("no exports");
            assert_eq!(*result, LocValue::Num(720), "fac 6 should be 720");
        })
        .expect("thread spawn failed");
    child.join().expect("test thread panicked");
}

#[test]
fn sg_apply_preserves_stack() {
    let mut tags = TagRegistry::new(Vec::new(), Vec::new());
    let _sender_tag = tags.ensure_tag_id(b"sender");
    let _body_tag = tags.ensure_tag_id(b"body");
    let _query_tag = tags.ensure_tag_id(b"query");
    let _delta_tag = tags.ensure_tag_id(b"delta");
    let _removed_tag = tags.ensure_tag_id(b"removed");
    let _added_tag = tags.ensure_tag_id(b"added");

    let common = vm::CommonTags::ensure(&mut tags);

    let mut core: Core<crate::fadeno::bridge::FadenoRuntime> = Core::new(
        1,
        0,
        crate::types::NodeId(0),
        Arc::new(crate::fadeno::bridge::FadenoModule::default()),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    let pk = SenderPk([1u8; 32]);
    let uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let msg_type = LocMsgTypeId(1);
    let group = LocValue::Tag(0);
    let group_id = EventContext::mk_loc_group(&mut core, msg_type, group.clone());
    let global_core_id = crate::fadeno::bridge::FadenoRuntime::route_group(
        &group,
        &WireLocCtx::<EmptyRuntime> {
            users: vec![],
            senders: vec![],
            data: vec![],
        },
    )
    .unwrap();
    let sender = EventContext::mk_loc_sender(&mut core, pk, Some(uid));
    let event_id = EventContext::store_event(
        &mut core,
        StoredEvent {
            group: group_id,
            sender,
            global_core_id,
            tx_id: 1,
            timestamp: 1,
            source_node: crate::types::NodeId(0),
            body: LocValue::Record {
                tag_set: Arc::new(vec![0]),
                fields: Arc::new(Vec::new()),
            },
        },
    )
    .unwrap()
    .new;

    let delta_record = LocValue::Record {
        tag_set: Arc::new(vec![common.delta_tag_set]),
        fields: Arc::new(vec![
            LocValue::List(Arc::new(Vec::new())), // .removed
            LocValue::List(Arc::new(vec![LocValue::KolEventId(event_id)])), // .added
        ]),
    };
    let constants = vec![
        LocValue::Num(42),
        LocValue::Builtin(BuiltinT::KolMkStateGraph),
        LocValue::Bool(false),
        LocValue::Record {
            tag_set: Arc::new(vec![0]),
            fields: Arc::new(vec![]),
        }, // unit
        delta_record,
        LocValue::Builtin(BuiltinT::KolStateGraphApply),
    ];

    let handler_offset = 30u32;
    let dep_resolver_offset = 31u32;

    let mut pool = vec![
        Instr::PushConst(0), // 0: push Num(42)
        Instr::PushConst(1), // 1: mk_stategraph
        Instr::Closure {
            captures: 0,
            args: 1,
            body: InstrRange {
                start: handler_offset,
                len: 1,
            },
        }, // 2: handler
        Instr::Closure {
            captures: 0,
            args: 1,
            body: InstrRange {
                start: dep_resolver_offset,
                len: 1,
            },
        }, // 3: dep_resolver
        Instr::PushConst(4), // 4: delta
        Instr::PushConst(5), // 5: KolStateGraphApply
        Instr::App(4),       // 6: stategraph_apply(4 args)
        Instr::Copy(1),      // 7: should get Num(42)
    ];

    while pool.len() < handler_offset as usize {
        pool.push(Instr::PushConst(0)); // filler (never reached)
    }
    pool.push(Instr::PushConst(3)); // 30: handler body
    pool.push(Instr::Copy(0)); // 31: dep_resolver body

    let main_closure = LocValue::Closure(Closure {
        captures: Arc::new(vec![]),
        args: 2,
        body: InstrRange { start: 0, len: 8 },
    });

    let ctx = VmContext {
        pool: &pool,
        constants: &constants,
        tags: &tags,
        imports: &[],
        common: &common,
    };

    let result = vm::call_gear_step(
        &ctx,
        &core,
        main_closure,
        vec![
            LocValue::Record {
                tag_set: Arc::new(vec![0]),
                fields: Arc::new(vec![]),
            },
            LocValue::KolPrimary,
        ], // _cache = unit, primary = placeholder
        Some(group_id),
    );

    match result {
        Ok(v) => assert_eq!(v, LocValue::Num(42),
            "expected Num(42) from pre-apply stack slot, got {v:?} — stack corrupted by stategraph_apply"),
        Err(e) => panic!("step failed: {e:?}"),
    }
}
