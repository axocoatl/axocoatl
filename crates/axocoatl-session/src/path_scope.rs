//! Repository path patterns: what a write scope names and what a changed path
//! is checked against.

/// Repository-relative, normalized, no traversal.
pub fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.len() > 1024
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(format!("{path:?} is not a normalized repository path"));
    }
    Ok(())
}

/// A pattern is a repository-relative path that may end in `/` and may use
/// `*`, `?` and `**`; it never escapes the repository.
pub fn validate_pattern(pattern: &str) -> Result<(), String> {
    let trimmed = pattern.strip_suffix('/').unwrap_or(pattern);
    if trimmed.is_empty()
        || pattern.len() > 512
        || pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern.contains('\0')
        || trimmed
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(format!("{pattern:?} is not a repository path pattern"));
    }
    Ok(())
}

/// Most patterns one write scope may name.
pub const MAX_WRITE_SCOPE_PATTERNS: usize = 64;

/// A write scope lists the repository paths an Agent may change. An empty
/// list is a read-only Agent; no list at all (`None` where it is optional)
/// leaves every path open.
pub fn validate_write_scope(scope: &[String]) -> Result<(), String> {
    if scope.len() > MAX_WRITE_SCOPE_PATTERNS {
        return Err(format!(
            "a write scope names at most {MAX_WRITE_SCOPE_PATTERNS} paths; this one names {}",
            scope.len()
        ));
    }
    for (index, pattern) in scope.iter().enumerate() {
        validate_pattern(pattern)?;
        if scope[..index].contains(pattern) {
            return Err(format!("{pattern:?} is listed twice"));
        }
    }
    Ok(())
}

/// Whether a scope stays inside the scope it is derived from. An unrestricted
/// parent allows anything; an unrestricted child under a restricted parent is
/// wider. Otherwise every child pattern must appear literally in the parent's
/// list, so an empty (read-only) child fits under any parent. Literal
/// containment is deliberately conservative: `lib/a/` does not fit under
/// `lib/` even though it names less.
pub fn write_scope_within(child: Option<&[String]>, parent: Option<&[String]>) -> bool {
    match (child, parent) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(child), Some(parent)) => child.iter().all(|pattern| parent.contains(pattern)),
    }
}

/// Whether a scope lets an Agent change the repository-relative `path`.
pub fn scope_allows(scope: Option<&[String]>, path: &str) -> bool {
    scope.is_none_or(|scope| scope.iter().any(|pattern| pattern_matches(pattern, path)))
}

/// Gitignore-flavoured matching. A pattern without `/` matches a file name at
/// any depth; a pattern with `/` is anchored at the repository root. A trailing
/// `/` names a directory and everything under it. `**` spans segments, `*` and
/// `?` stay within one segment.
pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    if let Some(directory) = pattern.strip_suffix('/') {
        return pattern_matches(&format!("{directory}/**"), path);
    }
    let path: Vec<&str> = path.split('/').collect();
    if !pattern.contains('/') {
        return path
            .last()
            .is_some_and(|name| segment_matches(pattern.as_bytes(), name.as_bytes()));
    }
    let pattern: Vec<&str> = pattern.split('/').collect();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        Some((head, rest)) => path.split_first().is_some_and(|(segment, tail)| {
            segment_matches(head.as_bytes(), segment.as_bytes()) && segments_match(rest, tail)
        }),
    }
}

fn segment_matches(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| segment_matches(rest, &text[skip..])),
        Some((b'?', rest)) => !text.is_empty() && segment_matches(rest, &text[1..]),
        Some((byte, rest)) => text.first() == Some(byte) && segment_matches(rest, &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_follow_repository_conventions() {
        assert!(pattern_matches("lib/orders.js", "lib/orders.js"));
        assert!(!pattern_matches("lib/orders.js", "lib/orders.jsx"));
        assert!(pattern_matches("lib/", "lib/a/b.js"));
        assert!(!pattern_matches("lib/", "library/a.js"));
        assert!(pattern_matches("*.test.js", "lib/deep/x.test.js"));
        assert!(pattern_matches("lib/*.js", "lib/x.js"));
        assert!(!pattern_matches("lib/*.js", "lib/a/x.js"));
        assert!(pattern_matches("lib/**/*.js", "lib/x.js"));
        assert!(pattern_matches("lib/**/*.js", "lib/a/b/x.js"));
        assert!(pattern_matches("**", "anything/at/all"));
        assert!(pattern_matches("lib/?.js", "lib/a.js"));
        assert!(!pattern_matches("lib/?.js", "lib/ab.js"));
    }

    #[test]
    fn paths_and_patterns_stay_inside_the_repository() {
        assert!(validate_path("lib/orders.js").is_ok());
        for bad in ["", "/etc/passwd", "lib/../x", "./lib", "lib//x", "a\\b"] {
            assert!(validate_path(bad).is_err(), "{bad:?}");
        }
        assert!(validate_pattern("lib/").is_ok());
        assert!(validate_pattern("**/*.rs").is_ok());
        for bad in ["", "/", "/lib", "../lib", "lib/../x", "a\\b"] {
            assert!(validate_pattern(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn write_scope_nesting_never_widens() {
        let scope = |patterns: &[&str]| -> Vec<String> {
            patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect()
        };
        let lib = scope(&["lib/"]);
        let lib_and_docs = scope(&["lib/", "docs/*.md"]);
        let read_only = scope(&[]);
        // Unrestricted parent: anything fits, including another unrestricted scope.
        assert!(write_scope_within(None, None));
        assert!(write_scope_within(Some(&lib), None));
        // Restricted parent: an unrestricted child is wider.
        assert!(!write_scope_within(None, Some(&lib)));
        assert!(!write_scope_within(None, Some(&read_only)));
        // Read-only fits under anything; nothing but read-only fits under read-only.
        assert!(write_scope_within(Some(&read_only), Some(&lib)));
        assert!(write_scope_within(Some(&read_only), Some(&read_only)));
        assert!(!write_scope_within(Some(&lib), Some(&read_only)));
        // Literal containment only.
        assert!(write_scope_within(Some(&lib), Some(&lib_and_docs)));
        assert!(!write_scope_within(Some(&lib_and_docs), Some(&lib)));
        assert!(!write_scope_within(Some(&scope(&["src/"])), Some(&lib)));
        assert!(!write_scope_within(Some(&scope(&["lib/a/"])), Some(&lib)));

        assert!(scope_allows(None, "anything/at/all"));
        assert!(scope_allows(Some(&lib), "lib/a.js"));
        assert!(!scope_allows(Some(&lib), "src/a.js"));
        assert!(!scope_allows(Some(&read_only), "lib/a.js"));

        assert!(validate_write_scope(&read_only).is_ok());
        assert!(validate_write_scope(&lib_and_docs).is_ok());
        assert!(validate_write_scope(&scope(&["lib/", "lib/"])).is_err());
        assert!(validate_write_scope(&scope(&["../x"])).is_err());
        let wide: Vec<String> = (0..=MAX_WRITE_SCOPE_PATTERNS)
            .map(|index| format!("dir{index}/"))
            .collect();
        assert!(validate_write_scope(&wide).is_err());
        assert!(validate_write_scope(&wide[..MAX_WRITE_SCOPE_PATTERNS]).is_ok());
    }
}
