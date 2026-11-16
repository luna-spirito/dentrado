//! The shape grammar, pinned exactly: these strings are schema identities.

#![allow(dead_code)]
//!
//! A change here is a change to every id a log will mint, so the assertions
//! spell the text out rather than comparing to a builder.

use dentrado_types::Shape;

#[derive(Shape)]
struct Pair {
    a: u64,
    b: Option<Vec<u8>>,
}

#[derive(Shape)]
struct Tuple(u8, [u8; 32]);

#[derive(Shape)]
struct Unit;

#[derive(Shape)]
#[serde(transparent)]
struct Newtype(Vec<u8>);

#[derive(Shape)]
struct Skipped {
    a: u8,
    #[serde(skip)]
    hidden: u8,
    c: bool,
}

#[derive(Shape)]
enum Message {
    Ping,
    Point(i32, i32),
    Move { x: i32, y: i32 },
}

#[derive(Shape)]
#[serde(rename_all = "snake_case")]
enum Renamed {
    FirstOne,
    SecondOne { value: u8 },
}

#[derive(Shape)]
enum WithSkipped {
    A,
    #[serde(skip)]
    B,
    C,
}

#[derive(Shape)]
struct Generic<T> {
    value: T,
    rest: Vec<T>,
}

/// The `RepoAssetPath` case: a hand-written encoding whose bytes are exactly
/// `String`'s, with a hand-written `Shape` beside it.
struct ValidatedPath(String);

impl Shape for ValidatedPath {
    fn shape() -> String {
        String::shape()
    }
}

#[derive(Shape)]
struct WithManual {
    path: ValidatedPath,
}

#[test]
fn a_struct_lists_its_fields_in_order() {
    assert_eq!(Pair::shape(), "Pair{a:u64,b:Option<Vec<u8>>}");
    assert_eq!(Tuple::shape(), "Tuple(u8,[u8;32])");
    assert_eq!(Unit::shape(), "Unit");
}

#[test]
fn a_transparent_newtype_is_its_inner_shape() {
    assert_eq!(Newtype::shape(), "Vec<u8>");
}

#[test]
fn a_skipped_field_is_absent_not_empty() {
    assert_eq!(Skipped::shape(), "Skipped{a:u8,c:bool}");
    assert_eq!(WithSkipped::shape(), "WithSkipped{A,C}");
}

#[test]
fn an_enum_lists_variants_in_declaration_order() {
    assert_eq!(
        Message::shape(),
        "Message{Ping,Point(i32,i32),Move{x:i32,y:i32}}"
    );
}

#[test]
fn a_rename_does_not_change_the_layout() {
    // Postcard encodes no names, so `rename_all` must not mint a new id.
    assert_eq!(Renamed::shape(), "Renamed{FirstOne,SecondOne{value:u8}}");
}

#[test]
fn a_generic_renders_its_parameters_shape() {
    assert_eq!(
        Generic::<u64>::shape(),
        "Generic<u64>{value:u64,rest:Vec<u64>}"
    );
    assert_eq!(
        Generic::<Option<u8>>::shape(),
        "Generic<Option<u8>>{value:Option<u8>,rest:Vec<Option<u8>>}"
    );
}

#[test]
fn containers_compose() {
    assert_eq!(<Vec<Option<u8>>>::shape(), "Vec<Option<u8>>");
    assert_eq!(<(u8, String)>::shape(), "(u8,String)");
    assert_eq!(<Box<u8>>::shape(), "u8");
    assert_eq!(<[u16; 3]>::shape(), "[u16;3]");
    assert_eq!(<()>::shape(), "()");
}

#[test]
fn a_hand_written_shape_composes_as_a_field() {
    assert_eq!(WithManual::shape(), "WithManual{path:String}");
}
