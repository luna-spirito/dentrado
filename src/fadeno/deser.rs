#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::missing_errors_doc
)]

use std::sync::Arc;

use crate::fadeno::types::{
    Bits, BuiltinT, Compiled, Instr, InstrRange, LocValue, NumDesc, TagRegistry,
};

#[derive(Debug)]
pub enum DeError {
    UnexpectedEof(&'static str),
    UnknownTag { context: &'static str, tag: u8 },
    InvalidBool(u8),
    Utf8(std::str::Utf8Error),
}

impl std::fmt::Display for DeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeError::UnexpectedEof(ctx) => write!(f, "unexpected EOF while reading {ctx}"),
            DeError::UnknownTag { context, tag } => {
                write!(f, "unknown tag {tag:#x} while reading {context}")
            }
            DeError::InvalidBool(v) => write!(f, "invalid boolean value {v}"),
            DeError::Utf8(e) => write!(f, "utf-8 error: {e}"),
        }
    }
}

impl std::error::Error for DeError {}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read_bytes(&mut self, n: usize, ctx: &'static str) -> Result<&'a [u8], DeError> {
        if self.remaining() < n {
            return Err(DeError::UnexpectedEof(ctx));
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn read_u8(&mut self, ctx: &'static str) -> Result<u8, DeError> {
        Ok(self.read_bytes(1, ctx)?[0])
    }

    fn read_u32_le(&mut self, ctx: &'static str) -> Result<u32, DeError> {
        let b = self.read_bytes(4, ctx)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_u64_le(&mut self, ctx: &'static str) -> Result<u64, DeError> {
        let b = self.read_bytes(8, ctx)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn read_i64_le(&mut self, ctx: &'static str) -> Result<i64, DeError> {
        let b = self.read_bytes(8, ctx)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn read_bool8(&mut self, ctx: &'static str) -> Result<bool, DeError> {
        match self.read_u8(ctx)? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(DeError::InvalidBool(v)),
        }
    }

    fn read_byte_string(&mut self, ctx: &'static str) -> Result<&'a [u8], DeError> {
        let len = self.read_u32_le(ctx)? as usize;
        self.read_bytes(len, ctx)
    }

    fn read_vec<T>(
        &mut self,
        ctx: &'static str,
        read_item: impl Fn(&mut Self) -> Result<T, DeError>,
    ) -> Result<Vec<T>, DeError> {
        let len = self.read_u32_le(ctx)? as usize;
        let mut items = Vec::with_capacity(len);
        for _ in 0..len {
            items.push(read_item(self)?);
        }
        Ok(items)
    }
}

pub(crate) fn deserialize_compile_result(data: &[u8]) -> Result<Compiled, DeError> {
    let mut cur = Cursor::new(data);

    let idents_len = cur.read_u32_le("idents_vec_len")? as usize;
    let mut idents = Vec::with_capacity(idents_len);
    for _ in 0..idents_len {
        let raw = cur.read_byte_string("ident_bytes")?;
        let _is_op = cur.read_bool8("ident_is_op")?;
        idents.push(raw.to_vec());
    }

    let tag_sets_len = cur.read_u32_le("tag_sets_map_len")? as usize;
    let mut tag_sets = Vec::with_capacity(tag_sets_len);
    for _ in 0..tag_sets_len {
        let ts_len = cur.read_u32_le("tag_set_len")? as usize;
        let mut ts = Vec::with_capacity(ts_len);
        for _ in 0..ts_len {
            let k = cur.read_u64_le("tag_set_key")?;
            let v = cur.read_u8("tag_set_val")?;
            ts.push((k, v));
        }
        let ts_id = cur.read_u64_le("tag_set_id")?;
        tag_sets.push((ts, ts_id));
    }

    let modules_raw = cur.read_vec("modules", |c| {
        c.read_vec("module_instrs", read_instr_nested)
    })?;

    let mut pool: Vec<Instr> = Vec::new();
    let mut constants: Vec<LocValue> = Vec::new();
    let mut module_ranges: Vec<InstrRange> = Vec::new();

    for module_instrs in &modules_raw {
        let range = flatten_into_pool(&mut pool, &mut constants, module_instrs);
        module_ranges.push(range);
    }

    Ok(Compiled {
        tags: TagRegistry::new(idents, tag_sets),
        constants,
        pool,
        module_ranges,
    })
}

#[derive(Debug, Clone)]
enum InstrNested {
    Push(LocValue),
    PushVar,
    Copy(u8),
    PopVar,
    App(u8),
    Closure {
        captures: u8,
        args: u8,
        body: Vec<InstrNested>,
    },
    IfElse {
        then_: Vec<InstrNested>,
        else_: Vec<InstrNested>,
    },
    MkList(u8),
    MkRecord(u8),
    MkQRecord {
        tag_set: u64,
        n: u8,
    },
    RecordCat,
}

enum DeferredSub {
    ClosureBody {
        slot: usize,
        captures: u8,
        args: u8,
        body: Vec<InstrNested>,
    },
    IfElse {
        slot: usize,
        then_: Vec<InstrNested>,
        else_: Vec<InstrNested>,
    },
}

fn flatten_into_pool(
    pool: &mut Vec<Instr>,
    constants: &mut Vec<LocValue>,
    nested: &[InstrNested],
) -> InstrRange {
    let start = pool.len() as u32;

    let mut deferred: Vec<DeferredSub> = Vec::new();
    for instr in nested {
        match instr {
            InstrNested::Push(v) => {
                let idx = constants.len() as u32;
                constants.push(v.clone());
                pool.push(Instr::PushConst(idx));
            }
            InstrNested::PushVar => pool.push(Instr::PushVar),
            InstrNested::Copy(n) => pool.push(Instr::Copy(*n)),
            InstrNested::PopVar => pool.push(Instr::PopVar),
            InstrNested::App(n) => pool.push(Instr::App(*n)),
            InstrNested::MkList(n) => pool.push(Instr::MkList(*n)),
            InstrNested::MkRecord(n) => pool.push(Instr::MkRecord(*n)),
            InstrNested::MkQRecord { tag_set, n } => {
                pool.push(Instr::MkQRecord {
                    tag_set: *tag_set,
                    n: *n,
                });
            }
            InstrNested::RecordCat => pool.push(Instr::RecordCat),
            InstrNested::Closure {
                captures,
                args,
                body,
            } => {
                let slot = pool.len();
                pool.push(Instr::Closure {
                    captures: *captures,
                    args: *args,
                    body: InstrRange { start: 0, len: 0 }, // placeholder
                });
                deferred.push(DeferredSub::ClosureBody {
                    slot,
                    captures: *captures,
                    args: *args,
                    body: body.clone(),
                });
            }
            InstrNested::IfElse { then_, else_ } => {
                let slot = pool.len();
                pool.push(Instr::IfElse {
                    then_: InstrRange { start: 0, len: 0 }, // placeholder
                    else_: InstrRange { start: 0, len: 0 }, // placeholder
                });
                deferred.push(DeferredSub::IfElse {
                    slot,
                    then_: then_.clone(),
                    else_: else_.clone(),
                });
            }
        }
    }

    let len = pool.len() as u32 - start;
    let range = InstrRange { start, len };

    for d in deferred {
        match d {
            DeferredSub::ClosureBody {
                slot,
                captures,
                args,
                body,
            } => {
                let body_range = flatten_into_pool(pool, constants, &body);
                pool[slot] = Instr::Closure {
                    captures,
                    args,
                    body: body_range,
                };
            }
            DeferredSub::IfElse { slot, then_, else_ } => {
                let then_range = flatten_into_pool(pool, constants, &then_);
                let else_range = flatten_into_pool(pool, constants, &else_);
                pool[slot] = Instr::IfElse {
                    then_: then_range,
                    else_: else_range,
                };
            }
        }
    }

    range
}

fn read_bits(cur: &mut Cursor) -> Result<Bits, DeError> {
    match cur.read_u8("bits_tag")? {
        0 => Ok(Bits::Bits8),
        1 => Ok(Bits::Bits16),
        2 => Ok(Bits::Bits32),
        3 => Ok(Bits::Bits64),
        tag => Err(DeError::UnknownTag {
            context: "Bits",
            tag,
        }),
    }
}

fn read_num_desc(cur: &mut Cursor) -> Result<NumDesc, DeError> {
    match cur.read_u8("num_desc_tag")? {
        0 => {
            let nonneg = cur.read_bool8("num_desc_nonneg")?;
            let bits = read_bits(cur)?;
            Ok(NumDesc::Fin { nonneg, bits })
        }
        1 => Ok(NumDesc::Inf),
        tag => Err(DeError::UnknownTag {
            context: "NumDesc",
            tag,
        }),
    }
}

fn read_builtin(cur: &mut Cursor) -> Result<BuiltinT, DeError> {
    match cur.read_u8("builtin_tag")? {
        0 => Ok(BuiltinT::Any),
        1 => Ok(BuiltinT::Bool),
        2 => Ok(BuiltinT::Eq),
        3 => Ok(BuiltinT::Loop),
        4 => Ok(BuiltinT::If),
        5 => Ok(BuiltinT::IntEq),
        6 => Ok(BuiltinT::IntGte0),
        7 => Ok(BuiltinT::List),
        8 => Ok(BuiltinT::ListIndexL),
        9 => Ok(BuiltinT::ListLength),
        10 => Ok(BuiltinT::ListViewL),
        11 => Ok(BuiltinT::Never),
        12 => Ok(BuiltinT::OpaqueVal(cur.read_u64_le("opaque_id")?)),
        13 => Ok(BuiltinT::RecordDropFields),
        14 => Ok(BuiltinT::RecordGet),
        15 => Ok(BuiltinT::RecordKeepFields),
        16 => Ok(BuiltinT::Refl),
        17 => Ok(BuiltinT::RowPlus),
        18 => Ok(BuiltinT::Tag),
        19 => Ok(BuiltinT::TagEq),
        20 => Ok(BuiltinT::TypePlus),
        21 => Ok(BuiltinT::W),
        22 => Ok(BuiltinT::WUnwrap),
        23 => Ok(BuiltinT::WWrap),
        24 => Ok(BuiltinT::Int(read_num_desc(cur)?)),
        25 => Ok(BuiltinT::IntAdd(read_num_desc(cur)?)),
        26 => Ok(BuiltinT::IntMul(read_num_desc(cur)?)),
        27 => Ok(BuiltinT::IntNeg(read_num_desc(cur)?)),
        28 => Ok(BuiltinT::PropListViewlDec),
        29 => Ok(BuiltinT::PropLteTrans),
        30 => Ok(BuiltinT::KolDataId),
        31 => Ok(BuiltinT::KolGear),
        32 => Ok(BuiltinT::KolId),
        33 => Ok(BuiltinT::KolLocEventId),
        34 => Ok(BuiltinT::KolMkEventType),
        35 => Ok(BuiltinT::KolMkGear),
        36 => Ok(BuiltinT::KolQuery),
        37 => Ok(BuiltinT::KolUserId),
        38 => Ok(BuiltinT::KolEventTypeId),
        39 => Ok(BuiltinT::KolMkStateGraph),
        40 => Ok(BuiltinT::KolQueryDelta),
        41 => Ok(BuiltinT::KolSenderToUser),
        42 => Ok(BuiltinT::KolSgCtxDepQuery),
        43 => Ok(BuiltinT::KolSgCtxQuery),
        44 => Ok(BuiltinT::KolSgCtxUpdate),
        45 => Ok(BuiltinT::KolStateGraphApply),
        46 => Ok(BuiltinT::KolStateGraphOut),
        47 => Ok(BuiltinT::KolStateGraphOutT),
        48 => Ok(BuiltinT::KolStateGraphT),
        49 => Ok(BuiltinT::KolTimestamp),
        tag => Err(DeError::UnknownTag {
            context: "BuiltinT",
            tag,
        }),
    }
}

fn read_value(cur: &mut Cursor) -> Result<LocValue, DeError> {
    match cur.read_u8("value_tag")? {
        0 => Ok(LocValue::Num(cur.read_i64_le("vnum")?)),
        1 => Ok(LocValue::Tag(cur.read_u64_le("vtag")?)),
        2 => Ok(LocValue::Bool(cur.read_bool8("vbool")?)),
        3 => Ok(LocValue::List(Arc::new(cur.read_vec("vlist", read_value)?))),
        4 => {
            let tag_set = cur.read_i64_le("vrecord_ts")? as u64;
            let fields = cur.read_vec("vrecord_fields", read_value)?;
            Ok(LocValue::Record {
                tag_set: Arc::new(vec![tag_set]),
                fields: Arc::new(fields),
            })
        }
        5 => Ok(LocValue::BuiltinsVar),
        6 => Ok(LocValue::Builtin(read_builtin(cur)?)),
        7 => Ok(LocValue::Panic),
        8 => Ok(LocValue::Import(cur.read_u64_le("vimport")?)),
        tag => Err(DeError::UnknownTag {
            context: "Value",
            tag,
        }),
    }
}

fn read_instr_nested(cur: &mut Cursor) -> Result<InstrNested, DeError> {
    match cur.read_u8("instr_tag")? {
        0 => Ok(InstrNested::Push(read_value(cur)?)),
        1 => Ok(InstrNested::PushVar),
        2 => Ok(InstrNested::Copy(cur.read_u8("icopy_n")?)),
        3 => Ok(InstrNested::PopVar),
        4 => Ok(InstrNested::App(cur.read_u8("iapp_n")?)),
        5 => {
            let captures = cur.read_u8("iclosure_captures")?;
            let args = cur.read_u8("iclosure_args")?;
            let body = cur.read_vec("iclosure_body", read_instr_nested)?;
            Ok(InstrNested::Closure {
                captures,
                args,
                body,
            })
        }
        6 => {
            let then_ = cur.read_vec("iif_then", read_instr_nested)?;
            let else_ = cur.read_vec("iif_else", read_instr_nested)?;
            Ok(InstrNested::IfElse { then_, else_ })
        }
        7 => Ok(InstrNested::MkList(cur.read_u8("imklist_n")?)),
        8 => Ok(InstrNested::MkRecord(cur.read_u8("imkrecord_n")?)),
        9 => Ok(InstrNested::MkQRecord {
            tag_set: cur.read_u64_le("imkqrecord_ts")?,
            n: cur.read_u8("imkqrecord_n")?,
        }),
        10 => Ok(InstrNested::RecordCat),
        tag => Err(DeError::UnknownTag {
            context: "Instr",
            tag,
        }),
    }
}
