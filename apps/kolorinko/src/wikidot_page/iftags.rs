use super::*;

/// Resolve `[[iftags …]]` conditionals against `tags`: a conditional whose
/// required tags (`+tag` and plain tags, as the parser folds them) are all
/// present — and whose excluded tags (`-tag`) are all absent — is replaced by
/// its body; the others vanish.
pub(super) fn evaluate_iftags(content: Content, tags: &[String]) -> Content {
    let has = |t: &str| tags.iter().any(|pt| pt.eq_ignore_ascii_case(t));
    let mut walk = |c: Content| evaluate_iftags(c, tags);
    content
        .into_iter()
        .flat_map(|node| match node {
            Node::Container {
                kind: ContainerKind::IfTags { has_all, has_none },
                content,
            } => {
                if has_all.iter().all(|t| has(t)) && has_none.iter().all(|t| !has(t)) {
                    walk(content)
                } else {
                    Vec::new()
                }
            }
            other => vec![other.map_node(&mut walk)],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(content: &Content) -> String {
        let mut s = String::new();
        collect_plain(content, &mut s);
        s
    }

    #[test]
    fn iftags_keeps_matching_branch() {
        let src = crate::wikidot_parser::parse(
            "[[iftags +rumor]]yes[[/iftags]][[iftags -rumor]]no[[/iftags]]",
        );
        let out = evaluate_iftags(src, &["rumor".into()]);
        assert_eq!(plain(&out), "yes");
    }
}
