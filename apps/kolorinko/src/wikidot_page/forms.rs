use super::*;
use std::borrow::Cow;

// =========================================================================
// Data-form pages — a category `_template`'s `[[form]]` and its pages
// =========================================================================

/// Expand a page through its category's data-form `_template` when its body
/// is form data. Wikidot stores such pages as serialized field values
/// (`key: value` lines; wiki fields as one JSON-escaped quoted line, which
/// is why they look like a single giant line with literal `\n`), and renders
/// the template's layout half with `%%form_raw{field}%%` slots substituted —
/// the `header: ''` / `content: "…"` lines themselves never render as text
/// (verified against the live site's `home:home`). Returns the expansion
/// plus the template page as a dependency; the body passes through unchanged
/// when the category declares no form or the body doesn't parse as its form
/// data (every ordinary page).
pub(super) fn expand_form<'a>(
    body: &'a str,
    origin: &Key,
    slug: &Slug,
    state: &ResolveState,
) -> (Cow<'a, str>, Vec<PageDep>) {
    // The page's own category template first, then `_default`'s — Wikidot's
    // category-over-default precedence.
    let own = slug.0.as_deref().map(String::as_str).unwrap_or("_default");
    let both = [own, "_default"];
    let cats = &both[..if own == "_default" { 1 } else { 2 }];
    for cat in cats {
        let Some((key, tpl)) = template_of(state, cat) else {
            continue;
        };
        if &key == origin {
            continue;
        }
        if let Some(expanded) = apply_form(body, &tpl) {
            return (Cow::Owned(expanded), vec![page_dep(&key, Vec::new())]);
        }
    }
    (Cow::Borrowed(body), Vec::new())
}

/// The `(key, body)` of `<cat>:_template` on the resolving site, out of the
/// run's snapshot. `None` when the site has no such page.
fn template_of(state: &ResolveState, cat: &str) -> Option<(Key, Arc<str>)> {
    let slug = (
        Some(SafePathComponent::new(cat.to_string())?),
        SafePathComponent::new("_template".to_string())?,
    );
    let (local, _) = local_id(&state.snap, &state.site, &slug)?;
    let body = latest(&state.snap, state.space, local).map(|p| Arc::clone(&p.body))?;
    Some(((state.site.clone(), slug.0, slug.1), body))
}

/// Expand `body` as the form data of `tpl`: the template's layout half with
/// every `%%form_raw{f}%%` / `%%form_data{f}%%` slot replaced by the field's
/// value. `None` unless the template declares a `[[form]]` whose fields
/// cover every `key: value` line of `body`. (`form_raw` and `form_data`
/// differ only in HTML-escaping of text fields on Wikidot; only wiki fields
/// occur in the corpus, where both render the value.)
fn apply_form(body: &str, tpl: &str) -> Option<String> {
    let (layout, decl) = split_template(tpl);
    let fields = form_fields(decl?);
    if fields.is_empty() {
        return None;
    }
    let data = form_data(body, &fields)?;
    let mut out = layout.to_string();
    for (key, value) in data {
        for tag in ["form_raw", "form_data"] {
            out = out.replace(&format!("%%{tag}{{{key}}}%%"), &value);
        }
    }
    Some(out)
}

/// The `(layout, declaration)` halves of a `_template` page, split at the
/// first line of `=`s (Wikidot's `=====` separator; `>= 4` leniently).
fn split_template(tpl: &str) -> (&str, Option<&str>) {
    let mut off = 0;
    for line in tpl.split_inclusive('\n') {
        let l = line.trim_end();
        if l.len() >= 4 && l.chars().all(|c| c == '=') {
            return (&tpl[..off], Some(&tpl[off + line.len()..]));
        }
        off += line.len();
    }
    (tpl, None)
}

/// The field names a template's declaration lists: the two-space-indented
/// `name:` lines of its `[[form]]` block. Empty when it declares no form (a
/// plain live template — `%%content%%` pages, not form data).
fn form_fields(decl: &str) -> Vec<String> {
    if !decl.to_ascii_lowercase().contains("[[form]]") {
        return Vec::new();
    }
    decl.lines()
        .filter_map(|line| {
            let name = line.strip_prefix("  ")?.strip_suffix(':')?;
            (!name.starts_with(' ')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
            .then(|| name.to_string())
        })
        .collect()
}

/// Parse `body` as serialized form data: every non-empty line a `key: value`
/// pair with `key` among `fields`. `None` on any line that doesn't fit —
/// the body is ordinary wiki text, never form data.
fn form_data(body: &str, fields: &[String]) -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line.split_once(": ")?;
        if !fields.iter().any(|f| f == key) {
            return None;
        }
        out.push((key.to_string(), decode_value(value)?));
    }
    (!out.is_empty()).then_some(out)
}

/// One field value: `"…"` unescapes as a JSON string literal (the wiki-field
/// serialization — newlines arrive as `\n`), `'…'` drops its quotes,
/// anything else stays verbatim.
fn decode_value(v: &str) -> Option<String> {
    if v.starts_with('"') {
        serde_json::from_str(v).ok()
    } else if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        Some(v[1..v.len() - 1].to_string())
    } else {
        Some(v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAU_TPL: &str = "[[div class=\"feature feature-header\"]]\n%%form_raw{header}%%\n[[/div]]\n\n%%form_raw{content}%%\n\n[[module CSS]]\n@import url('http://css.wikidot.com/theme:standard-cover/code_');\n[[/module]]\n\n=========\n[[form]]\nfields:\n  header:\n    label: Header\n    type: wiki\n    height: 6\n  content:\n    label: Content\n    type: wiki\n    height: 12\n[[/form]]\n";

    #[test]
    fn bau_home_form_expands() {
        let body = "header: ''\ncontent: \"[[include :x:y]]\\n[[module css]]\\n.gray-divider-bar { width: 100% }\\n[[/module]]\"\n";
        let out = apply_form(body, BAU_TPL).expect("form expansion");
        assert_eq!(
            out,
            "[[div class=\"feature feature-header\"]]\n\n[[/div]]\n\n[[include :x:y]]\n[[module css]]\n.gray-divider-bar { width: 100% }\n[[/module]]\n\n[[module CSS]]\n@import url('http://css.wikidot.com/theme:standard-cover/code_');\n[[/module]]\n\n"
        );
    }

    #[test]
    fn plain_template_and_prose_never_expand() {
        // A live template (no [[form]]) leaves any body alone…
        let css_tpl = "\n%%content%%\n=========\n[[code type=\"css\"]]\n\n[[/code]]\n";
        assert!(apply_form("content: \"x\"", css_tpl).is_none());
        // …and a form template leaves non-form bodies alone.
        assert!(apply_form("[[div]]\nordinary page text\n[[/div]]", BAU_TPL).is_none());
        assert!(apply_form("unknown: \"x\"", BAU_TPL).is_none());
        assert!(apply_form("", BAU_TPL).is_none());
    }

    #[test]
    fn template_splits_and_fields_parse() {
        let (layout, decl) = split_template(BAU_TPL);
        assert!(layout.ends_with("[[/module]]\n\n"));
        assert!(decl.unwrap().starts_with("[[form]]"));
        assert_eq!(form_fields(decl.unwrap()), ["header", "content"]);
        // Bare/quoted values, `\n` escapes only inside the JSON form.
        assert_eq!(
            form_data(
                "header: top\ncontent: \"a\\nb\"",
                &["header".to_string(), "content".to_string()]
            ),
            Some(vec![
                ("header".into(), "top".into()),
                ("content".into(), "a\nb".into())
            ])
        );
    }
}
