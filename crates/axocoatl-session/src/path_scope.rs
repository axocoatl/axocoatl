//! Repository path patterns: what a write scope names and what a changed path
//! is checked against.

/// A pattern is a repository-relative path that may end in `/` and may use
/// `*`, `?` and `**`; it never escapes the repository and never names Git's
/// own directory.
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
    if in_git_directory(trimmed) {
        return Err(format!(
            "{pattern:?} names Git's own .git directory, which no Agent with a write scope may \
             change"
        ));
    }
    Ok(())
}

/// Whether any segment of a repository path is `.git`, in any letter case:
/// Git's own settings, hooks and history, or a nested repository's. A
/// case-insensitive file system resolves `.GIT` to the same directory.
pub fn in_git_directory(path: &str) -> bool {
    path.split('/')
        .any(|segment| segment.eq_ignore_ascii_case(".git"))
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

/// Whether a scope lets an Agent change the repository-relative `path`. No
/// write scope reaches inside a `.git` directory, whatever its patterns say.
pub fn scope_allows(scope: Option<&[String]>, path: &str) -> bool {
    scope.is_none_or(|scope| {
        !in_git_directory(path) && scope.iter().any(|pattern| pattern_matches(pattern, path))
    })
}

/// Gitignore-flavoured matching. A pattern without `/` matches a file name at
/// any depth; a pattern with `/` is anchored at the repository root. A trailing
/// `/` names a directory and everything under it. `**` spans segments, `*` and
/// `?` stay within one segment. Matching takes time proportional to the
/// pattern times the path, never exponential, so a model-written pattern (the
/// `glob` tool's) cannot stall the host.
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
    wildcard_match(
        &pattern,
        &path,
        |segment| *segment == "**",
        |segment, name| segment_matches(segment.as_bytes(), name.as_bytes()),
    )
}

fn segment_matches(pattern: &[u8], text: &[u8]) -> bool {
    wildcard_match(
        pattern,
        text,
        |byte| *byte == b'*',
        |byte, text| *byte == b'?' || byte == text,
    )
}

/// Match `text` against `pattern`, where a `star` item matches any run of
/// items (none included) and every other item matches exactly one. Only the
/// most recent star is ever revisited: an earlier star can absorb anything a
/// later one would need, so backtracking further never finds another match.
fn wildcard_match<P, T>(
    pattern: &[P],
    text: &[T],
    star: impl Fn(&P) -> bool,
    one: impl Fn(&P, &T) -> bool,
) -> bool {
    let (mut next, mut position) = (0, 0);
    // The item after the latest star, and where its current attempt starts.
    let mut resume: Option<(usize, usize)> = None;
    while position < text.len() {
        if next < pattern.len() && star(&pattern[next]) {
            next += 1;
            resume = Some((next, position));
        } else if next < pattern.len() && one(&pattern[next], &text[position]) {
            next += 1;
            position += 1;
        } else if let Some((after, start)) = resume {
            // Let the star absorb one more item and try again.
            next = after;
            position = start + 1;
            resume = Some((after, position));
        } else {
            return false;
        }
    }
    pattern[next..].iter().all(star)
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

    /// The recursive definition the matcher replaced: exponential on some
    /// patterns, but plainly right, so the linear matcher must agree with it.
    fn reference_matches(pattern: &str, path: &str) -> bool {
        fn segments(pattern: &[&str], path: &[&str]) -> bool {
            match pattern.split_first() {
                None => path.is_empty(),
                Some((&"**", rest)) => (0..=path.len()).any(|skip| segments(rest, &path[skip..])),
                Some((head, rest)) => path.split_first().is_some_and(|(segment, tail)| {
                    bytes(head.as_bytes(), segment.as_bytes()) && segments(rest, tail)
                }),
            }
        }
        fn bytes(pattern: &[u8], text: &[u8]) -> bool {
            match pattern.split_first() {
                None => text.is_empty(),
                Some((b'*', rest)) => (0..=text.len()).any(|skip| bytes(rest, &text[skip..])),
                Some((b'?', rest)) => !text.is_empty() && bytes(rest, &text[1..]),
                Some((byte, rest)) => text.first() == Some(byte) && bytes(rest, &text[1..]),
            }
        }
        if let Some(directory) = pattern.strip_suffix('/') {
            return reference_matches(&format!("{directory}/**"), path);
        }
        let path: Vec<&str> = path.split('/').collect();
        if !pattern.contains('/') {
            return path
                .last()
                .is_some_and(|name| bytes(pattern.as_bytes(), name.as_bytes()));
        }
        let pattern: Vec<&str> = pattern.split('/').collect();
        segments(&pattern, &path)
    }

    #[test]
    fn matching_agrees_with_the_recursive_definition() {
        let patterns = "* ** ? *.js *.test.js a*b*c *a* a?c **/* **/*.js **/a lib/*.js lib/** \
                        lib/**/*.js lib/**/b/**/*.js **/b/* lib/ lib/a/ */a.js */*/* lib/?.js \
                        a**b **/**/x x/**/** lib/a.js **.js */** l*b/**/?.j*";
        let paths = "a b abc aXbYc ab a.js x.test.js lib/a.js lib/ab.js lib/a/b.js lib/a/b/c.js \
                     lib/b/x/y.js library/a.js x a/x a/b/x src/lib/a.js lib lib/a b/lib/a.js \
                     lib/b/a.js x/y/z";
        for pattern in patterns.split_whitespace() {
            for path in paths.split_whitespace() {
                assert_eq!(
                    pattern_matches(pattern, path),
                    reference_matches(pattern, path),
                    "{pattern:?} {path:?}"
                );
            }
        }
    }

    #[test]
    fn a_pathological_pattern_is_answered_quickly() {
        let pattern = format!("{}b", "*a".repeat(40));
        let name = "a".repeat(200);
        let started = std::time::Instant::now();
        assert!(!pattern_matches(&pattern, &name));
        let deep = format!("{}x", "**/".repeat(40));
        let path = vec!["a"; 200].join("/");
        assert!(!pattern_matches(&deep, &path));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn patterns_stay_inside_the_repository() {
        assert!(validate_pattern("lib/").is_ok());
        assert!(validate_pattern("**/*.rs").is_ok());
        for bad in ["", "/", "/lib", "../lib", "lib/../x", "a\\b"] {
            assert!(validate_pattern(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn no_write_scope_reaches_into_a_git_directory() {
        for bad in [
            ".git",
            ".git/",
            ".GIT/config",
            "lib/.git/",
            ".Git/hooks/pre-commit",
        ] {
            let refused = validate_pattern(bad).unwrap_err();
            assert!(refused.contains(".git directory"), "{refused}");
        }
        for good in [".github/", ".gitignore", "lib/.gitkeep", "git/", "*.git.md"] {
            assert!(validate_pattern(good).is_ok(), "{good:?}");
        }
        // Wildcards and bare file names match at any depth, but never there.
        let scope = |patterns: &[&str]| -> Vec<String> {
            patterns
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect()
        };
        let wide = scope(&["**", "*", "config", "HEAD", "pre-commit", "exclude"]);
        for inside in [
            ".git/config",
            ".git/HEAD",
            ".git/hooks/pre-commit",
            ".git/info/exclude",
            ".GIT/config",
            "vendor/lib/.git/config",
        ] {
            assert!(pattern_matches("**", inside));
            assert!(!scope_allows(Some(&wide), inside), "{inside}");
        }
        assert!(scope_allows(Some(&wide), "lib/config"));
        assert!(scope_allows(Some(&wide), ".github/workflows/ci.yml"));
        // Without a write scope the Agent is unrestricted.
        assert!(scope_allows(None, ".git/config"));
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
