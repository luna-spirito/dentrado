//! Link / page-ref / tag-filter helpers, colour normalisation, and the post-processing pass that fuses adjacent text fragments.

use super::*;

/// Turn a raw link target string into a [`LinkTarget`]: external URL if it
/// starts with `http://`/`https://`, otherwise an internal wiki page reference.
pub(crate) fn parse_link_target(raw: &str) -> LinkTarget {
    let trimmed = raw.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return LinkTarget::Url(trimmed.to_string());
    }
    LinkTarget::Page(parse_page_ref(trimmed))
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

/// Normalize a `##color|` argument: prefix with `#` if it's a bare hex triplet
/// of a valid length (3/4/6/8 digits).
pub(crate) fn normalize_color(c: String) -> String {
    if [3, 4, 6, 8].contains(&c.len()) && c.chars().all(is_hex_char) {
        format!("#{c}")
    } else {
        c
    }
}

// ── ListPages module arguments ────────────────────────────────────────────

/// `[[module ListPages …]]` argument map → selection parameters. Recognized
/// selectors are parsed; values with no data behind them in the export
/// (`rating`, `votes`, `parent`, `link_to`, `name`, …) and purely interactive
/// arguments (`rss`, `urlAttrPrefix`, …) are ignored. An `@URL|default`
/// reference keeps only its default — URL-passed arguments have no meaning in
/// a static render.
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
        pagetype: sel("pagetype"),
        order: sel("order").and_then(|v| parse_order(&v)),
        offset: sel("offset").and_then(|v| v.parse().ok()),
        limit: sel("limit").and_then(|v| v.parse().ok()),
        per_page: sel("perpage").and_then(|v| v.parse().ok()),
        separate: sel("separate").is_none_or(|v| parse_yes(&v)),
        wrapper: sel("wrapper").is_none_or(|v| parse_yes(&v)),
    }
}

/// One attribute's value flattened to a trimmed string: plain text with
/// `%%var%%`/`{$var$}` defaults spliced in (an unset variable contributes
/// nothing), and an `@URL|default` reference reduced to its default.
pub(crate) fn attr_value(attrs: &HashMap<String, Vec<TextObj>>, key: &str) -> Option<String> {
    let mut s = String::new();
    for o in attrs.get(key)? {
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
    let s = s.trim().to_string();
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
/// single-char path doesn't fragment output (e.g. `[[toc]]` → one text node).
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
