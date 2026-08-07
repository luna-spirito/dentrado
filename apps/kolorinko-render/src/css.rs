//! CSS reference rewriting and URL localization for mirrored assets.
//!
//! Mirrored themes and pages keep their original absolute `@import`/`url(...)`
//! text. [`rewrite`] rewrites those references to same-origin
//! `/repo/<site>/<kind>/<host>/<path>` URLs (server-side for stylesheet files,
//! client-side for inline `[[module css]]`), and [`asset_url`] localizes a
//! single absolute URL for `<img>`/`<a>`. Everything else passes through
//! byte-for-byte.

/// Localize an absolute external URL (`http(s)://…` or `//…`) to
/// `/repo/<site>/<kind>/<host>/<path>`. `None` for anything else.
#[must_use]
pub fn asset_url(site: &str, kind: &str, url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("//"))?;
    let no_frag = rest.split('#').next().unwrap_or(rest);
    Some(format!("/repo/{site}/{kind}/{no_frag}"))
}

/// Rewrite every resolvable `@import`/`url()` reference in `css` to a local
/// `/repo/<site>/<kind>/<host>/<path>` URL. `base` is the stylesheet's own
/// original URL (needed to absolutize relative references); `None` keeps only
/// absolute references. Non-HTTP references (`data:`, document fragments,
/// unknown schemes) are left untouched.
#[must_use]
pub fn rewrite(css: &str, base: Option<&str>, site: &str, kind: &str) -> String {
    let bytes = css.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0usize;
    while i < n {
        // Comment → opaque, skip verbatim.
        if bytes[i] == b'/' && i + 1 < n && bytes[i + 1] == b'*' {
            let end = css[i + 2..].find("*/").map_or(n, |p| i + 2 + p + 2);
            out.push_str(&css[i..end]);
            i = end;
            continue;
        }
        // String literal (the `@import` target case handled just below).
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            let (s, used) = read_string(css, i, n);
            out.push_str(s);
            i += used;
            continue;
        }
        // `@import` — rewrite the trailing string target.
        if css[i..]
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("@import"))
        {
            out.push_str(&css[i..i + 7]);
            i += 7;
            let j = skip_ws(bytes, i, n);
            out.push_str(&css[i..j]);
            i = j;
            if i < n && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let (s, used) = read_string(css, i, n);
                match to_local(unquote(s.trim()), base, site, kind) {
                    Some(loc) => {
                        out.push_str("url(\"");
                        out.push_str(&loc);
                        out.push_str("\")");
                    }
                    None => out.push_str(s),
                }
                i += used;
            }
            // `@import url(...)` falls through to the generic `url(` branch.
            continue;
        }
        // `url(...)` in any property (incl. `@font-face src`).
        if css[i..]
            .get(..3)
            .is_some_and(|s| s.eq_ignore_ascii_case("url"))
        {
            let j = skip_ws(bytes, i + 3, n);
            if j < n && bytes[j] == b'(' {
                out.push_str(&css[i..j]);
                let (inner, end) = read_url_inner(css, j + 1, n);
                match to_local(unquote(inner.trim()), base, site, kind) {
                    Some(loc) => {
                        out.push_str("(\"");
                        out.push_str(&loc);
                        out.push_str("\")");
                    }
                    None => {
                        out.push('(');
                        out.push_str(inner);
                        out.push(')');
                    }
                }
                i = end;
                continue;
            }
        }
        // Default: copy the next code point.
        let ch = css[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Resolve `u` (an `@import`/`url()` payload) to a local URL, or `None` if it
/// is not an HTTP reference (or needs a base that isn't available).
fn to_local(u: &str, base: Option<&str>, site: &str, kind: &str) -> Option<String> {
    if u.is_empty()
        || u.starts_with('#')
        || u.starts_with("data:")
        || u.starts_with("blob:")
        || u.starts_with("mailto:")
        || u.starts_with("javascript:")
    {
        return None;
    }
    if let Some(rest) = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .or_else(|| u.strip_prefix("//"))
    {
        return Some(local(site, kind, rest));
    }
    let base = base?;
    if u.starts_with('/') {
        return Some(local(site, kind, &format!("{}{}", origin_of(base), u)));
    }
    if u.contains("://") {
        return None; // unknown scheme
    }
    let abs = resolve_relative(base, u);
    let rest = abs
        .strip_prefix("https://")
        .or_else(|| abs.strip_prefix("http://"))?;
    Some(local(site, kind, rest))
}

/// Build the local path, dropping any fragment (never sent to the server).
fn local(site: &str, kind: &str, host_and_path: &str) -> String {
    let no_frag = host_and_path.split('#').next().unwrap_or(host_and_path);
    format!("/repo/{site}/{kind}/{no_frag}")
}

/// `scheme://host` of `base` (everything up to the first path slash).
fn origin_of(base: &str) -> String {
    match base.find("://") {
        Some(p) => {
            let after = p + 3;
            match base[after..].find('/') {
                Some(q) => base[..after + q].to_string(),
                None => base.to_string(),
            }
        }
        None => base.to_string(),
    }
}

/// Absolute-ify a relative reference against `base`, collapsing `.`/`..`
/// (including against `base`'s own directory).
fn resolve_relative(base: &str, u: &str) -> String {
    let origin = origin_of(base);
    let dir = base
        .strip_prefix(&origin)
        .and_then(|p| p.rsplit_once('/').map(|(d, _)| d))
        .unwrap_or("");
    let mut segs: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in u.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    if segs.is_empty() {
        origin
    } else {
        format!("{origin}/{}", segs.join("/"))
    }
}

/// Consume a string literal at `i` (kept verbatim), returning its slice and
/// the number of bytes consumed (through the closing quote, or to EOF).
fn read_string(css: &str, i: usize, n: usize) -> (&str, usize) {
    let bytes = css.as_bytes();
    let q = bytes[i];
    let mut j = i + 1;
    while j < n {
        if bytes[j] == b'\\' {
            j = (j + 2).min(n);
            continue;
        }
        if bytes[j] == q {
            return (&css[i..=j], j + 1 - i);
        }
        j += 1;
    }
    (&css[i..], n - i)
}

/// Consume the payload of one balanced `url(...)` at `i` (just past the `(`),
/// returning the raw interior and the index just past the closing `)`.
fn read_url_inner(css: &str, i: usize, n: usize) -> (&str, usize) {
    let bytes = css.as_bytes();
    let mut depth = 1usize;
    let mut j = i;
    while j < n {
        match bytes[j] {
            b'\\' => j = (j + 2).min(n),
            b'"' | b'\'' => j += read_string(css, j, n).1,
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return (&css[i..j], j + 1);
                }
                j += 1;
            }
            _ => j += 1,
        }
    }
    (&css[i..], n)
}

fn skip_ws(bytes: &[u8], mut i: usize, n: usize) -> usize {
    while i < n && matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    i
}

/// Strip a single matching pair of quotes around a URL payload.
fn unquote(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::{asset_url, rewrite};

    const BASE: &str = "https://cdn.jsdelivr.net/gh/a/b@refs/heads/main/style.css";
    const SITE: &str = "rpcauthority";
    const KIND: &str = "theme";

    #[test]
    fn absolute_imports_are_localized() {
        let css = "@import url('https://fonts.googleapis.com/css?family=Exo+2');";
        assert_eq!(
            rewrite(css, Some(BASE), SITE, KIND),
            "@import url(\"/repo/rpcauthority/theme/fonts.googleapis.com/css?family=Exo+2\");"
        );
    }

    #[test]
    fn import_string_target_keeps_media_query() {
        let css = "@import \"http://maxcdn.bootstrapcdn.com/fa/css/f.min.css\" screen;";
        assert_eq!(
            rewrite(css, Some(BASE), SITE, KIND),
            "@import url(\"/repo/rpcauthority/theme/maxcdn.bootstrapcdn.com/fa/css/f.min.css\") screen;"
        );
    }

    #[test]
    fn url_refs_are_localized() {
        let css = "a{background:url(https://rpcauthority.wdfiles.com/x.png)}";
        assert_eq!(
            rewrite(css, Some(BASE), SITE, KIND),
            "a{background:url(\"/repo/rpcauthority/theme/rpcauthority.wdfiles.com/x.png\")}"
        );
    }

    #[test]
    fn relative_refs_resolve_against_base() {
        let css = "@font-face{src:url(../fonts/webfont.woff2)}";
        assert_eq!(
            rewrite(css, Some(BASE), SITE, KIND),
            "@font-face{src:url(\"/repo/rpcauthority/theme/cdn.jsdelivr.net/gh/a/b@refs/heads/fonts/webfont.woff2\")}"
        );
    }

    #[test]
    fn relative_refs_skip_without_base() {
        let css = "@font-face{src:url(../fonts/webfont.woff2)}";
        assert_eq!(
            rewrite(css, None, SITE, KIND),
            "@font-face{src:url(../fonts/webfont.woff2)}"
        );
    }

    #[test]
    fn non_http_refs_are_left_alone() {
        let css = "a{fill:url(#grad);x:url(data:image/png;base64,AA==)}";
        assert_eq!(
            rewrite(css, Some(BASE), SITE, KIND),
            "a{fill:url(#grad);x:url(data:image/png;base64,AA==)}"
        );
    }

    #[test]
    fn comments_are_opaque() {
        let css = "/* url(https://x.example/a.png) */ a{}";
        assert_eq!(
            rewrite(css, Some(BASE), SITE, KIND),
            "/* url(https://x.example/a.png) */ a{}"
        );
    }

    #[test]
    fn fragment_is_dropped_query_kept() {
        let css = "a{background:url('https://h/p.svg#foo')}";
        assert_eq!(
            rewrite(css, Some(BASE), SITE, KIND),
            "a{background:url(\"/repo/rpcauthority/theme/h/p.svg\")}"
        );
    }

    #[test]
    fn quoted_url_inner_handled() {
        let css = "b{background:url( \"https://h/q.png\" );}";
        assert_eq!(
            rewrite(css, Some(BASE), SITE, KIND),
            "b{background:url(\"/repo/rpcauthority/theme/h/q.png\");}"
        );
    }

    #[test]
    fn asset_url_localizes_absolute_and_skips_others() {
        assert_eq!(
            asset_url(SITE, "files", "https://rpcauthority.wikidot.com/a/b.png"),
            Some("/repo/rpcauthority/files/rpcauthority.wikidot.com/a/b.png".into())
        );
        assert_eq!(asset_url(SITE, "files", "/local/a.png"), None);
        assert_eq!(asset_url(SITE, "files", ""), None);
    }
}
