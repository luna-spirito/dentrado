//! The textual identity of a serialized layout.
//!
//! A **shape** is the canonical description of how a value is laid out on the
//! wire, written by walking its definition: names, field order, field types.
//! It is the identity a journal schema is interned by, so two values share an
//! id exactly when their bytes mean the same thing.
//!
//! The walk is deliberately **layout-based, not syntax-based**:
//!
//! - `#[serde(skip)]` removes a field or variant, because it removes it from
//!   the encoding; a skipped field never appears.
//! - `#[serde(transparent)]` collapses a newtype to its inner shape, because
//!   that is what the encoding is.
//! - `#[serde(rename)]` / `rename_all` are **ignored**: postcard encodes no
//!   names, so a rename is not a different layout and must not mint a new
//!   schema id.
//! - Attributes the walk cannot follow (`flatten`, `with`, `tag`,
//!   `serialize_with`, ...) are compile errors, never silent divergence. A
//!   type with a hand-written encoding writes a hand-written [`Shape`] impl
//!   beside it; there is no per-field override.
//!
//! # Faithfulness
//!
//! A journaled type's serde impl is part of its schema: it **encodes the
//! value**. It may validate on the way in, but it must not re-encode it — a
//! base64 string for raw bytes, say. That is a presentation choice for a
//! textual wire format; it has no place in the log, where it inflates the
//! record, parses on every replay, and ties a permanent schema to a spelling
//! that may change. When a type must serve a human-readable wire as well, the
//! presentation lives behind `is_human_readable` (postcard answers `false`,
//! JSON `true`) or in a wire-only type; the binary branch is the shape.
//!
//! # Grammar
//!
//! ```text
//! unit struct       Name
//! newtype struct    Name(Inner)
//! tuple struct      Name(A,B)
//! named struct      Name{a:A,b:B}
//! enum              Name{Variant,Other(A),Third{x:X,y:Y}}
//! Vec<T>            Vec<T>
//! Option<T>         Option<T>
//! [T; N]            [T;N]
//! tuple             (A,B)
//! ```
//!
//! No spaces, no trailing punctuation, declaration order throughout: the text
//! is deterministic, so a change in it is a review-visible diff.

use std::marker::PhantomData;

/// A type whose serialized layout is identifiable by a string.
///
/// Implement it with `#[derive(Shape)]`; the hand impls here cover primitives
/// and std containers. The method is a function rather than a constant
/// because composing it across generics needs runtime string building.
pub trait Shape {
    /// The shape of this type, as described in the module documentation.
    fn shape() -> String;
}

macro_rules! shape_as {
    ($($t:ty => $name:literal),* $(,)?) => {
        $(
            impl Shape for $t {
                fn shape() -> String {
                    $name.to_owned()
                }
            }
        )*
    };
}

shape_as! {
    () => "()",
    bool => "bool",
    char => "char",
    f32 => "f32",
    f64 => "f64",
    String => "String",
    &'static str => "&str",
    u8 => "u8",
    u16 => "u16",
    u32 => "u32",
    u64 => "u64",
    u128 => "u128",
    usize => "usize",
    i8 => "i8",
    i16 => "i16",
    i32 => "i32",
    i64 => "i64",
    i128 => "i128",
    isize => "isize",
}

impl<T: Shape> Shape for Vec<T> {
    fn shape() -> String {
        format!("Vec<{}>", T::shape())
    }
}

impl<T: Shape> Shape for Option<T> {
    fn shape() -> String {
        format!("Option<{}>", T::shape())
    }
}

impl<T: Shape, const N: usize> Shape for [T; N] {
    fn shape() -> String {
        format!("[{};{N}]", T::shape())
    }
}

/// A box is an allocation detail, not a layout: the bytes are `T`'s.
impl<T: Shape> Shape for Box<T> {
    fn shape() -> String {
        T::shape()
    }
}

/// A byte slice encodes exactly as a `Vec<u8>`: a length prefix and the bytes.
/// Borrowing the body of a record instead of owning it changes nothing on the
/// wire, so it must not change the schema identity either.
impl Shape for &[u8] {
    fn shape() -> String {
        "Vec<u8>".to_owned()
    }
}

/// Phantom data names no bytes.
impl<T> Shape for PhantomData<T> {
    fn shape() -> String {
        "PhantomData".to_owned()
    }
}

macro_rules! shape_tuple {
    ($(($($t:ident),+)),* $(,)?) => {
        $(
            impl<$($t: Shape),+> Shape for ($($t,)+) {
                fn shape() -> String {
                    let parts = [$(<$t as Shape>::shape()),+];
                    format!("({})", parts.join(","))
                }
            }
        )*
    };
}

shape_tuple! {
    (A),
    (A, B),
    (A, B, C),
    (A, B, C, D),
    (A, B, C, D, E),
    (A, B, C, D, E, F),
}
