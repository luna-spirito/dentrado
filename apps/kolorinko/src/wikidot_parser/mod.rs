use chumsky::Parser;

use crate::wikidot_parser::types::Content;

mod types;

enum Tag {
    // List all the tags
}

enum ContentExitReason {
    EOF,
    EndOfTag(Tag),
}

fn content<'a>() -> impl Parser<'a, &'a [u8], (Content, ContentExitReason)> {
    todo!()
}
