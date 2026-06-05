use std::{cell::Cell, sync::Arc};

use crate::{
    core::{
        core_ctx::Core,
        loc_ctx::{EventContext, LocCtx},
    },
    fadeno::{
        bridge::FadenoRuntime,
        types::{
            Bits, BuiltinT, Closure, Compiled, Instr, InstrRange, KolGear, LocValue, NumDesc,
            SgHandlerRef, TagRegistry,
        },
    },
    types::{AnyLocEventId, LocGroupId, LocMsgTypeId, LocSenderEventId, LocUserId},
    utils::state_graph::StateGraphOut,
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct CommonTags {
    pub(crate) sender: u64,
    pub(crate) body: u64,
    pub(crate) has: u64,
    pub(crate) edit: u64,
    pub(crate) cache: u64,
    pub(crate) out: u64,
    pub(crate) primary: u64,
    #[allow(dead_code)] // used by mk_gear destructuring
    pub(crate) r#type: u64,
    pub(crate) group: u64,
    pub(crate) initial_cache: u64,
    pub(crate) step: u64,

    pub(crate) viewl_tag_set: u64,
    pub(crate) query_result_tag_set: u64,
    pub(crate) delta_tag_set: u64,
    pub(crate) opt_none_tag_set: u64,
    pub(crate) opt_some_tag_set: u64,
    pub(crate) event_rec_tag_set: u64,
    pub(crate) sender_tag_set: u64,
}

impl CommonTags {
    pub(crate) fn ensure(tags: &mut TagRegistry) -> Self {
        let sender = tags.ensure_tag_id(b"sender");
        let body = tags.ensure_tag_id(b"body");
        let has = tags.ensure_tag_id(b"has");
        let edit = tags.ensure_tag_id(b"edit");
        let cache = tags.ensure_tag_id(b"cache");
        let out = tags.ensure_tag_id(b"out");
        let primary = tags.ensure_tag_id(b"primary");
        let r#type = tags.ensure_tag_id(b"type");
        let group = tags.ensure_tag_id(b"group");
        let initial_cache = tags.ensure_tag_id(b"initial_cache");
        let step = tags.ensure_tag_id(b"step");

        let left = tags.ensure_tag_id(b"left");
        let rest = tags.ensure_tag_id(b"rest");
        let query = tags.ensure_tag_id(b"query");
        let delta = tags.ensure_tag_id(b"delta");
        let removed = tags.ensure_tag_id(b"removed");
        let added = tags.ensure_tag_id(b"added");
        let branch = tags.ensure_tag_id(b"branch");
        let is_merge = tags.ensure_tag_id(b"is_merge");
        let from = tags.ensure_tag_id(b"from");
        let curr = tags.ensure_tag_id(b"curr");

        let viewl_tag_set = tags.ensure_tag_set(&[left, rest]);
        let query_result_tag_set = tags.ensure_tag_set(&[query, delta]);
        let delta_tag_set = tags.ensure_tag_set(&[removed, added]);
        let opt_none_tag_set = tags.ensure_tag_set(&[has]);
        let opt_some_tag_set = tags.ensure_tag_set(&[has, body]);
        let event_rec_tag_set = tags.ensure_tag_set(&[sender, body]);
        let sender_tag_set = tags.ensure_tag_set(&[sender]);

        let _ = tags.ensure_tag_set(&[branch, is_merge, edit]);
        let _ = tags.ensure_tag_set(&[branch, is_merge, from]);
        let _ = tags.ensure_tag_set(&[has, curr, rest]);

        Self {
            sender,
            body,
            has,
            edit,
            cache,
            out,
            primary,
            r#type,
            group,
            initial_cache,
            step,
            viewl_tag_set,
            query_result_tag_set,
            delta_tag_set,
            opt_none_tag_set,
            opt_some_tag_set,
            event_rec_tag_set,
            sender_tag_set,
        }
    }
}

#[derive(Debug)]
pub enum VmError {
    StackUnderflow {
        op: &'static str,
    },
    VarNotFound {
        depth: usize,
    },
    TypeError {
        op: &'static str,
        expected: &'static str,
        got: String,
    },
    OutsideGearStepContext {
        op: &'static str,
    },
    NumericOverflow,
    DivisionByZero,
    Panic(&'static str),
    RecordGetFailed {
        record: LocValue,
        tag: Option<String>,
    },
    MissingImport(i64),
    InvalidArgCount {
        expected: u8,
        got: usize,
    },
    WireError {
        op: &'static str,
        detail: String,
    },
}

impl std::fmt::Display for VmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VmError::StackUnderflow { op } => write!(f, "stack underflow in {op}"),
            VmError::VarNotFound { depth } => write!(f, "variable at depth {depth} not found"),
            VmError::TypeError { op, expected, got } => {
                write!(f, "type error in {op}: expected {expected}, got {got}")
            }
            VmError::OutsideGearStepContext { op } => {
                write!(f, "{op} called outside gear step context")
            }
            VmError::NumericOverflow => write!(f, "numeric overflow"),
            VmError::DivisionByZero => write!(f, "division by zero"),
            VmError::Panic(x) => write!(f, "runtime panic at: {x}"),
            VmError::MissingImport(n) => write!(f, "missing import #{n}"),
            VmError::InvalidArgCount { expected, got } => {
                write!(f, "invalid arg count: expected {expected}, got {got}")
            }
            VmError::WireError { op, detail } => {
                write!(f, "wire error in {op}: {detail}")
            }
            VmError::RecordGetFailed { record, tag } => {
                write!(f, "record get failed: {tag:?} in {record}")
            }
        }
    }
}

impl std::error::Error for VmError {}

pub(crate) fn init(cr: &Compiled, common: &CommonTags) -> Result<Vec<LocValue>, VmError> {
    let mut all_exports = Vec::with_capacity(cr.module_ranges.len());
    let mut imports: Vec<LocValue> = Vec::new();
    let kol_counter: Cell<u64> = Cell::new(0);

    for &range in &cr.module_ranges {
        let export = {
            let vm = Vm {
                pool: &cr.pool,
                constants: &cr.constants,
                tags: &cr.tags,
                imports: &imports,
                stage: VmStage::Init {
                    kol_id_counter_next: &kol_counter,
                },
                common: *common,
            };
            let mut init_stack = Vec::new();
            let result = vm.exec_range(&mut init_stack, range, None);
            result?
        };
        imports.push(export.clone());
        all_exports.push(export);
    }

    Ok(all_exports)
}

pub(crate) fn call_with_storage(
    pool: &[Instr],
    constants: &[LocValue],
    tags: &TagRegistry,
    imports: &[LocValue],
    common: &CommonTags,
    func: LocValue,
    args: Vec<LocValue>,
    storage: &LocCtx<FadenoRuntime>,
) -> Result<LocValue, VmError> {
    let vm = Vm {
        pool,
        constants,
        tags,
        imports,
        stage: VmStage::Run {
            ctx: storage,
            impure_core: None,
            impure_group: None,
        },
        common: *common,
    };
    vm.call(&mut Vec::new(), func, args, None)
}

pub(crate) struct VmContext<'a> {
    pub(crate) pool: &'a [Instr],
    pub(crate) constants: &'a [LocValue],
    pub(crate) tags: &'a TagRegistry,
    pub(crate) imports: &'a [LocValue],
    pub(crate) common: &'a CommonTags,
}

pub(crate) fn call_gear_step(
    ctx: &VmContext,
    core: &Core<FadenoRuntime>,
    func: LocValue,
    args: Vec<LocValue>,
    group: Option<crate::types::LocGroupId>,
) -> Result<LocValue, VmError> {
    let vm = Vm {
        pool: ctx.pool,
        constants: ctx.constants,
        tags: ctx.tags,
        imports: ctx.imports,
        stage: VmStage::Run {
            ctx: core.loc_ctx(),
            impure_core: Some(core),
            impure_group: group,
        },
        common: *ctx.common,
    };
    vm.call(&mut Vec::new(), func, args, None)
}

enum VmStage<'a> {
    Init {
        kol_id_counter_next: &'a Cell<u64>,
    },
    Run {
        ctx: &'a crate::core::loc_ctx::LocCtx<FadenoRuntime>,
        impure_core: Option<&'a Core<FadenoRuntime>>,
        impure_group: Option<LocGroupId>,
    },
}

struct Vm<'a> {
    pool: &'a [Instr],
    constants: &'a [LocValue],
    tags: &'a TagRegistry,
    imports: &'a [LocValue],
    stage: VmStage<'a>,
    common: CommonTags,
}

impl Vm<'_> {
    fn call<'h>(
        &self,
        stack: &mut Vec<LocValue>,
        func: LocValue,
        args: Vec<LocValue>,
        handler_ctx: Option<&'h SgHandlerRef<'h, '_>>,
    ) -> Result<LocValue, VmError> {
        let saved_len = stack.len();
        self.apply(stack, func, args, handler_ctx)?;
        let result = stack.pop().ok_or(VmError::StackUnderflow { op: "call" })?;
        stack.truncate(saved_len);
        Ok(result)
    }

    fn exec_range<'h>(
        &self,
        stack: &mut Vec<LocValue>,
        range: InstrRange,
        handler_ctx: Option<&'h SgHandlerRef<'h, '_>>,
    ) -> Result<LocValue, VmError> {
        let instrs = range.slice(self.pool);
        for instr in instrs {
            self.exec_one(stack, instr, handler_ctx)?;
        }
        stack
            .last()
            .cloned()
            .ok_or(VmError::StackUnderflow { op: "result" })
    }

    fn resolve_import(&self, v: &LocValue) -> LocValue {
        match v {
            LocValue::Import(n) => {
                #[allow(clippy::cast_possible_truncation)]
                let idx = *n as usize;
                if idx < self.imports.len() {
                    self.imports[idx].clone()
                } else {
                    LocValue::Import(*n)
                }
            }
            other => other.clone(),
        }
    }

    #[allow(
        clippy::too_many_lines,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap
    )]
    fn exec_one<'h>(
        &self,
        stack: &mut Vec<LocValue>,
        instr: &Instr,
        handler_ctx: Option<&'h SgHandlerRef<'h, '_>>,
    ) -> Result<(), VmError> {
        match instr {
            Instr::PushConst(idx) => {
                let v = self
                    .constants
                    .get(*idx as usize)
                    .cloned()
                    .ok_or(VmError::TypeError {
                        op: "PushConst",
                        expected: "valid constant index",
                        got: format!("index {idx} out of bounds (max {})", self.constants.len()),
                    })?;
                stack.push(self.resolve_import(&v));
                Ok(())
            }

            Instr::PushVar => Ok(()),

            Instr::PopVar => {
                if stack.len() < 2 {
                    return Err(VmError::StackUnderflow { op: "PopVar" });
                }
                let body_result = stack.pop().unwrap();
                let _binding = stack.pop().unwrap();
                stack.push(body_result);
                Ok(())
            }

            Instr::Copy(depth) => {
                let n = *depth as usize;
                if n >= stack.len() {
                    return Err(VmError::StackUnderflow { op: "Copy" });
                }
                let val = stack[stack.len() - 1 - n].clone();
                stack.push(val);
                Ok(())
            }

            Instr::App(n) => self.exec_app(stack, *n, handler_ctx),

            Instr::Closure {
                captures,
                args,
                body,
            } => {
                let n_captures = *captures as usize;
                if stack.len() < n_captures {
                    return Err(VmError::StackUnderflow { op: "Closure" });
                }
                let cap_vals: Vec<LocValue> = stack.drain(stack.len() - n_captures..).collect();
                stack.push(LocValue::Closure(Closure {
                    captures: Arc::new(cap_vals),
                    args: *args,
                    body: *body,
                }));
                Ok(())
            }

            Instr::IfElse { then_, else_ } => {
                let cond = stack
                    .pop()
                    .ok_or(VmError::StackUnderflow { op: "IfElse" })?;
                let LocValue::Bool(cond_bool) = cond else {
                    return Err(VmError::TypeError {
                        op: "IfElse",
                        expected: "Bool",
                        got: format!("{cond}"),
                    });
                };
                let branch = if cond_bool { then_ } else { else_ };
                self.exec_range(stack, *branch, handler_ctx)?;
                Ok(())
            }

            Instr::MkList(n) => {
                let n = *n as usize;
                if stack.len() < n {
                    return Err(VmError::StackUnderflow { op: "MkList" });
                }
                let items: Vec<LocValue> = stack.drain(stack.len() - n..).collect();
                stack.push(LocValue::List(Arc::new(items)));
                Ok(())
            }

            Instr::MkRecord(n) => {
                let n = *n as usize;
                if stack.len() < 2 * n {
                    return Err(VmError::StackUnderflow { op: "MkRecord" });
                }
                let kv: Vec<LocValue> = stack.drain(stack.len() - 2 * n..).collect();
                let fields: Vec<LocValue> = kv
                    .into_iter()
                    .enumerate()
                    .filter(|(i, _)| i % 2 == 1)
                    .map(|(_, v)| v)
                    .collect();
                stack.push(LocValue::Record {
                    tag_set: Arc::new(Vec::new()),
                    fields: Arc::new(fields),
                });
                Ok(())
            }

            Instr::MkQRecord { tag_set, n } => {
                let n = *n as usize;
                if stack.len() < n {
                    return Err(VmError::StackUnderflow { op: "MkQRecord" });
                }
                let fields: Vec<LocValue> = stack.drain(stack.len() - n..).collect();
                stack.push(LocValue::Record {
                    tag_set: Arc::new(vec![*tag_set]),
                    fields: Arc::new(fields),
                });
                Ok(())
            }
            Instr::RecordCat => {
                if stack.len() < 2 {
                    return Err(VmError::StackUnderflow { op: "RecordCat" });
                }
                let record_b = stack.pop().expect("RecordCat: b");
                let record_a = stack.pop().expect("RecordCat: a");
                let (ts_a, fields_a) = match &record_a {
                    LocValue::Record { tag_set, fields } => (tag_set.clone(), fields.clone()),
                    _ => {
                        return Err(VmError::TypeError {
                            op: "RecordCat",
                            expected: "Record, Record",
                            got: format!("{record_a}, {record_b}"),
                        });
                    }
                };
                let (ts_b, fields_b) = match &record_b {
                    LocValue::Record { tag_set, fields } => (tag_set.clone(), fields.clone()),
                    _ => {
                        return Err(VmError::TypeError {
                            op: "RecordCat",
                            expected: "Record, Record",
                            got: format!("{record_a}, {record_b}"),
                        });
                    }
                };

                let mut tag_set = Vec::with_capacity(ts_a.len() + ts_b.len());
                tag_set.extend_from_slice(&ts_a);
                tag_set.extend_from_slice(&ts_b);

                let mut fields = Vec::with_capacity(fields_a.len() + fields_b.len());
                fields.extend_from_slice(&fields_a);
                fields.extend_from_slice(&fields_b);

                stack.push(LocValue::Record {
                    tag_set: Arc::new(tag_set),
                    fields: Arc::new(fields),
                });
                Ok(())
            }
        }
    }

    fn exec_app<'h>(
        &self,
        stack: &mut Vec<LocValue>,
        n: u8,
        handler_ctx: Option<&'h SgHandlerRef<'h, '_>>,
    ) -> Result<(), VmError> {
        let n_args = n as usize;
        if stack.len() < n_args + 1 {
            return Err(VmError::StackUnderflow { op: "App" });
        }
        let func = stack.pop().unwrap();
        let args: Vec<LocValue> = stack.drain(stack.len() - n_args..).collect();
        self.apply(stack, func, args, handler_ctx)
    }

    fn apply<'h>(
        &self,
        stack: &mut Vec<LocValue>,
        func: LocValue,
        args: Vec<LocValue>,
        handler_ctx: Option<&'h SgHandlerRef<'h, '_>>,
    ) -> Result<(), VmError> {
        let mut func = func;
        let mut args = args;
        loop {
            match func {
                LocValue::Closure(Closure {
                    captures,
                    args: n_params,
                    body,
                }) => {
                    if args.len() < n_params as usize {
                        stack.push(LocValue::Partial {
                            func: Arc::new(LocValue::Closure(Closure {
                                captures,
                                args: n_params,
                                body,
                            })),
                            applied: Arc::new(args),
                        });
                        return Ok(());
                    }
                    let saved_len = stack.len();
                    for c in captures.iter() {
                        stack.push(c.clone());
                    }
                    for a in &args[..n_params as usize] {
                        stack.push(a.clone());
                    }
                    let result = self.exec_range(stack, body, handler_ctx);
                    let body_result = stack.pop().unwrap();
                    stack.truncate(saved_len);
                    let _result = result?;
                    let leftover = args.len() - n_params as usize;
                    if leftover > 0 {
                        func = body_result;
                        args = args[n_params as usize..].to_vec();
                        continue;
                    }
                    stack.push(body_result);
                    return Ok(());
                }

                LocValue::Partial {
                    func: pfunc,
                    applied,
                } => {
                    let total_args = [&applied[..], &args[..]].concat();
                    func = Arc::unwrap_or_clone(pfunc);
                    args = total_args;
                }

                LocValue::Builtin(b) => {
                    let result = self.exec_builtin(stack, b, &args, handler_ctx)?;
                    stack.push(result);
                    return Ok(());
                }

                LocValue::Panic => {
                    stack.push(LocValue::Panic);
                    return Ok(());
                }

                LocValue::LoopCont { step } => {
                    let new_cont = LocValue::LoopCont {
                        step: Arc::clone(&step),
                    };
                    func = Arc::unwrap_or_clone(step);
                    let i = args
                        .into_iter()
                        .next()
                        .expect("LoopCont applied with no arguments");
                    args = vec![i, new_cont];
                }

                other => {
                    return Err(VmError::TypeError {
                        op: "App",
                        expected: "callable",
                        got: format!("{other}"),
                    });
                }
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::match_same_arms
    )]
    fn exec_builtin<'h>(
        &self,
        stack: &mut Vec<LocValue>,
        b: BuiltinT,
        args: &[LocValue],
        handler_ctx: Option<&'h SgHandlerRef<'h, '_>>,
    ) -> Result<LocValue, VmError> {
        let plain = |v: &LocValue| -> Result<LocValue, VmError> {
            match v {
                LocValue::Panic
                | LocValue::Num(_)
                | LocValue::Tag(_)
                | LocValue::Bool(_)
                | LocValue::List(_)
                | LocValue::Record { .. }
                | LocValue::BuiltinsVar
                | LocValue::Builtin(_)
                | LocValue::Import(_)
                | LocValue::KolQuery(_, _) => Ok(v.clone()),
                other => Err(VmError::TypeError {
                    op: "builtin",
                    expected: "plain LocValue",
                    got: format!("{other}"),
                }),
            }
        };

        match b {
            BuiltinT::If => {
                if args.len() != 3 {
                    return Err(VmError::InvalidArgCount {
                        expected: 3,
                        got: args.len(),
                    });
                }
                let cond = plain(&args[0])?;
                match cond {
                    LocValue::Bool(true) => Ok(args[1].clone()),
                    LocValue::Bool(false) => Ok(args[2].clone()),
                    _ => Err(VmError::TypeError {
                        op: "If",
                        expected: "Bool",
                        got: format!("{cond}"),
                    }),
                }
            }

            BuiltinT::IntAdd(desc) => {
                if args.len() != 2 {
                    return Err(VmError::InvalidArgCount {
                        expected: 2,
                        got: args.len(),
                    });
                }
                let a = plain(&args[0])?;
                let b = plain(&args[1])?;
                match (&a, &b) {
                    (LocValue::Num(x), LocValue::Num(y)) => {
                        Ok(LocValue::Num(num_add(*x, *y, desc)?))
                    }
                    _ => Err(VmError::TypeError {
                        op: "IntAdd",
                        expected: "Num, Num",
                        got: format!("{a}, {b}"),
                    }),
                }
            }

            BuiltinT::IntMul(desc) => {
                if args.len() != 2 {
                    return Err(VmError::InvalidArgCount {
                        expected: 2,
                        got: args.len(),
                    });
                }
                let a = plain(&args[0])?;
                let b = plain(&args[1])?;
                match (&a, &b) {
                    (LocValue::Num(x), LocValue::Num(y)) => {
                        Ok(LocValue::Num(num_mul(*x, *y, desc)?))
                    }
                    _ => Err(VmError::TypeError {
                        op: "IntMul",
                        expected: "Num, Num",
                        got: format!("{a}, {b}"),
                    }),
                }
            }

            BuiltinT::IntNeg(desc) => {
                if args.len() != 1 {
                    return Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: args.len(),
                    });
                }
                let a = plain(&args[0])?;
                match &a {
                    LocValue::Num(x) => Ok(LocValue::Num(num_neg(*x, desc)?)),
                    _ => Err(VmError::TypeError {
                        op: "IntNeg",
                        expected: "Num",
                        got: format!("{a}"),
                    }),
                }
            }

            BuiltinT::IntEq => {
                if args.len() != 2 {
                    return Err(VmError::InvalidArgCount {
                        expected: 2,
                        got: args.len(),
                    });
                }
                let a = plain(&args[0])?;
                let b = plain(&args[1])?;
                match (&a, &b) {
                    (LocValue::Num(x), LocValue::Num(y)) => Ok(LocValue::Bool(x == y)),
                    _ => Err(VmError::TypeError {
                        op: "IntEq",
                        expected: "Num, Num",
                        got: format!("{a}, {b}"),
                    }),
                }
            }

            BuiltinT::IntGte0 => {
                if args.len() != 1 {
                    return Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: args.len(),
                    });
                }
                let a = plain(&args[0])?;
                match &a {
                    LocValue::Num(x) => {
                        let result = *x >= 0;
                        Ok(LocValue::Bool(result))
                    }
                    _ => Err(VmError::TypeError {
                        op: "IntGte0",
                        expected: "Num",
                        got: format!("{a}"),
                    }),
                }
            }

            BuiltinT::ListLength => {
                if args.len() != 1 {
                    return Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: args.len(),
                    });
                }
                let a = plain(&args[0])?;
                match &a {
                    LocValue::List(vs) => Ok(LocValue::Num(vs.len() as i64)),
                    _ => Err(VmError::TypeError {
                        op: "ListLength",
                        expected: "List",
                        got: format!("{a}"),
                    }),
                }
            }

            BuiltinT::ListIndexL => {
                if args.len() != 2 {
                    return Err(VmError::InvalidArgCount {
                        expected: 2,
                        got: args.len(),
                    });
                }
                let list = plain(&args[0])?;
                let idx = plain(&args[1])?;
                match (&list, &idx) {
                    (LocValue::List(vs), LocValue::Num(i)) => {
                        if *i < 0 || *i as usize >= vs.len() {
                            { Err(VmError::Panic("ListIndexL")) }
                        } else {
                            Ok(vs[*i as usize].clone())
                        }
                    }
                    _ => Err(VmError::TypeError {
                        op: "ListIndexL",
                        expected: "List, Num",
                        got: format!("{list}, {idx}"),
                    }),
                }
            }

            BuiltinT::ListViewL => {
                if args.is_empty() {
                    return Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: 0,
                    });
                }
                let list_arg = args
                    .iter()
                    .rev()
                    .find(|a| !matches!(a, LocValue::Panic))
                    .cloned()
                    .unwrap_or_else(|| args.last().cloned().unwrap());
                let a = plain(&list_arg)?;
                match &a {
                    LocValue::List(vs) => {
                        if vs.is_empty() {
                            { Err(VmError::Panic("ListViewL empty")) }
                        } else {
                            let left = vs[0].clone();
                            let rest = LocValue::List(Arc::new(vs[1..].to_vec()));
                            Ok(LocValue::Record {
                                tag_set: Arc::new(vec![self.common.viewl_tag_set]),
                                fields: Arc::new(vec![left, rest]), // TODO: where are the guarantees that this is the correct order?
                            })
                        }
                    }
                    _ => Err(VmError::TypeError {
                        op: "ListViewL",
                        expected: "List",
                        got: format!("{a}"),
                    }),
                }
            }

            BuiltinT::TagEq => {
                if args.len() != 2 {
                    return Err(VmError::InvalidArgCount {
                        expected: 2,
                        got: args.len(),
                    });
                }
                let a = plain(&args[0])?;
                let b = plain(&args[1])?;
                match (&a, &b) {
                    (LocValue::Tag(x), LocValue::Tag(y)) => Ok(LocValue::Bool(x == y)),
                    _ => Err(VmError::TypeError {
                        op: "TagEq",
                        expected: "Tag, Tag",
                        got: format!("{a}, {b}"),
                    }),
                }
            }

            BuiltinT::WWrap | BuiltinT::WUnwrap => {
                if args.len() != 1 {
                    return Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: args.len(),
                    });
                }
                Ok(args[0].clone())
            }

            BuiltinT::Loop => {
                if args.len() != 2 {
                    return Err(VmError::InvalidArgCount {
                        expected: 2,
                        got: args.len(),
                    });
                }
                let init = args[0].clone();
                let step = args[1].clone();
                let cont = LocValue::LoopCont {
                    step: Arc::new(step.clone()),
                };
                self.apply(stack, step, vec![init, cont], handler_ctx)?;
                Ok(stack.pop().unwrap())
            }

            BuiltinT::RecordGet => {
                if args.len() < 2 {
                    return Err(VmError::InvalidArgCount {
                        expected: 2,
                        got: args.len(),
                    });
                }
                let tag = plain(&args[0])?;
                let record = &args[1];

                let field_val = match (&tag, record) {
                    (LocValue::Tag(t), LocValue::Record { .. }) => self
                        .tags
                        .record_get_by_tag(record, *t)
                        .ok_or_else(|| VmError::RecordGetFailed {
                            record: record.clone(),
                            tag: self
                                .tags
                                .tag_to_name(*t)
                                .map(|x| String::from_utf8_lossy(x).to_string()),
                        }),
                    (LocValue::Tag(t), LocValue::BuiltinsVar) => {
                        let name_bytes = self.tags.tag_to_name(*t).unwrap_or(b"?");
                        let name = String::from_utf8_lossy(name_bytes);
                        let builtin = Self::resolve_builtin_by_name(&name)?;
                        Ok(LocValue::Builtin(builtin))
                    }
                    _ => Err(VmError::TypeError {
                        op: "RecordGet",
                        expected: "Tag, Record/BuiltinsVar",
                        got: format!("{tag}, {record}"),
                    }),
                }?;

                if args.len() > 2 {
                    self.apply(stack, field_val, args[2..].to_vec(), handler_ctx)?;
                    Ok(stack.pop().unwrap())
                } else {
                    Ok(field_val)
                }
            }

            BuiltinT::RecordKeepFields | BuiltinT::RecordDropFields => {
                if args.is_empty() {
                    return Err(VmError::InvalidArgCount {
                        expected: 2,
                        got: args.len(),
                    });
                }
                Ok(args.last().unwrap().clone())
            }

            BuiltinT::KolMkEventType => {
                if args.is_empty() {
                    return Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: 0,
                    });
                }
                let id = self.next_kol_id()?;
                Ok(LocValue::KolEventTypeId(LocMsgTypeId(id)))
            }

            BuiltinT::KolDataId | BuiltinT::KolId | BuiltinT::KolGear | BuiltinT::KolQuery => {
                if let Some(first) = args.first() {
                    Ok(first.clone())
                } else {
                    Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: 0,
                    })
                }
            }

            BuiltinT::KolUserId => {
                if let Some(first) = args.first() {
                    match first {
                        LocValue::Num(n) => Ok(LocValue::KolUserId(LocUserId(*n as u64))),
                        other => Err(VmError::TypeError {
                            op: "KolUserId",
                            expected: "Num",
                            got: format!("{other}"),
                        }),
                    }
                } else {
                    Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: 0,
                    })
                }
            }

            BuiltinT::KolMkGear => {
                if let Some(first) = args.first() {
                    match first {
                        LocValue::Record { .. } => {
                            let primary = self
                                .tags
                                .record_get_by_tag(first, self.common.primary)
                                .ok_or(VmError::TypeError {
                                op: "mk_gear",
                                expected: "record with .primary",
                                got: format!("{first}"),
                            })?;
                            let primary_msg_type = match self
                                .tags
                                .record_get_by_tag(&primary, self.common.r#type)
                            {
                                Some(LocValue::KolEventTypeId(id)) => id,
                                other => panic!(
                                    "mk_gear: .primary.type expected KolEventTypeId, got {other:?}"
                                ),
                            };
                            let primary_group = self
                                .tags
                                .record_get_by_tag(&primary, self.common.group)
                                .ok_or(VmError::TypeError {
                                    op: "mk_gear",
                                    expected: ".primary with .group field",
                                    got: format!("{primary}"),
                                })?;
                            let initial_cache = self
                                .tags
                                .record_get_by_tag(first, self.common.initial_cache)
                                .ok_or(VmError::TypeError {
                                    op: "mk_gear",
                                    expected: "record with .initial_cache",
                                    got: format!("{first}"),
                                })?;
                            let step = self.tags.record_get_by_tag(first, self.common.step).ok_or(
                                VmError::TypeError {
                                    op: "mk_gear",
                                    expected: "record with .step",
                                    got: format!("{first}"),
                                },
                            )?;
                            Ok(LocValue::KolGear(Box::new(KolGear {
                                primary_msg_type,
                                primary_group,
                                initial_cache,
                                step,
                            })))
                        }
                        other => Err(VmError::TypeError {
                            op: "mk_gear",
                            expected: "Record",
                            got: format!("{other}"),
                        }),
                    }
                } else {
                    Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: 0,
                    })
                }
            }

            BuiltinT::KolMkQuery => Ok(LocValue::KolQuery(0, 0)),
            BuiltinT::KolMkStateGraph => Ok(LocValue::KolStateGraph(Box::default())),

            BuiltinT::KolStateGraphApply => {
                if args.len() < 4 {
                    return Err(VmError::InvalidArgCount {
                        expected: 4,
                        got: args.len(),
                    });
                }
                let mut sg = match &args[0] {
                    LocValue::KolStateGraph(b) => *b.clone(),
                    LocValue::Builtin(BuiltinT::KolMkStateGraph) => {
                        crate::utils::state_graph::StateGraph::default()
                    }
                    other => {
                        return Err(VmError::TypeError {
                            op: "stategraph_apply",
                            expected: "StateGraph",
                            got: format!("{other}"),
                        });
                    }
                };
                let handler_closure = args[1].clone();
                let dep_resolver_closure = args[2].clone();
                let delta_arg = &args[3];

                let extract_delta_ids =
                    |record: &LocValue, field: &[u8]| -> Result<Vec<AnyLocEventId>, VmError> {
                        let Some(list) = self.tags.record_get(record, field) else {
                            return Ok(Vec::new());
                        };
                        let LocValue::List(vs) = list else {
                            return Err(VmError::TypeError {
                                op: "stategraph_apply",
                                expected: "List",
                                got: format!("{list}"),
                            });
                        };
                        vs.iter()
                            .enumerate()
                            .map(|(i, v)| match v {
                                LocValue::KolEventId(id) => Ok(*id),
                                other => Err(VmError::TypeError {
                                    op: "stategraph_apply delta",
                                    expected: "KolEventId",
                                    got: format!("delta[{i}] = {other}"),
                                }),
                            })
                            .collect()
                    };

                let added_ids = extract_delta_ids(delta_arg, b"added")?;
                let removed_ids = extract_delta_ids(delta_arg, b"removed")?;

                let delta = crate::utils::state_graph::DeltaList {
                    removed: removed_ids,
                    added: added_ids,
                };

                let _sender_tag = self
                    .tags
                    .name_to_tag(b"sender")
                    .expect("sender tag not found in tag registry")
                    as usize;
                let _body_tag =
                    self.tags
                        .name_to_tag(b"body")
                        .expect("body tag not found in tag registry") as usize;
                let event_resolver_core = match self.stage {
                    VmStage::Init { .. } => {
                        return Err(VmError::TypeError {
                            op: "stategraph_apply",
                            expected: "gear step context",
                            got: "init context".into(),
                        });
                    }
                    VmStage::Run { ctx, .. } => ctx,
                };

                let event_resolver =
                    |lid: AnyLocEventId| -> (crate::utils::sg_ord_map::SGEventId, LocValue) {
                        let stored = event_resolver_core
                            .get_stored_event(lid, std::clone::Clone::clone)
                            .expect("event_resolver: event not found in core");

                        let sg_event_id = crate::utils::sg_ord_map::SGEventId::new(
                            crate::utils::sg_ord_map::SGBucketId {
                                timestamp: stored.timestamp,
                                global_core_id: stored.global_core_id,
                            },
                            lid,
                        );

                        let _event_rec = LocValue::Record {
                            tag_set: Arc::new(vec![self.common.event_rec_tag_set]),
                            fields: Arc::new(vec![
                                LocValue::KolSenderId(stored.sender),
                                stored.body,
                            ]),
                        };

                        (sg_event_id, LocValue::KolEventId(lid))
                    };

                let dep_resolver = {
                    let dep_resolver_closure = dep_resolver_closure.clone();
                    move |dep: LocValue| -> crate::utils::state_graph::StateGraphOut<LocValue, LocValue> {
                        let result = self.call(&mut Vec::new(), dep_resolver_closure.clone(), vec![dep.clone()], None);
                        match result {
                            Ok(LocValue::KolStateGraphOut(sg)) => {
                                *sg
                            }
                            Ok(other) => panic!(
                                "stategraph_apply dep_resolver: expected KolStateGraphOut, got: {other}"
                            ),
                            Err(e) => panic!(
                                "stategraph_apply dep_resolver: call failed: {e:?}"
                            ),
                        }
                    }
                };

                let handler = |event: &LocValue,
                               ctx: &crate::utils::state_graph::HandlerCtx<
                    LocValue,
                    LocValue,
                    LocValue,
                    FadenoRuntime,
                    LocValue,
                    LocValue,
                >| {
                    let info = SgHandlerRef { ctx };
                    let _result = self.call(
                        &mut Vec::new(),
                        handler_closure.clone(),
                        vec![event.clone()],
                        Some(&info),
                    );
                };

                sg.apply(
                    &handler,
                    &event_resolver,
                    &dep_resolver,
                    event_resolver_core,
                    &delta,
                );

                Ok(LocValue::KolStateGraph(Box::new(sg)))
            }

            BuiltinT::KolStateGraphOut => match args.first() {
                Some(LocValue::KolStateGraph(b)) => {
                    Ok(LocValue::KolStateGraphOut(Box::new(StateGraphOut {
                        writes: b.writes.clone(),
                    })))
                }
                Some(other) => Err(VmError::TypeError {
                    op: "stategraph_out",
                    expected: "StateGraph",
                    got: format!("{other}"),
                }),
                None => Err(VmError::InvalidArgCount {
                    expected: 1,
                    got: 0,
                }),
            },

            BuiltinT::KolQueryDelta => {
                let since: (usize, usize) = match args.first() {
                    Some(LocValue::KolQuery(n, m)) => (*n as usize, *m as usize),
                    Some(LocValue::Builtin(BuiltinT::KolMkQuery)) => (0, 0),
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "query_delta",
                            expected: "KolQuery",
                            got: format!("{other:?}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 1,
                            got: 0,
                        });
                    }
                };
                let (core, group) = match &self.stage {
                    VmStage::Init { .. } => {
                        return Err(VmError::TypeError {
                            op: "query_delta",
                            expected: "gear step context",
                            got: "init context".into(),
                        });
                    }
                    VmStage::Run {
                        impure_core: Some(core),
                        impure_group,
                        ..
                    } => (*core, *impure_group),
                    VmStage::Run {
                        impure_core: None, ..
                    } => {
                        return Err(VmError::TypeError {
                            op: "query_delta",
                            expected: "gear step context with core",
                            got: "run context without core".into(),
                        });
                    }
                };

                let (added, removed, next_since) = group
                    .and_then(|group| {
                        core.query_events(group, since, |added, removed| {
                            (
                                added.iter().copied().map(LocValue::KolEventId).collect(),
                                removed.iter().copied().map(LocValue::KolEventId).collect(),
                                (since.0 + added.len(), since.1 + removed.len()),
                            )
                        })
                    })
                    .unwrap_or((Vec::new(), Vec::new(), since));

                let delta = LocValue::Record {
                    tag_set: Arc::new(vec![self.common.delta_tag_set]),
                    fields: Arc::new(vec![
                        LocValue::List(Arc::new(removed)), // .removed
                        LocValue::List(Arc::new(added)),   // .added
                    ]),
                };
                Ok(LocValue::Record {
                    tag_set: Arc::new(vec![self.common.query_result_tag_set]),
                    fields: Arc::new(vec![
                        LocValue::KolQuery(next_since.0 as u64, next_since.1 as u64),
                        delta,
                    ]),
                })
            }

            BuiltinT::KolSenderToUser => {
                let sid = match args.first() {
                    Some(LocValue::KolSenderId(id)) => *id,
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "sender-to>user",
                            expected: "KolSenderId",
                            got: format!("{other}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 1,
                            got: 0,
                        });
                    }
                };
                let ctx = match self.stage {
                    VmStage::Run { ctx, .. } => ctx,
                    _ => {
                        return Err(VmError::OutsideGearStepContext {
                            op: "sender-to>user",
                        });
                    }
                };
                let luid = ctx.sender_user(sid).unwrap_or_else(|| {
                    panic!("sender-to>user: no user mapping for sender {sid:?}")
                });
                Ok(LocValue::KolUserId(luid))
            }

            BuiltinT::KolSgCtxQuery => {
                let key = match args.first() {
                    Some(k) => k.clone(),
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 1,
                            got: 0,
                        });
                    }
                };

                let info = handler_ctx.expect("sgctx_query called outside handler context");
                match info.ctx.query(&key) {
                    Some(v) => Ok(mk_some(self.tags, self.common, v)),
                    None => Ok(mk_none(self.tags, self.common)),
                }
            }

            BuiltinT::KolSgCtxUpdate => {
                let key = match args.first() {
                    Some(k) => k.clone(),
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 2,
                            got: 0,
                        });
                    }
                };
                let val = match args.get(1) {
                    Some(v) => v.clone(),
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 2,
                            got: 1,
                        });
                    }
                };

                let info = handler_ctx.expect("sgctx_update called outside handler context");
                info.ctx.update(key, val);

                Ok(LocValue::Record {
                    tag_set: Arc::new(vec![1]), // ts_id=1 = empty record {}
                    fields: Arc::new(Vec::new()),
                })
            }

            BuiltinT::KolSgCtxDepQuery => {
                let dep = match args.first() {
                    Some(d) => d.clone(),
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 2,
                            got: 0,
                        });
                    }
                };
                let dep_key = match args.get(1) {
                    Some(k) => k.clone(),
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 2,
                            got: 1,
                        });
                    }
                };

                let info = handler_ctx.expect("sgctx_dep_query called outside handler context");
                match info.ctx.dep_query(&dep, &dep_key) {
                    Some(v) => Ok(mk_some(self.tags, self.common, v)),
                    None => Ok(mk_none(self.tags, self.common)),
                }
            }

            BuiltinT::KolResolveEvent => {
                let event_id: AnyLocEventId = match args.first() {
                    Some(LocValue::KolEventId(id)) => *id,
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "resolve_event",
                            expected: "KolEventId",
                            got: format!("{other:?}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 1,
                            got: 0,
                        });
                    }
                };
                let ctx = match self.stage {
                    VmStage::Run { ctx, .. } => ctx,
                    _ => {
                        return Err(VmError::OutsideGearStepContext {
                            op: "resolve_event",
                        });
                    }
                };
                let (sender, body) = ctx
                    .get_stored_event(event_id, |s| (s.sender, s.body.clone()))
                    .expect("resolve_event: event not found in core");

                Ok(LocValue::Record {
                    tag_set: Arc::new(vec![self.common.event_rec_tag_set]),
                    fields: Arc::new(vec![LocValue::KolSenderId(sender), body]),
                })
            }

            BuiltinT::KolResolveData => {
                let did = match args.first() {
                    Some(LocValue::KolDataId(id)) => *id,
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "resolve_data",
                            expected: "KolDataId",
                            got: format!("{other:?}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 1,
                            got: 0,
                        });
                    }
                };
                let ctx = match self.stage {
                    VmStage::Run { ctx, .. } => ctx,
                    _ => {
                        return Err(VmError::OutsideGearStepContext { op: "resolve_data" });
                    }
                };
                ctx.get_data(did, |(_data_id, content)| content.clone())
                    .ok_or(VmError::WireError {
                        op: "resolve_data",
                        detail: format!("data not found for LocDataId({})", did.0),
                    })
            }

            BuiltinT::KolUserEq => {
                let a = args.first();
                let b = args.get(1);
                match (a, b) {
                    (Some(LocValue::KolUserId(a)), Some(LocValue::KolUserId(b))) => {
                        Ok(LocValue::Bool(a == b))
                    }
                    (Some(_), Some(_)) => Err(VmError::TypeError {
                        op: "user_eq",
                        expected: "KolUserId, KolUserId",
                        got: format!("{}, {}", a.unwrap(), b.unwrap()),
                    }),
                    _ => Err(VmError::InvalidArgCount {
                        expected: 2,
                        got: args.len(),
                    }),
                }
            }

            BuiltinT::KolEventTypeId
            | BuiltinT::KolLocEventId
            | BuiltinT::KolTimestamp
            | BuiltinT::KolUserId
            | BuiltinT::KolStateGraphT
            | BuiltinT::KolStateGraphOutT
            | BuiltinT::KolDataId => {
                if let Some(first) = args.first() {
                    Ok(first.clone())
                } else {
                    Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: 0,
                    })
                }
            }

            BuiltinT::KolMkAnchorAgg => {
                Ok(LocValue::KolAnchorAgg(crate::utils::text::AnchorAgg::new()))
            }

            BuiltinT::KolAnchorAggApply => {
                let agg = match args.first() {
                    Some(LocValue::KolAnchorAgg(a)) => a.clone(),
                    Some(LocValue::Builtin(BuiltinT::KolMkAnchorAgg)) => {
                        crate::utils::text::AnchorAgg::new()
                    }
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "anchor_agg_apply",
                            expected: "AnchorAgg",
                            got: format!("{other}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 3,
                            got: args.len(),
                        });
                    }
                };
                let event_id = match args.get(1) {
                    Some(LocValue::KolEventId(id)) => *id,
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "anchor_agg_apply",
                            expected: "KolEventId",
                            got: format!("{other}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 3,
                            got: args.len(),
                        });
                    }
                };
                let upd = match args.get(2) {
                    Some(LocValue::KolTextUpd(u)) => u,
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "anchor_agg_apply",
                            expected: "TextUpd",
                            got: format!("{other}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 3,
                            got: args.len(),
                        });
                    }
                };
                let ctx = match &self.stage {
                    VmStage::Run {
                        ctx,
                        impure_core: Some(_),
                        ..
                    } => ctx,
                    _ => {
                        return Err(VmError::OutsideGearStepContext {
                            op: "anchor_agg_apply",
                        });
                    }
                };
                let loc_sender_eid = ctx
                    .get_stored_event(event_id, |s| {
                        crate::types::LocSenderEventId(s.sender, s.global_core_id, s.tx_id)
                    })
                    .expect("anchor_agg_apply: event not found in ctx");
                Ok(LocValue::KolAnchorAgg(agg.apply(loc_sender_eid, upd, ctx)))
            }

            BuiltinT::KolMkTextAgg => Ok(LocValue::KolTextAgg(crate::utils::text::TextAgg::new())),

            BuiltinT::KolTextAggApply => {
                let agg = match args.first() {
                    Some(LocValue::KolTextAgg(a)) => a.clone(),
                    Some(LocValue::Builtin(BuiltinT::KolMkTextAgg)) => {
                        crate::utils::text::TextAgg::new()
                    }
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "text_agg_apply",
                            expected: "TextAgg",
                            got: format!("{other}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 3,
                            got: args.len(),
                        });
                    }
                };
                let event_id = match args.get(1) {
                    Some(LocValue::KolEventId(id)) => *id,
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "text_agg_apply",
                            expected: "KolEventId",
                            got: format!("{other}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 3,
                            got: args.len(),
                        });
                    }
                };
                let upd = match args.get(2) {
                    Some(LocValue::KolTextUpd(u)) => u.clone(),
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "text_agg_apply",
                            expected: "TextUpd",
                            got: format!("{other}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 3,
                            got: args.len(),
                        });
                    }
                };
                let ctx = match &self.stage {
                    VmStage::Run {
                        ctx,
                        impure_core: Some(_),
                        ..
                    } => ctx,
                    _ => {
                        return Err(VmError::OutsideGearStepContext {
                            op: "text_agg_apply",
                        });
                    }
                };
                let loc_sender_eid = ctx
                    .get_stored_event(event_id, |s| {
                        LocSenderEventId(s.sender, s.global_core_id, s.tx_id)
                    })
                    .expect("text_agg_apply: event not found");
                Ok(LocValue::KolTextAgg(agg.apply(loc_sender_eid, &upd)))
            }

            BuiltinT::KolTextAggMerge => {
                let lhs = match args.first() {
                    Some(LocValue::KolTextAgg(a)) => a.clone(),
                    Some(LocValue::Builtin(BuiltinT::KolMkTextAgg)) => {
                        crate::utils::text::TextAgg::new()
                    }
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "text_agg_merge",
                            expected: "TextAgg",
                            got: format!("{other}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 2,
                            got: args.len(),
                        });
                    }
                };
                let rhs = match args.get(1) {
                    Some(LocValue::KolTextAgg(a)) => a.clone(),
                    Some(LocValue::Builtin(BuiltinT::KolMkTextAgg)) => {
                        crate::utils::text::TextAgg::new()
                    }
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "text_agg_merge",
                            expected: "TextAgg",
                            got: format!("{other}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 2,
                            got: args.len(),
                        });
                    }
                };
                Ok(LocValue::KolTextAgg(lhs.merge(&rhs)))
            }

            BuiltinT::KolSecondaryGet => {
                let gear = match args.first() {
                    Some(LocValue::KolGear(g)) => g.clone(),
                    Some(other) => {
                        return Err(VmError::TypeError {
                            op: "secondary_get",
                            expected: "KolGear",
                            got: format!("{other}"),
                        });
                    }
                    None => {
                        return Err(VmError::InvalidArgCount {
                            expected: 1,
                            got: 0,
                        });
                    }
                };
                let core = match &self.stage {
                    VmStage::Run {
                        impure_core: Some(core),
                        ..
                    } => *core,
                    _ => {
                        return Err(VmError::OutsideGearStepContext {
                            op: "secondary_get",
                        });
                    }
                };

                Ok(core.secondary_get(*gear))
            }

            BuiltinT::KolLoopIter => {
                if args.len() != 3 {
                    return Err(VmError::InvalidArgCount {
                        expected: 3,
                        got: args.len(),
                    });
                }
                let init = args[0].clone();
                let step = args[1].clone();
                let acc = args[2].clone();

                match init {
                    LocValue::Panic => Ok(acc), // empty list → return accumulator
                    LocValue::Record { ref fields, .. } if fields.len() >= 2 => {
                        let head = fields[0].clone();
                        let tail = fields[1].clone();
                        let new_acc =
                            self.call(stack, step.clone(), vec![head, acc], handler_ctx)?;
                        let next_init = self.call(
                            stack,
                            LocValue::Builtin(BuiltinT::KolIterList),
                            vec![tail],
                            handler_ctx,
                        )?;
                        self.call(
                            stack,
                            LocValue::Builtin(BuiltinT::KolLoopIter),
                            vec![next_init, step, new_acc],
                            handler_ctx,
                        )
                    }
                    other => Err(VmError::TypeError {
                        op: "loop_iter",
                        expected: "Panic or Record{head, tail}",
                        got: format!("{other}"),
                    }),
                }
            }

            BuiltinT::KolIterList => {
                if args.len() != 1 {
                    return Err(VmError::InvalidArgCount {
                        expected: 1,
                        got: args.len(),
                    });
                }
                let list = plain(&args[0])?;
                match &list {
                    LocValue::List(vs) if !vs.is_empty() => {
                        let head = vs[0].clone();
                        let tail = LocValue::List(Arc::new(vs[1..].to_vec()));
                        Ok(LocValue::Record {
                            tag_set: Arc::new(vec![self.common.viewl_tag_set]),
                            fields: Arc::new(vec![head, tail]), // TODO: where are the guarantees that this is the correct order?
                        })
                    }
                    LocValue::List(_) => Ok(LocValue::Panic), // empty list → stop iteration
                    _ => Err(VmError::TypeError {
                        op: "iter_list",
                        expected: "List",
                        got: format!("{list}"),
                    }),
                }
            }

            BuiltinT::Eq
            | BuiltinT::Refl
            | BuiltinT::PropLteTrans
            | BuiltinT::PropListViewlDec
            | BuiltinT::Any
            | BuiltinT::Bool
            | BuiltinT::Never
            | BuiltinT::Tag
            | BuiltinT::TypePlus
            | BuiltinT::RowPlus
            | BuiltinT::List
            | BuiltinT::Int(_)
            | BuiltinT::W
            | BuiltinT::OpaqueVal(_)
            | BuiltinT::KolSenderId
            | BuiltinT::KolLocalUserId
            | BuiltinT::KolTextUpdT
            | BuiltinT::KolAnchorAggT
            | BuiltinT::KolTextAggT
            | BuiltinT::KolUnEventType
            | BuiltinT::KolPrimaryT
            | BuiltinT::KolSecondaryT
            | BuiltinT::KolPropQueryEvents
            | BuiltinT::KolPropMkPrimary
            | BuiltinT::KolPropMkSecondary
            | BuiltinT::KolResolveData => Ok(LocValue::Panic),
        }
    }

    fn next_kol_id(&self) -> Result<u64, VmError> {
        match &self.stage {
            VmStage::Init {
                kol_id_counter_next,
            } => {
                let id = kol_id_counter_next.get();
                kol_id_counter_next.set(id + 1);
                Ok(id)
            }
            VmStage::Run { .. } => Err(VmError::TypeError {
                op: "kol_id",
                expected: "init context",
                got: "run context".into(),
            }),
        }
    }

    fn resolve_builtin_by_name(name: &str) -> Result<BuiltinT, VmError> {
        match name {
            "Any" => Ok(BuiltinT::Any),
            "Bool" => Ok(BuiltinT::Bool),
            "Eq" => Ok(BuiltinT::Eq),
            "loop" => Ok(BuiltinT::Loop),
            "if" => Ok(BuiltinT::If),
            "int_==" => Ok(BuiltinT::IntEq),
            "int_>=0" => Ok(BuiltinT::IntGte0),
            "List" => Ok(BuiltinT::List),
            "list_indexl" => Ok(BuiltinT::ListIndexL),
            "list_length" => Ok(BuiltinT::ListLength),
            "list_viewl" => Ok(BuiltinT::ListViewL),
            "Never" => Ok(BuiltinT::Never),
            "list_viewl~dec" => Ok(BuiltinT::PropListViewlDec),
            "<=~trans" => Ok(BuiltinT::PropLteTrans),
            "record_drop_fields" => Ok(BuiltinT::RecordDropFields),
            "record_get" => Ok(BuiltinT::RecordGet),
            "record_keep_fields" => Ok(BuiltinT::RecordKeepFields),
            "refl" => Ok(BuiltinT::Refl),
            "Row^" => Ok(BuiltinT::RowPlus),
            "Tag" => Ok(BuiltinT::Tag),
            "tag_==" => Ok(BuiltinT::TagEq),
            "Type^" => Ok(BuiltinT::TypePlus),
            "W" => Ok(BuiltinT::W),
            "w_unwrap" => Ok(BuiltinT::WUnwrap),
            "w_wrap" => Ok(BuiltinT::WWrap),
            "Int" => Ok(BuiltinT::Int(NumDesc::Inf)),
            "I8" => Ok(BuiltinT::Int(NumDesc::Fin {
                nonneg: false,
                bits: Bits::Bits8,
            })),
            "I16" => Ok(BuiltinT::Int(NumDesc::Fin {
                nonneg: false,
                bits: Bits::Bits16,
            })),
            "I32" => Ok(BuiltinT::Int(NumDesc::Fin {
                nonneg: false,
                bits: Bits::Bits32,
            })),
            "I64" => Ok(BuiltinT::Int(NumDesc::Fin {
                nonneg: false,
                bits: Bits::Bits64,
            })),
            "I8+" => Ok(BuiltinT::Int(NumDesc::Fin {
                nonneg: true,
                bits: Bits::Bits8,
            })),
            "I16+" => Ok(BuiltinT::Int(NumDesc::Fin {
                nonneg: true,
                bits: Bits::Bits16,
            })),
            "I32+" => Ok(BuiltinT::Int(NumDesc::Fin {
                nonneg: true,
                bits: Bits::Bits32,
            })),
            "I64+" => Ok(BuiltinT::Int(NumDesc::Fin {
                nonneg: true,
                bits: Bits::Bits64,
            })),
            n if n.contains("_add") => Ok(BuiltinT::IntAdd(NumDesc::Inf)),
            n if n.contains("_mul") => Ok(BuiltinT::IntMul(NumDesc::Inf)),
            n if n.contains("_neg") => Ok(BuiltinT::IntNeg(NumDesc::Inf)),
            "DataId" => Ok(BuiltinT::KolDataId),
            "UserId" => Ok(BuiltinT::KolUserId),
            "mk_event_type" => Ok(BuiltinT::KolMkEventType),
            "Gear" => Ok(BuiltinT::KolGear),
            "mk_gear" => Ok(BuiltinT::KolMkGear),
            "Query" => Ok(BuiltinT::KolQuery),
            "Id" => Ok(BuiltinT::KolId),
            "query_delta" => Ok(BuiltinT::KolQueryDelta),
            "sender-to>user" => Ok(BuiltinT::KolSenderToUser),
            "mk_query" => Ok(BuiltinT::KolMkQuery),
            "mk_stategraph" => Ok(BuiltinT::KolMkStateGraph),
            "stategraph_apply" => Ok(BuiltinT::KolStateGraphApply),
            "stategraph_out" => Ok(BuiltinT::KolStateGraphOut),
            "sgctx_query" => Ok(BuiltinT::KolSgCtxQuery),
            "sgctx_update" => Ok(BuiltinT::KolSgCtxUpdate),
            "sgctx_dep_query" => Ok(BuiltinT::KolSgCtxDepQuery),
            "resolve_event" => Ok(BuiltinT::KolResolveEvent),
            "resolve_data" => Ok(BuiltinT::KolResolveData),
            "user_eq" => Ok(BuiltinT::KolUserEq),
            "EventTypeId" => Ok(BuiltinT::KolEventTypeId),
            "LocEventId" => Ok(BuiltinT::KolLocEventId),
            "Timestamp" => Ok(BuiltinT::KolTimestamp),
            "LocalUserId" => Ok(BuiltinT::KolUserId),
            "StateGraph" => Ok(BuiltinT::KolStateGraphT),
            "StateGraphOut" => Ok(BuiltinT::KolStateGraphOutT),
            "mk_anchor_agg" => Ok(BuiltinT::KolMkAnchorAgg),
            "anchor_agg_apply" => Ok(BuiltinT::KolAnchorAggApply),
            "mk_text_agg" => Ok(BuiltinT::KolMkTextAgg),
            "text_agg_apply" => Ok(BuiltinT::KolTextAggApply),
            "text_agg_merge" => Ok(BuiltinT::KolTextAggMerge),
            "secondary_get" => Ok(BuiltinT::KolSecondaryGet),
            "loop_iter" => Ok(BuiltinT::KolLoopIter),
            "iter_list" => Ok(BuiltinT::KolIterList),
            "un_event_type" => Ok(BuiltinT::KolUnEventType),
            "Primary" => Ok(BuiltinT::KolPrimaryT),
            "Secondary" => Ok(BuiltinT::KolSecondaryT),
            "~query_events" => Ok(BuiltinT::KolPropQueryEvents),
            "SenderId" => Ok(BuiltinT::KolSenderId),
            _ => Err(VmError::Panic("unknown builtin")),
        }
    }
}

fn num_add(x: i64, y: i64, desc: NumDesc) -> Result<i64, VmError> {
    let r = x.checked_add(y).ok_or(VmError::NumericOverflow)?;
    check_range(r, desc)
}

fn num_mul(x: i64, y: i64, desc: NumDesc) -> Result<i64, VmError> {
    let r = x.checked_mul(y).ok_or(VmError::NumericOverflow)?;
    check_range(r, desc)
}

fn num_neg(x: i64, desc: NumDesc) -> Result<i64, VmError> {
    let r = x.checked_neg().ok_or(VmError::NumericOverflow)?;
    check_range(r, desc)
}

fn check_range(v: i64, desc: NumDesc) -> Result<i64, VmError> {
    match desc {
        NumDesc::Inf => Ok(v),
        NumDesc::Fin { nonneg, bits } => {
            let (min, max): (i64, i64) = match (nonneg, bits) {
                (false, Bits::Bits8) => (i64::from(i8::MIN), i64::from(i8::MAX)),
                (true, Bits::Bits8) => (0, i64::from(u8::MAX)),
                (false, Bits::Bits16) => (i64::from(i16::MIN), i64::from(i16::MAX)),
                (true, Bits::Bits16) => (0, i64::from(u16::MAX)),
                (false, Bits::Bits32) => (i64::from(i32::MIN), i64::from(i32::MAX)),
                (true, Bits::Bits32) => (0, i64::from(u32::MAX)),
                (false, Bits::Bits64) => (i64::MIN, i64::MAX),
                (true, Bits::Bits64) => (0, -1), // u64 doesn't fit in i64
            };
            if v >= min && v <= max {
                Ok(v)
            } else {
                Err(VmError::NumericOverflow)
            }
        }
    }
}

fn mk_none(_tags: &TagRegistry, common: CommonTags) -> LocValue {
    LocValue::Record {
        tag_set: Arc::new(vec![common.opt_none_tag_set]),
        fields: Arc::new(vec![LocValue::Bool(false)]),
    }
}

fn mk_some(_tags: &TagRegistry, common: CommonTags, v: LocValue) -> LocValue {
    LocValue::Record {
        tag_set: Arc::new(vec![common.opt_some_tag_set]),
        fields: Arc::new(vec![LocValue::Bool(true), v]),
    }
}

#[cfg(test)]
#[path = "vm_tests.rs"]
mod vm_tests;
