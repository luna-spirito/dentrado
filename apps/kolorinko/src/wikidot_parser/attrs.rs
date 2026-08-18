//! Attribute parsing and `TextObj` run collection.

use super::*;

/// Attribute-context whitespace: real pages carry copy-pasted non-breaking
/// spaces inside module headers (`[[module ListPages\u{a0}category=…]]`).
fn is_param_ws(c: char) -> bool {
    c == ' ' || c == '\u{a0}'
}

/// Parse `key="value"` / `key=value` attributes until `]` or newline. Values
/// may contain `%%vars%%` and `{$vars$}`.
pub(crate) fn params_block<'a>()
-> impl Parser<'a, In<'a>, HashMap<String, Vec<TextObj>>, E<'a>> + Clone + 'a {
    custom(|inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| {
        let mut map: HashMap<String, Vec<TextObj>> = HashMap::new();
        loop {
            while matches!(inp.peek(), Some(c) if is_param_ws(c)) {
                inp.next();
            }
            let full = inp.full_slice();
            let off = *inp.cursor().inner();
            let rest = &full[off..];
            if rest.is_empty() || rest.starts_with(']') || rest.starts_with('\n') {
                break;
            }
            let key_start = *inp.cursor().inner();
            while matches!(inp.peek(), Some(c) if is_prop_char(c)) {
                inp.next();
            }
            let key_end = *inp.cursor().inner();
            if key_end == key_start {
                break;
            }
            let key = full[key_start..key_end].to_ascii_lowercase();
            if !matches!(inp.next(), Some('=')) {
                break;
            }
            let value = if matches!(inp.peek(), Some('"')) {
                inp.next();
                let v = collect_text_objs(inp, &[], &['"']);
                if matches!(inp.peek(), Some('"')) {
                    inp.next();
                }
                v
            } else {
                collect_text_objs(inp, &[], &[' ', '\u{a0}', ']'])
            };
            map.insert(key, value);
        }
        Ok(map)
    })
}

/// A run of [`TextObj`]s — plain text chunks interleaved with `%%var%%` and
/// `{$var$}` substitutions — up to any of `delims`, a newline, or EOF.
pub(crate) fn text_objs<'a>(
    delims: &'static [&'static str],
) -> impl Parser<'a, In<'a>, Vec<TextObj>, E<'a>> + Clone + 'a {
    custom(move |inp: &mut InputRef<'a, '_, In<'a>, E<'a>>| Ok(collect_text_objs(inp, delims, &[])))
}

/// Imperative core shared by [`params_block`] and [`text_objs`].
///
/// Accumulates plain text into a buffer, flushing it as [`TextObj::Plain`]
/// whenever a `%%var%%` or `{$var$}` substitution is encountered, and stops at
/// any of: a multi-char `delim`, a `single_stop` char, a newline, or EOF.
pub(crate) fn collect_text_objs<'a, 'b>(
    inp: &mut InputRef<'a, 'b, In<'a>, E<'a>>,
    delims: &[&str],
    single_stops: &[char],
) -> Vec<TextObj> {
    let mut result: Vec<TextObj> = Vec::new();
    let mut buf = String::new();
    let flush = |buf: &mut String, result: &mut Vec<TextObj>| {
        if !buf.is_empty() {
            result.push(TextObj::Plain(std::mem::take(buf)));
        }
    };
    loop {
        let full = inp.full_slice();
        let off = *inp.cursor().inner();
        let rest = &full[off..];
        if rest.is_empty() || rest.starts_with('\n') || delims.iter().any(|d| rest.starts_with(d)) {
            break;
        }
        if let Some(c) = rest.chars().next() {
            if single_stops.contains(&c) {
                break;
            }
        }
        // %%name|default%%
        if rest.starts_with("%%") {
            flush(&mut buf, &mut result);
            inp.next();
            inp.next();
            let (name, default) = read_named_var(inp, "%%");
            result.push(TextObj::ModuleVar { name, default });
            continue;
        }
        // {$name//default}
        if rest.starts_with("{$") {
            flush(&mut buf, &mut result);
            inp.next();
            inp.next();
            let (name, default) = read_include_var(inp);
            result.push(TextObj::IncludeVar { name, default });
            continue;
        }
        if let Some(c) = inp.next() {
            buf.push(c);
        }
    }
    flush(&mut buf, &mut result);
    result
}

/// Read `name` (prop chars) then, if `closer` follows optionally after
/// `|default`, consume through `closer`. Returns `(name, default)`.
pub(crate) fn read_named_var<'a, 'b>(
    inp: &mut InputRef<'a, 'b, In<'a>, E<'a>>,
    closer: &str,
) -> (String, Option<String>) {
    let full = inp.full_slice();
    let name_start = *inp.cursor().inner();
    while matches!(inp.peek(), Some(c) if is_prop_char(c)) {
        inp.next();
    }
    let name_end = *inp.cursor().inner();
    let name = full[name_start..name_end].to_string();
    let default = if matches!(inp.peek(), Some('|')) {
        inp.next();
        let d_start = *inp.cursor().inner();
        loop {
            let f = inp.full_slice();
            let o = *inp.cursor().inner();
            if f[o..].starts_with(closer) {
                break;
            }
            if inp.next().is_none() {
                break;
            }
        }
        let d_end = *inp.cursor().inner();
        let d = full[d_start..d_end].to_string();
        consume_prefix(inp, closer);
        Some(d)
    } else {
        consume_prefix(inp, closer);
        None
    };
    (name, default)
}

/// Read `{$name//default}`'s tail (after `{$`): name, optional `//default`,
/// then `}`.
pub(crate) fn read_include_var<'a, 'b>(
    inp: &mut InputRef<'a, 'b, In<'a>, E<'a>>,
) -> (String, Option<Content>) {
    let full = inp.full_slice();
    let name_start = *inp.cursor().inner();
    while matches!(inp.peek(), Some(c) if is_prop_char(c)) {
        inp.next();
    }
    let name_end = *inp.cursor().inner();
    let name = full[name_start..name_end].to_string();
    let default = if {
        let f = inp.full_slice();
        let o = *inp.cursor().inner();
        f[o..].starts_with("//")
    } {
        inp.next();
        inp.next();
        let d_start = *inp.cursor().inner();
        loop {
            let f = inp.full_slice();
            let o = *inp.cursor().inner();
            if f[o..].starts_with('}') {
                break;
            }
            if inp.next().is_none() {
                break;
            }
        }
        let d_end = *inp.cursor().inner();
        let d = full[d_start..d_end].to_string();
        if matches!(inp.peek(), Some('}')) {
            inp.next();
        }
        Some(parse(&d))
    } else {
        if matches!(inp.peek(), Some('}')) {
            inp.next();
        }
        None
    };
    (name, default)
}

/// Consume `prefix` from the input if it's next.
pub(crate) fn consume_prefix<'a, 'b>(inp: &mut InputRef<'a, 'b, In<'a>, E<'a>>, prefix: &str) {
    let f = inp.full_slice();
    let o = *inp.cursor().inner();
    if f[o..].starts_with(prefix) {
        for _ in 0..prefix.chars().count() {
            inp.next();
        }
    }
}
