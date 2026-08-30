//! The pairing pass: a linear scan over the lexer's token stream answering
//! exactly one question — which opener token pairs with which closer token.
//! The answer is reported as intervals over token indices and nothing else:
//! no nodes are built, no degradation chosen; both are the builder's
//! policies over these facts.
//!
//! ## Crossings
//!
//! Every opener pushes an entry; a closer claims the topmost live entry
//! carrying its key, tombstones it, and leaves everything above untouched —
//! so a construct opened inside another may be closed outside it and the
//! two intervals cross (`[[div]] a [[span]] b [[/div]] c [[/span]]` →
//! `div(0,8)`, `span(4,12)`). Crossings are normal output here; cutting the
//! dominated construct at the dominator's edge is the builder's job.
//!
//! ## Line intervals
//!
//! Not every construct closes with a bracket token. Headings, centered
//! lines, list items and table cells are line-scoped: their closer is the
//! line's newline. Colours and sups reach further but not unboundedly:
//! Wikidot tokenizes a single newline so its rules match across it, while a
//! blank line survives as a real paragraph break — so a `##color|` span or
//! `^^sup^^` crosses single newlines and dies (its opener degrading to raw
//! text) at a blank line or the end of input. Quote levels live even
//! longer: a level stays open across consecutive quoted lines and closes
//! just past the newline of the last line shallower than itself (the newline
//! belongs to the quote's region; the closer itself never disappears). The
//! `eats` flag says whether the closer token itself disappears from the
//! stream: a `##`, a `||` and the newline a heading swallows do; the
//! newline that merely ends a colour stays a plain newline leaf.
//!
//! ## Verbatim checkpoints
//!
//! `[[code]]`, `[[module CSS]]` and `[!--` open verbatim regions: pairing
//! inside them is speculative. A successful closer rolls the world back to
//! the checkpoint — events emitted since die, the stack above is dropped,
//! and entries *below* it that were claimed inside the region revive (their
//! closers turn out to be raw text) — then the region is emitted as one
//! verbatim span. A closer that never comes means no rollback: the
//! speculative pairs were real.

use super::lexer::{OpenTag, Tok, Token};
use super::*;

/// The pairing key an open interval answers to: what closer tokens claim it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Key {
    Tag(ClosedTag),
    Mark(TextStyle),
    /// Any `##` form.
    Color,
    /// `^^` / `,,`.
    Sup(bool),
    Comment,
    /// Heading, centered line or list item: closed by the line's newline.
    Line,
    /// `||`: closed by the next `||` or the line's newline.
    Cell,
    /// One `>` level: closed by a shallower or non-quoted line.
    Quote(usize),
}

/// A claim no rollback can revive.
const ROOT: u32 = u32::MAX;

/// A verbatim region's rollback point: the world as it was when the region
/// opened.
#[derive(Clone, Copy)]
struct Checkpoint {
    serial: u32,
    stack: usize,
    out: usize,
    depth: usize,
}

/// One interval on the stack, live or tombstoned.
struct Entry {
    open: usize,
    key: Key,
    /// `None` while open; otherwise the region whose rollback revives it.
    dead: Option<u32>,
    /// Set for the verbatim regions.
    verbatim: Option<Checkpoint>,
}

/// One pairing fact: an interval over token indices. `close` may equal the
/// token count — the file's end acting as a virtual closer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Pair {
        open: usize,
        close: usize,
        /// Whether the closer token leaves the stream (a `**` does; the
        /// newline ending a colour stays a leaf).
        eats: bool,
    },
    Verbatim {
        open: usize,
        close: usize,
    },
}

impl Event {
    pub(crate) fn open(&self) -> usize {
        match self {
            Event::Pair { open, .. } | Event::Verbatim { open, .. } => *open,
        }
    }
}

/// The pairer's whole output.
pub(crate) struct Pairing {
    /// Every interval, sorted by opener index.
    pub(crate) events: Vec<Event>,
    /// Token index → the event it opens.
    pub(crate) open_of: Vec<Option<usize>>,
    /// Token index → the event whose closer token it is (only eaten closers).
    pub(crate) close_of: Vec<Option<usize>>,
    /// Bracket openers still live at EOF, outermost first.
    pub(crate) unclosed: Vec<usize>,
}

pub(crate) fn pair(src: &str, toks: &[Token]) -> Pairing {
    let n = toks.len();
    let mut p = Pairer {
        toks,
        out: Vec::new(),
        stack: Vec::new(),
        serial: 0,
        line_start: true,
        marks: Vec::new(),
        depth: 0,
        last_nl: None,
        unclosed: Vec::new(),
    };
    for (i, t) in toks.iter().enumerate() {
        p.token(src, i, t, n);
    }
    p.eof(n);
    let mut events = p.out;
    events.sort_by_key(Event::open);
    let mut open_of = vec![None; n];
    let mut close_of = vec![None; n];
    for (ei, e) in events.iter().enumerate() {
        match *e {
            Event::Pair { open, close, eats } => {
                open_of[open] = Some(ei);
                if eats && close < n {
                    close_of[close] = Some(ei);
                }
            }
            Event::Verbatim { open, close } => {
                open_of[open] = Some(ei);
                close_of[close] = Some(ei);
            }
        }
    }
    Pairing {
        events,
        open_of,
        close_of,
        unclosed: p.unclosed,
    }
}

struct Pairer<'a> {
    toks: &'a [Token<'a>],
    out: Vec<Event>,
    stack: Vec<Entry>,
    serial: u32,
    line_start: bool,
    /// `>` markers seen so far in the current line.
    marks: Vec<usize>,
    /// The currently open quote depth.
    depth: usize,
    /// The newline token ending the last processed line.
    last_nl: Option<usize>,
    unclosed: Vec<usize>,
}

impl Pairer<'_> {
    fn token(&mut self, src: &str, i: usize, t: &Token, n: usize) {
        match &t.tok {
            Tok::Newline => self.newline(i, n),
            Tok::QuoteMark if self.line_start => self.marks.push(i),
            _ => {
                if self.line_start {
                    self.commit_line(n);
                    self.line_start = false;
                }
                self.step(src, i, t);
            }
        }
    }

    fn newline(&mut self, i: usize, n: usize) {
        if self.line_start {
            self.commit_line(n);
            self.line_start = false;
        }
        self.close_line_scoped(i);
        if self.blank_ahead(i) {
            self.kill_inline();
        }
        self.last_nl = Some(i);
        self.line_start = true;
    }

    /// A real paragraph break follows this newline: the next line holds
    /// nothing but whitespace.
    fn blank_ahead(&self, i: usize) -> bool {
        self.toks[i + 1..]
            .iter()
            .find(|t| {
                !matches!(&t.tok, Tok::Text(s) if s.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\r')))
            })
            .is_some_and(|t| matches!(t.tok, Tok::Newline))
    }

    /// Colours and sups die where Wikidot's rules cannot follow: a blank
    /// line or the end of input. No closer will ever come, so the openers
    /// degrade to raw leaves (tombstoned entries survive — a verbatim
    /// rollback may still revive them).
    fn kill_inline(&mut self) {
        self.stack
            .retain(|e| !matches!(&e.key, Key::Color | Key::Sup(_)) || e.dead.is_some());
    }

    /// Close every live line-scoped interval at `close`, whatever sits
    /// above it on the stack: headings and cells die at a newline, while
    /// colours, sups, spans, marks, quotes and verbatim regions stay open
    /// across it — colours and sups until the next blank line, the rest for
    /// the builder to cut.
    fn close_line_scoped(&mut self, close: usize) {
        let mut due = Vec::new();
        let mut keep = Vec::new();
        let stack = std::mem::take(&mut self.stack);
        for e in stack {
            match &e.key {
                Key::Line | Key::Cell if e.dead.is_none() => due.push(Event::Pair {
                    open: e.open,
                    close,
                    eats: true,
                }),
                _ => keep.push(e),
            }
        }
        self.stack = keep;
        self.out.append(&mut due);
    }

    /// The quote-level state of a line settles at its first non-marker
    /// token (or its newline): shallower levels close at the previous
    /// line's newline, deeper ones open on this line's markers.
    fn commit_line(&mut self, n: usize) {
        let marks = std::mem::take(&mut self.marks);
        let nd = marks.len();
        while self.depth > nd {
            self.depth -= 1;
            // The quote region runs through the newline of its last line:
            // the closer sits on the next line's first token (a token the
            // builder re-renders, since quotes never eat it).
            let close = if self.line_start {
                self.last_nl.map(|nl| nl + 1)
            } else {
                Some(n)
            }
            .unwrap_or(n);
            let j = self
                .stack
                .iter()
                .rposition(|e| e.dead.is_none() && matches!(e.key, Key::Quote(_)))
                .expect("quote depth without a frame");
            let open = self.stack.remove(j).open;
            self.out.push(Event::Pair {
                open,
                close,
                eats: false,
            });
        }
        for level in self.depth + 1..=nd {
            self.stack.push(Entry {
                open: marks[level - 1],
                key: Key::Quote(level),
                dead: None,
                verbatim: None,
            });
        }
        self.depth = nd;
    }

    fn step(&mut self, src: &str, i: usize, t: &Token) {
        match &t.tok {
            Tok::Heading(_) | Tok::CenterEq | Tok::ListMark { .. } => self.push(i, Key::Line),
            Tok::Pipe2 => {
                self.close_line_scoped(i);
                self.push(i, Key::Cell);
            }
            Tok::ColorClose => self.close_keyed(i, Key::Color, true),
            // A `##spec|` in closer position closes the open color and
            // leaves its `spec|` behind as an unparsed leaf.
            Tok::ColorOpen(_) if self.top_is(Key::Color) => self.close_keyed(i, Key::Color, false),
            Tok::ColorOpen(_) => self.push(i, Key::Color),
            Tok::CommentClose => self.close_keyed(i, Key::Comment, true),
            Tok::SupMark => match self.live_sup(true) {
                Some(()) => self.close_keyed(i, Key::Sup(true), true),
                None => self.push(i, Key::Sup(true)),
            },
            Tok::SubMark => match self.live_sup(false) {
                Some(()) => self.close_keyed(i, Key::Sup(false), true),
                None => self.push(i, Key::Sup(false)),
            },
            Tok::Mark(style) if self.live_mark(*style).is_some() => {
                self.close_keyed(i, Key::Mark(*style), true)
            }
            // An opener mark followed by a space opens nothing (`-- ` is an
            // em-dash, `** ` plain text); a closer ignores the space.
            Tok::Mark(style) if !src[t.end..].starts_with(' ') => self.push(i, Key::Mark(*style)),
            Tok::CommentOpen => self.push_verbatim(i, Key::Comment),
            Tok::Open(open) => match opener_key(open) {
                // `[[module css]]` is line-anchored on Wikidot: its `^`-anchored
                // rule runs before the blockquote rule strips `> `, so a quoted
                // CSS region is documentation code, not a live stylesheet.
                Some((key, true))
                    if !matches!(open, OpenTag::Css)
                        || t.start == 0
                        || src.as_bytes()[t.start - 1] == b'\n' =>
                {
                    self.push_verbatim(i, key)
                }
                Some((_, true)) => {}
                Some((key, _)) => self.push(i, key),
                None => {}
            },
            Tok::Close(tag) => self.close_keyed(i, Key::Tag(tag.clone()), true),
            _ => {}
        }
    }

    fn live_mark(&self, style: TextStyle) -> Option<()> {
        self.stack
            .iter()
            .any(|e| e.dead.is_none() && e.key == Key::Mark(style))
            .then_some(())
    }

    fn live_sup(&self, sup: bool) -> Option<()> {
        self.stack
            .iter()
            .any(|e| e.dead.is_none() && e.key == Key::Sup(sup))
            .then_some(())
    }

    fn top_is(&self, key: Key) -> bool {
        matches!(self.stack.last(), Some(e) if e.dead.is_none() && e.key == key)
    }

    fn push(&mut self, i: usize, key: Key) {
        self.stack.push(Entry {
            open: i,
            key,
            dead: None,
            verbatim: None,
        });
    }

    fn push_verbatim(&mut self, i: usize, key: Key) {
        self.stack.push(Entry {
            open: i,
            key,
            dead: None,
            verbatim: Some(Checkpoint {
                serial: self.serial,
                stack: self.stack.len(),
                out: self.out.len(),
                depth: self.depth,
            }),
        });
        self.serial += 1;
    }

    /// Claim `key`'s topmost live entry. A plain interval is tombstoned and
    /// its pair emitted; a verbatim region rolls the world back to its
    /// checkpoint and is emitted verbatim. A key with no live entry is a
    /// stray: the token passes through untouched.
    fn close_keyed(&mut self, i: usize, key: Key, eats: bool) {
        let Some(j) = self
            .stack
            .iter()
            .rposition(|e| e.dead.is_none() && e.key == key)
        else {
            return;
        };
        let open = self.stack[j].open;
        match self.stack[j].verbatim {
            Some(cp) => {
                self.stack.truncate(cp.stack);
                for e in self.stack.iter_mut() {
                    if e.dead == Some(cp.serial) {
                        e.dead = None;
                    }
                }
                self.out.truncate(cp.out);
                self.depth = cp.depth;
                self.out.push(Event::Verbatim { open, close: i });
            }
            None => {
                let region = self
                    .stack
                    .iter()
                    .rev()
                    .filter(|e| e.dead.is_none())
                    .find_map(|e| e.verbatim.map(|cp| cp.serial))
                    .unwrap_or(ROOT);
                self.stack[j].dead = Some(region);
                self.out.push(Event::Pair {
                    open,
                    close: i,
                    eats,
                });
            }
        }
    }

    fn eof(&mut self, n: usize) {
        self.commit_line(n);
        self.close_line_scoped(n);
        self.kill_inline();
        self.unclosed = self
            .stack
            .iter()
            .filter(|e| e.dead.is_none())
            .map(|e| e.open)
            .collect();
    }
}

/// The pairing key of a bracket opener; `None` for constructs that are their
/// own leaf (`[[include]]`, `[[image]]`, `[[user]]`, single modules,
/// listpages slots).
fn opener_key(open: &OpenTag) -> Option<(Key, bool)> {
    let (tag, verbatim) = match open {
        OpenTag::Div { .. } => (ClosedTag::Div, false),
        OpenTag::Span { .. } => (ClosedTag::Span, false),
        OpenTag::Anchor { .. } => (ClosedTag::Anchor, false),
        OpenTag::Footnote => (ClosedTag::Footnote, false),
        OpenTag::Table { .. } => (ClosedTag::Table, false),
        OpenTag::Row { .. } => (ClosedTag::Row, false),
        OpenTag::Cell { .. } => (ClosedTag::Cell, false),
        OpenTag::Collapsible { .. } => (ClosedTag::Collapsible, false),
        OpenTag::Size(_) => (ClosedTag::Size, false),
        OpenTag::IfTags(_) => (ClosedTag::IfTags, false),
        OpenTag::Align { floating, side } => (
            ClosedTag::Align {
                floating: *floating,
                side: *side,
            },
            false,
        ),
        OpenTag::Tab { .. } => (ClosedTag::Tab, false),
        OpenTag::Tabview => (ClosedTag::Tabview, false),
        // `[[/module]]` claims whichever module construct is topmost: a body
        // module, a ListPages, or the CSS region.
        OpenTag::ModuleBlock { .. } | OpenTag::ListPages { .. } => (ClosedTag::Module, false),
        OpenTag::Code { .. } => (ClosedTag::Code, true),
        OpenTag::Css => (ClosedTag::Module, true),
        _ => return None,
    };
    Some((Key::Tag(tag), verbatim))
}

#[cfg(test)]
mod tests {
    use super::super::lexer::lex;
    use super::*;
    use std::path::Path;

    fn pair_str(src: &str) -> Pairing {
        let toks = lex(src);
        pair(src, &toks)
    }

    fn p(open: usize, close: usize) -> Event {
        Event::Pair {
            open,
            close,
            eats: true,
        }
    }

    /// A line-scoped close that leaves its newline in the stream.
    fn pn(open: usize, close: usize) -> Event {
        Event::Pair {
            open,
            close,
            eats: false,
        }
    }

    fn v(open: usize, close: usize) -> Event {
        Event::Verbatim { open, close }
    }

    /// `[[/code]]`'s rollback kills the phantom div pair and revives the
    /// div, so the outer `[[/div]]` pairs it — spanning the code region.
    #[test]
    fn rollback_discards_phantom_pairs() {
        let pr = pair_str("[[div]] [[code]] [[/div]] [[/code]] [[/div]]");
        assert_eq!(pr.events, vec![p(0, 8), v(2, 6)]);
        assert!(pr.unclosed.is_empty());
    }

    /// EOF is not a failure: pairs formed inside an unclosed code region
    /// stay real, the region itself stays open.
    #[test]
    fn unclosed_region_keeps_its_pairs() {
        let pr = pair_str("[[div]] [[code]] [[/div]] tail");
        assert_eq!(pr.events, vec![p(0, 4)]);
        assert_eq!(pr.unclosed, vec![2]);
    }

    /// A span claimed inside a code region revives on rollback and is
    /// re-paired wider by its later closer.
    #[test]
    fn rollback_revives_owner_below_region() {
        let pr = pair_str("[[span]] [[code]] [[/span]] x [[/code]] [[/span]]");
        assert_eq!(pr.events, vec![p(0, 8), v(2, 6)]);
        assert!(pr.unclosed.is_empty());
    }

    #[test]
    fn crossed_close() {
        let pr = pair_str("[[div]]\nhi1\n[[span]]\nhi2\n[[/div]]\nhi3\n[[/span]]\n");
        assert_eq!(pr.events, vec![p(0, 8), p(4, 12)]);
        assert!(pr.unclosed.is_empty());
    }

    #[test]
    fn crossed_open() {
        let pr = pair_str("[[span]] h1 [[div]] h2 [[/span]] h3 [[/div]]");
        assert_eq!(pr.events, vec![p(0, 4), p(2, 6)]);
        assert!(pr.unclosed.is_empty());
    }

    #[test]
    fn nested_code_outer_wins() {
        let pr = pair_str("[[code]] a [[code]] b [[/code]] c [[/code]]");
        assert_eq!(pr.events, vec![v(0, 6)]);
    }

    #[test]
    fn css_region_beats_inner_module_pair() {
        let pr = pair_str("[[module CSS]] x [[module CountPages]] y [[/module]] z [[/module]]");
        assert_eq!(pr.events, vec![v(0, 6)]);
    }

    #[test]
    fn listpages_pairs_with_module_closer() {
        let pr = pair_str("[[module ListPages]]\n[[/module]]");
        assert_eq!(pr.events, vec![p(0, 2)]);
        assert!(pr.unclosed.is_empty());
    }

    #[test]
    fn comment_is_verbatim() {
        let pr = pair_str("a [!-- [[div]] --] b");
        assert_eq!(pr.events, vec![v(1, 5)]);
    }

    #[test]
    fn stray_closers_pair_nothing() {
        let pr = pair_str("[[/div]] ## [[/collapsible]] x");
        assert_eq!(pr.events, vec![]);
        assert!(pr.unclosed.is_empty());
    }

    #[test]
    fn marks_pair_greedily() {
        let pr = pair_str("**b**b**");
        assert_eq!(pr.events, vec![p(0, 2)]);
        assert_eq!(pr.unclosed, vec![4]);
    }

    #[test]
    fn spaced_mark_opens_nothing() {
        assert_eq!(pair_str("a -- b").events, vec![]);
        let pr = pair_str("-- a --");
        assert_eq!(pr.events, vec![]);
        assert_eq!(pr.unclosed, vec![2]);
    }

    #[test]
    fn spaced_mark_still_closes() {
        assert_eq!(pair_str("--a-- tail").events, vec![p(0, 2)]);
    }

    #[test]
    fn mark_pairs_across_newline() {
        assert_eq!(pair_str("**a\nb**").events, vec![p(0, 4)]);
    }

    /// A colour without a `##` closes at its line's end — and the newline
    /// stays in the stream.
    #[test]
    fn color_dies_on_newline() {
        assert_eq!(pair_str("##r| a\nb##").events, vec![p(0, 4)]);
        assert_eq!(pair_str("##r| a\n\nb##").events, vec![]);
    }

    #[test]
    fn color_open_in_closer_position() {
        assert_eq!(pair_str("##red| x ##").events, vec![p(0, 2)]);
        let pr = pair_str("##red| ##green| x ##");
        assert_eq!(pr.events, vec![pn(0, 2)]);
        assert!(pr.unclosed.is_empty());
    }

    /// A heading, a centered line and a list item each swallow their
    /// newline: the interval's close is the newline token itself.
    #[test]
    fn line_constructs_eat_their_newline() {
        assert_eq!(pair_str("+ h\ntail").events, vec![p(0, 2)]);
        assert_eq!(pair_str("= c\ntail").events, vec![p(0, 2)]);
        assert_eq!(pair_str("* i\ntail").events, vec![p(0, 2)]);
        assert_eq!(pair_str("* i").events, vec![p(0, 2)]);
    }

    /// `||` closes the cell before it and opens the next; the trailing
    /// `||`'s empty cell dies at the newline.
    #[test]
    fn table_cells_pair_across_pipes() {
        assert_eq!(
            pair_str("|| a || b ||\ntail").events,
            vec![p(0, 2), p(2, 4), p(4, 5)]
        );
    }

    /// One quote level spans consecutive quoted lines; a deeper line nests;
    /// a plain line closes everything at the previous newline.
    #[test]
    fn quote_levels_nest_and_close() {
        let pr = pair_str("> a\n>> b\n> c\nd");
        assert_eq!(pr.events, vec![pn(0, 10), pn(4, 7)]);
    }

    #[test]
    fn collapsible_pairs_like_any_interval() {
        // The opener splits into opener + header leaf, so everything past
        // it shifts one token on.
        let pr = pair_str("[[div]] [[collapsible]] c [[/collapsible]] [[/div]]");
        assert_eq!(pr.events, vec![p(0, 7), p(2, 5)]);
    }

    #[test]
    fn tabs_pair_unconditionally() {
        let pr = pair_str("[[tabview]] [[tab A]] a [[/tab]] [[/tabview]]");
        assert_eq!(pr.events, vec![p(0, 6), p(2, 4)]);
    }

    #[test]
    fn align_closer_must_mirror_the_opener() {
        assert_eq!(pair_str("[[<]] x [[/<]]").events, vec![p(0, 2)]);
        let pr = pair_str("[[<]] x [[/>]]");
        assert_eq!(pr.events, vec![]);
        assert_eq!(pr.unclosed, vec![0]);
    }

    /// `^^a^^` pairs; `^^a` at EOF degrades to raw text (no closer ever
    /// comes).
    #[test]
    fn sup_pairs_greedily() {
        assert_eq!(pair_str("^^a^^").events, vec![p(0, 2)]);
        let pr = pair_str("a ^^b");
        assert_eq!(pr.events, vec![]);
        assert!(pr.unclosed.is_empty());
    }

    #[test]
    fn deep_nesting_stays_flat() {
        let src = "[[div]]".repeat(10_000);
        let pr = pair_str(&src);
        assert_eq!(pr.unclosed.len(), 10_000);
        assert!(pr.events.is_empty());
    }

    /// Lexes and pairs every page revision of the local export repo and
    /// checks the pairing invariants on real-world input. Run explicitly:
    /// `cargo test -p kolorinko --release -- --ignored`.
    #[test]
    #[ignore = "walks the full local export repo"]
    fn archive_smoke() {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kolorinko/repo/rpcauthority/pages");
        if !root.exists() {
            eprintln!("skipping: real repo not present");
            return;
        }
        let bounds = |e: &Event| match e {
            Event::Pair { open, close, .. } | Event::Verbatim { open, close } => (*open, *close),
        };
        let mut stats = Stats::default();
        visit(&root, &mut |src| {
            stats.files += 1;
            let toks = lex(src);
            stats.tokens += toks.len();
            let pr = pair(src, &toks);
            stats.unclosed += pr.unclosed.len();
            for e in &pr.events {
                let (o, c) = bounds(e);
                assert!(o < c, "empty interval in {src:?}");
            }
            let events: Vec<_> = pr.events.iter().map(bounds).collect();
            let mut last = None;
            for &(o, _) in &events {
                assert!(last.is_none_or(|l| l < o), "events not sorted in {src:?}");
                last = Some(o);
            }
            let mut last = None;
            for &o in &pr.unclosed {
                assert!(o < toks.len(), "unclosed out of range in {src:?}");
                assert!(last.is_none_or(|l| l < o), "unclosed not sorted in {src:?}");
                last = Some(o);
            }
            stats.events += events.len();
            if events.len() <= 2000 {
                for j in 1..events.len() {
                    for i in 0..j {
                        stats.crossings += usize::from(events[j].1 < events[i].1);
                    }
                }
            }
        });
        eprintln!(
            "files={} tokens={} events={} unclosed={} crossings={}",
            stats.files, stats.tokens, stats.events, stats.unclosed, stats.crossings
        );
    }

    #[derive(Default)]
    struct Stats {
        files: usize,
        tokens: usize,
        events: usize,
        unclosed: usize,
        crossings: usize,
    }

    fn visit(dir: &Path, f: &mut impl FnMut(&str)) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, f);
            } else if path.extension().is_some_and(|e| e == "txt") {
                if let Ok(src) = std::fs::read_to_string(&path) {
                    f(&src);
                }
            }
        }
    }
}
