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
}
