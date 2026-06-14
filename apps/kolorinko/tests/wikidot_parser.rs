use kolorinko::wikidot_parser;

#[test]
fn main() {
    let parsed = wikidot_parser::parse(include_str!("syntax.txt"));
    println!("{parsed:?}");
    // assert!(false);
}
