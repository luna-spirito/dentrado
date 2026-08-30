//! Pure string helpers: link / page-ref / tag-filter parsing, colour
//! normalisation, include-argument splitting, listpages parameter
//! interpretation, and the post-processing pass that fuses adjacent text
//! fragments.

use super::*;

/// `[*target …]` / `[[[*target|…]]]` — Wikidot's asterisk prefix opens the
/// link in a new tab; it is stripped from the target (`toUnixName` would
/// discard it anyway).
pub(crate) fn new_tab_mark(target: &str) -> (&str, bool) {
    (
        target.strip_prefix('*').unwrap_or(target),
        target.starts_with('*'),
    )
}

/// Turn a raw link target string into a [`LinkTarget`]: external URL if it
/// starts with `http://`/`https://` (or is a same-page `#fragment`),
/// otherwise an internal wiki page reference.
pub fn parse_link_target(raw: &str) -> LinkTarget {
    let trimmed = raw.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") || trimmed.starts_with('#')
    {
        return LinkTarget::Url(trimmed.to_string());
    }
    LinkTarget::Page(parse_page_ref(trimmed))
}

/// Turn an attribute-carried link target — any target the lexer hands over
/// as text that may hold `{$var}`/`%%var%%` slots (the `href` value of
/// `[[a …]]`, the target of `[[[…]]]` / `[…]`) — into a [`LinkTarget`]: a
/// fully literal value classifies through [`parse_link_target`], one with
/// variable slots stays [`LinkTarget::Unresolved`] until substitution (or
/// the render fallback) flattens it.
pub fn parse_link_target_objs(objs: &[TextObj]) -> LinkTarget {
    TextObj::plain_concat(objs).map_or_else(
        || LinkTarget::Unresolved(objs.to_vec()),
        |s| parse_link_target(&s),
    )
}

/// Split a raw link-target string into [`TextObj`]s, recognizing `%%var%%`
/// / `{$var}` slots; a slot-free target stays a single [`TextObj::Plain`].
pub(crate) fn text_objs_of(raw: &str) -> Vec<TextObj> {
    lexer::collect_text_objs(raw.as_bytes(), 0, &[]).1
}

/// Parse a `[[include]]` source or internal link path into a [`PageRef`].
///
/// A leading `space:` segment is a cross-space reference; the rest is the path.
pub(crate) fn parse_page_ref(raw: &str) -> PageRef {
    let raw = raw.trim().trim_start_matches('/');
    let lower = raw.to_ascii_lowercase().replace(' ', "-");
    let parts: Vec<&str> = lower.split(':').collect();
    match parts.as_slice() {
        [] | [""] => PageRef {
            space: None,
            path: Vec::new(),
        },
        [single] => PageRef {
            space: None,
            path: vec![(*single).to_string()],
        },
        [space, rest @ ..] => PageRef {
            space: Some((*space).to_string()),
            path: rest.iter().map(|s| (*s).to_string()).collect(),
        },
    }
}

/// Parse a `[[iftags …]]` argument string into `(has_all, has_none)` per
/// PureScript `objFiltr` (plain tags and `+tag` both required, `-tag` excluded;
/// the OR-distinction between plain tags is intentionally collapsed).
pub(crate) fn parse_tag_filter(raw: &str) -> (Vec<String>, Vec<String>) {
    let mut has_all = Vec::new();
    let mut has_none = Vec::new();
    for token in raw.split([',', ' ']) {
        let tok = token.trim();
        if tok.is_empty() {
            continue;
        }
        match tok.chars().next() {
            Some('+') => has_all.push(tok[1..].to_string()),
            Some('-') => has_none.push(tok[1..].to_string()),
            _ => has_all.push(tok.to_string()),
        }
    }
    (has_all, has_none)
}

fn is_hex_char(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// Normalize a `##color|` argument: prefix with `#` if it's a bare hex triplet
/// of a valid length (3/4/6/8 digits).
pub(crate) fn normalize_color(c: String) -> String {
    if [3, 4, 6, 8].contains(&c.len()) && c.chars().all(is_hex_char) {
        format!("#{c}")
    } else {
        c
    }
}

// ── Include arguments ─────────────────────────────────────────────────────

/// Split the body of a `[[include ...]]` into the source page reference and
/// its variable substitution map, parsing each value as real wikitext
/// markup ([`Content`]) — the parser's `Node::Include` form, where `{$x}`
/// becomes a [`TextObj::IncludeVar`] node and `[[image ...]]` an
/// [`Node::Image`].
///
/// Two assignment syntaxes are recognised, distinguished by a depth-0 `|`:
/// • pipe-separated — `source | k1=v1 | k2=v2` (a value runs to the next
///   depth-0 `|`, so it may contain spaces and balanced `[[...]]` markup).
/// • space-separated — `source k1="v1" k2=v2` (quoted values, or bare values
///   running to the next depth-0 whitespace).
///
/// A later assignment to the same key is kept alongside the earlier one
/// (in source order); the first non-empty value wins at substitution, which is
/// what makes the `key={$key}|key=default` fallback idiom work. Only ASCII
/// bytes act as delimiters and bracket pairs are scanned non-overlapping, so
/// every slice lands on a UTF-8 character boundary and `[[[...]]]` stays
/// depth-balanced (one `[[`/`]]` pair plus a literal `[`/`]`).
pub(crate) fn parse_include_args(raw: &str) -> (PageRef, Vec<(String, Content)>) {
    split_include_args_with(raw, builder::parse_sub)
}

/// The same split with every value kept as literal text (quotes unwrapped,
/// whitespace trimmed) — the form the textual include assembly works with.
pub(crate) fn split_include_args(raw: &str) -> (PageRef, Vec<(String, String)>) {
    split_include_args_with(raw, |v| v.to_string())
}

fn split_include_args_with<V>(raw: &str, map: impl Fn(&str) -> V) -> (PageRef, Vec<(String, V)>) {
    let b = raw.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i < n && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let src_start = i;
    while i < n && !b[i].is_ascii_whitespace() && b[i] != b'|' {
        i += 1;
    }
    let source = parse_page_ref(&raw[src_start..i]);
    let remainder = &raw[i..];
    let vars = if has_depth0_pipe(remainder) {
        parse_pipe_vars(remainder, &map)
    } else {
        parse_space_vars(remainder, &map)
    };
    (source, vars)
}

/// Strip one layer of surrounding double quotes, if present.
pub(crate) fn unquote(value: &str) -> &str {
    let t = value.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Record a `key=value` segment: split on the first `=`, parse the value as
/// wikitext markup. Quoted values are unwrapped first.
/// Record a `key=value` segment: split on the first `=`, map the value
/// through `value`. Quoted values are unwrapped first.
fn insert_kv<V>(seg: &str, vars: &mut Vec<(String, V)>, value: &impl Fn(&str) -> V) {
    let Some(eq) = seg.find('=') else {
        return;
    };
    let key = seg[..eq].trim();
    if key.is_empty() {
        return;
    }
    vars.push((key.to_string(), value(unquote(&seg[eq + 1..]))));
}

/// Track `[[`/`]]` depth (and skip over `"..."` quotes) across `s`; return
/// whether a `|` occurs at bracket depth 0 outside quotes — the marker of the
/// pipe-separated assignment syntax.
fn has_depth0_pipe(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    let mut quote = false;
    while i < b.len() {
        if quote {
            if b[i] == b'"' {
                quote = false;
            }
            i += 1;
            continue;
        }
        if i + 1 < b.len() && b[i] == b'[' && b[i + 1] == b'[' {
            depth += 1;
            i += 2;
            continue;
        }
        if i + 1 < b.len() && b[i] == b']' && b[i + 1] == b']' {
            if depth > 0 {
                depth -= 1;
            }
            i += 2;
            continue;
        }
        if b[i] == b'"' && i > 0 && b[i - 1] == b'=' {
            quote = true;
            i += 1;
            continue;
        }
        if depth == 0 && b[i] == b'|' {
            return true;
        }
        i += 1;
    }
    false
}

fn parse_pipe_vars<V>(remainder: &str, map: &impl Fn(&str) -> V) -> Vec<(String, V)> {
    let insert = |seg: &str, vars: &mut Vec<(String, V)>| insert_kv(seg, vars, map);
    let b = remainder.as_bytes();
    let mut vars = Vec::new();
    let mut seg_start = 0;
    let mut i = 0;
    let mut depth = 0i32;
    let mut quote = false;
    while i < b.len() {
        if quote {
            if b[i] == b'"' {
                quote = false;
            }
            i += 1;
            continue;
        }
        if i + 1 < b.len() && b[i] == b'[' && b[i + 1] == b'[' {
            depth += 1;
            i += 2;
            continue;
        }
        if i + 1 < b.len() && b[i] == b']' && b[i + 1] == b']' {
            if depth > 0 {
                depth -= 1;
            }
            i += 2;
            continue;
        }
        if b[i] == b'"' && i > 0 && b[i - 1] == b'=' {
            quote = true;
            i += 1;
            continue;
        }
        if depth == 0 && b[i] == b'|' {
            insert(&remainder[seg_start..i], &mut vars);
            seg_start = i + 1;
        }
        i += 1;
    }
    insert(&remainder[seg_start..], &mut vars);
    vars
}

fn parse_space_vars<V>(remainder: &str, map: &impl Fn(&str) -> V) -> Vec<(String, V)> {
    let b = remainder.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut vars: Vec<(String, V)> = Vec::new();
    while i < n {
        while i < n && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let key_start = i;
        while i < n && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let key_end = i;
        if key_start == key_end || i >= n || b[i] != b'=' {
            while i < n && !b[i].is_ascii_whitespace() {
                i += 1;
            }
            continue;
        }
        i += 1;
        let value = if i < n && b[i] == b'"' {
            i += 1;
            let v_start = i;
            while i < n && b[i] != b'"' {
                i += 1;
            }
            let v = remainder[v_start..i].to_string();
            if i < n {
                i += 1;
            }
            v
        } else {
            let v_start = i;
            let mut depth = 0i32;
            while i < n {
                if i + 1 < n && b[i] == b'[' && b[i + 1] == b'[' {
                    depth += 1;
                    i += 2;
                    continue;
                }
                if i + 1 < n && b[i] == b']' && b[i + 1] == b']' {
                    if depth > 0 {
                        depth -= 1;
                    }
                    i += 2;
                    continue;
                }
                if depth == 0 && b[i].is_ascii_whitespace() {
                    break;
                }
                i += 1;
            }
            remainder[v_start..i].to_string()
        };
        let key = remainder[key_start..key_end].trim();
        if !key.is_empty() {
            vars.push((key.to_string(), map(value.trim())));
        }
    }
    vars
}

// ── ListPages module arguments ────────────────────────────────────────────

/// `[[module ListPages …]]` argument map → selection parameters. Recognized
/// selectors are parsed; values with no data behind them in the export
/// (`rating`, `votes`, `parent`, `link_to`, …) and purely interactive
/// arguments (`rss`, `urlAttrPrefix`, …) are ignored. An `@URL|default`
/// reference keeps only its default — URL-passed arguments have no meaning in
/// a static render.
/// A `[[code …]]` opener's `type` attribute as written — `type="css"` marks
/// the block a stylesheet (Wikidot's `/code/N` endpoint compares
/// case-insensitively at serve time). `None` when absent or non-literal
/// (a `%%var%%` value can never name a type here — the endpoint serves
/// templates un-substituted too).
pub(crate) fn code_type(attrs: &HashMap<String, Vec<TextObj>>) -> Option<String> {
    TextObj::plain_concat(attrs.get("type")?)
}

pub(crate) fn listpages_params(attrs: &HashMap<String, Vec<TextObj>>) -> ListPagesParams {
    let sel = |key: &str| attr_value(attrs, key);
    ListPagesParams {
        category: sel("category").or_else(|| sel("categories")),
        tags: sel("tags")
            .or_else(|| sel("tag"))
            .map(|v| parse_tags_filter(&v)),
        created_by: sel("created_by"),
        created_at: sel("created_at")
            .or_else(|| sel("date"))
            .and_then(|v| parse_time_filter(&v)),
        updated_at: sel("updated_at").and_then(|v| parse_time_filter(&v)),
        fullname: sel("fullname").or_else(|| {
            // `range="."` selects the current page — the same page the
            // assembly later substitutes for `fullname="="`.
            sel("range").filter(|r| r == ".").map(|_| "=".to_string())
        }),
        name: sel("name"),
        pagetype: sel("pagetype"),
        order: sel("order").and_then(|v| parse_order(&v)),
        offset: sel("offset").and_then(|v| v.parse().ok()),
        limit: sel("limit").and_then(|v| v.parse().ok()),
        per_page: sel("perpage").and_then(|v| v.parse().ok()),
        separate: sel("separate").is_none_or(|v| parse_yes(&v)),
        wrapper: sel("wrapper").is_none_or(|v| parse_yes(&v)),
    }
}

/// An attribute's value flattened to a string: plain text with
/// `%%var%%`/`{$var$}` defaults spliced in (an unset variable contributes
/// nothing), and an `@URL|default` reference reduced to its default.
/// Whitespace is kept as written (`hide=" "` stays a real label).
/// Attribute names match case-insensitively on a miss of the exact key
/// (Wikidot's `perPage` and `perpage` are the same parameter).
pub(crate) fn attr_value_raw(attrs: &HashMap<String, Vec<TextObj>>, key: &str) -> Option<String> {
    let objs = attrs.get(key).or_else(|| {
        attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })?;
    let mut s = String::new();
    for o in objs {
        match o {
            TextObj::Plain(t) => s.push_str(t),
            TextObj::ModuleVar { default, .. } => {
                if let Some(d) = default {
                    s.push_str(d);
                }
            }
            TextObj::IncludeVar { .. } => {}
        }
    }
    Some(s)
}

/// [`attr_value_raw`] trimmed, with empty values — and `@URL` refs without a
/// default — treated as absent.
pub(crate) fn attr_value(attrs: &HashMap<String, Vec<TextObj>>, key: &str) -> Option<String> {
    let s = attr_value_raw(attrs, key)?.trim().to_string();
    if s.is_empty() {
        return None;
    }
    if let Some(default) = s.strip_prefix("@URL") {
        return default
            .strip_prefix('|')
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty());
    }
    Some(s)
}

/// `"yes"`/`"true"` (and anything unrecognized) → `true`; `"no"`/`"false"`/
/// `"0"` → `false`.
fn parse_yes(v: &str) -> bool {
    !matches!(v.trim().to_ascii_lowercase().as_str(), "no" | "false" | "0")
}

/// A `tags="…"` selector: `-` alone means "no tags", otherwise a space- /
/// comma-separated list of `+required`, `-excluded` and plain (additive, OR)
/// tags.
fn parse_tags_filter(v: &str) -> TagsFilter {
    if v.trim() == "-" {
        return TagsFilter {
            no_tags: true,
            ..TagsFilter::default()
        };
    }
    let mut f = TagsFilter::default();
    for token in v.split([',', ' ']).filter(|t| !t.is_empty()) {
        match token.strip_prefix('+') {
            Some(t) => f.all.push(t.to_string()),
            None => match token.strip_prefix('-') {
                Some(t) => f.none.push(t.to_string()),
                None => f.any.push(token.to_string()),
            },
        }
    }
    f
}

/// A `created_at`/`updated_at` selector: `last n unit`, `older than n unit`,
/// or an optionally prefixed (`>` `<` `>=` `<=` `=`) `yyyy[.mm[.dd]]` date
/// (all UTC).
fn parse_time_filter(v: &str) -> Option<TimeFilter> {
    let lower = v.trim().to_ascii_lowercase();
    let relative = lower
        .strip_prefix("last ")
        .or_else(|| lower.strip_prefix("older than "));
    if let Some(rest) = relative {
        let secs = relative_secs(rest)?;
        return Some(if lower.starts_with("older") {
            TimeFilter::OlderThan(secs)
        } else {
            TimeFilter::Last(secs)
        });
    }
    let v = v.trim();
    let (prefix, date) = ["<=", ">=", "<>", "<", ">", "="]
        .into_iter()
        .find_map(|p| v.strip_prefix(p).map(|d| (p, d.trim())))
        .unwrap_or(("=", v));
    let (start, end) = date_range(date)?;
    Some(match prefix {
        "=" => TimeFilter::Between(start, end),
        ">" => TimeFilter::After(end),
        ">=" => TimeFilter::After(start),
        "<" => TimeFilter::Before(start),
        "<=" => TimeFilter::Before(end),
        _ => return None,
    })
}

/// `n unit` → seconds (`n` defaults to 1; `hour`/`day`/`week`/`month`,
/// singular or plural).
fn relative_secs(rest: &str) -> Option<i64> {
    let rest = rest.trim();
    let (n, unit) = rest.split_once(' ').unwrap_or(("1", rest));
    let n: i64 = n.trim().parse().ok()?;
    let unit = unit.trim().to_ascii_lowercase();
    let unit = unit.strip_suffix('s').unwrap_or(&unit);
    let per = match unit {
        "hour" => 3600,
        "day" => 86_400,
        "week" => 604_800,
        "month" => 2_592_000,
        _ => return None,
    };
    Some(n * per)
}

/// `yyyy`, `yyyy.mm` or `yyyy.mm.dd` → the `[start, end)` Unix-timestamp range
/// it covers (UTC): the whole year, month, or day respectively.
fn date_range(v: &str) -> Option<(i64, i64)> {
    let mut parts = v.split('.');
    let y: i64 = parts.next()?.parse().ok()?;
    let m_str = parts.next();
    let d_str = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let m: i64 = m_str.map_or(Ok(1), str::parse).ok()?;
    let d: i64 = d_str.map_or(Ok(1), str::parse).ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let start = days_from_civil(y, m, d).checked_mul(86_400)?;
    let end = match (m_str, d_str) {
        (None, None) => days_from_civil(y + 1, 1, 1).checked_mul(86_400)?,
        (Some(_), None) => {
            let (ny, nm) = month_after(y, m);
            days_from_civil(ny, nm, 1).checked_mul(86_400)?
        }
        _ => start + 86_400,
    };
    Some((start, end))
}

/// The civil month after `(y, m)`.
fn month_after(y: i64, m: i64) -> (i64, i64) {
    if m == 12 { (y + 1, 1) } else { (y, m + 1) }
}

/// An `order="…"` argument: a property name with an optional trailing
/// `desc` (default ascending; `desc desc` cancels out).
fn parse_order(v: &str) -> Option<ListOrder> {
    let v = v.trim();
    let (by, ascending) = if let Some(b) = v.strip_suffix("desc desc") {
        (b.trim(), true)
    } else if let Some(b) = v.strip_suffix(" desc") {
        (b.trim(), false)
    } else {
        (v, true)
    };
    if by.is_empty() {
        return None;
    }
    Some(ListOrder {
        by: by.to_string(),
        ascending,
    })
}

/// Recursively merge adjacent [`Node::Text(Plain(_))`] nodes so the fallback
/// text path doesn't fragment output (e.g. `[[toc]]` → one text node).
pub(crate) fn merge_text(content: Content) -> Content {
    let mut out: Content = Vec::with_capacity(content.len());
    for node in content {
        match node {
            Node::Text(TextObj::Plain(s)) => {
                if let Some(Node::Text(TextObj::Plain(prev))) = out.last_mut() {
                    prev.push_str(&s);
                } else {
                    out.push(Node::Text(TextObj::Plain(s)));
                }
            }
            other => out.push(other.map_node(&mut merge_text)),
        }
    }
    out
}

/// Wikidot's verbatim-stylesheet pipeline — the one filter shared by every
/// context that serves markup as CSS: `[[module css]]` bodies (emitted into
/// the page's `<style>`) and `[[code]]` blocks served by the legacy
/// `/code/N` endpoint (see [`crate::wikidot_page::code_block`]). `&amp;`
/// becomes `&amp;amp;` (a bare `&` stays as written), the edges are trimmed,
/// and exactly one trailing newline is appended. Byte-verified against the
/// live site on `/code/N` (two corpus pages) and the `discord` page's inline
/// `<style>`. (Live Wikidot also maps NBSP to space in this pipeline; bodies
/// reach it NBSP-free — [`crate::wikidot_page::repo_l_article_latest`]
/// normalizes at the dataset boundary — so there is nothing left to map.)
pub(crate) fn wikidot_verbatim(raw: &str) -> String {
    let mut served = raw.replace("&amp;", "&amp;amp;").trim().to_owned();
    served.push('\n');
    served
}

/// The Text_Wiki Typography substitutions applied to plain text: `...` and
/// `. . .` become an ellipsis. (`--` → em-dash is handled at the mark level.)
pub(crate) fn typography(s: &str) -> String {
    if !s.contains("...") && !s.contains(". . .") {
        return s.to_string();
    }
    s.replace("...", "\u{2026}").replace(". . .", "\u{2026}")
}
