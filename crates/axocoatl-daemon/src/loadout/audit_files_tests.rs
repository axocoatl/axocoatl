//! The audit's host listing, assignment and per-file coverage.
use super::*;
use serde_json::json;

fn write(root: &Path, files: &[(&str, &[u8])]) {
    for (path, bytes) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Audit fixture",
            "-c",
            "user.email=audit@example.invalid",
        ])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn listed(listing: &RepoListing) -> Vec<(&str, FileKind)> {
    listing
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.kind))
        .collect()
}

/// In a Git work tree the host lists what Git lists: tracked and untracked
/// files, not ignored ones, not links or files deleted from the work tree.
/// Each file's kind decides whether its worker must read it.
#[tokio::test]
async fn a_git_work_tree_is_listed_by_git_with_its_ignore_rules() {
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    let mut big = b"x".repeat(MAX_AUDITED_FILE_BYTES as usize);
    big.push(b'\n');
    let mut logo = b"\x89PNG\r\n\x1a\n".to_vec();
    logo.extend([0, 0, 0, 13]);
    write(
        root,
        &[
            (".gitignore", b"target/\n*.log\n"),
            ("README.md", b"# fixture\n"),
            ("src/lib.rs", b"pub fn one() -> u32 { 1 }\n"),
            ("src/__init__.py", b""),
            ("assets/logo.png", &logo),
            ("data/big.json", &big),
            ("src/gone.rs", b"fn gone() {}\n"),
        ],
    );
    #[cfg(unix)]
    std::os::unix::fs::symlink("src/lib.rs", root.join("link.rs")).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "fixture"]);
    std::fs::remove_file(root.join("src/gone.rs")).unwrap();
    write(
        root,
        &[
            ("target/debug/out.o", b"object"),
            ("run.log", b"log"),
            ("notes/todo.md", b"untracked, not ignored\n"),
        ],
    );
    let listing = list_repository(root).await.unwrap();
    assert_eq!(listing.method, ListingMethod::Git);
    assert!(!listing.capped);
    assert_eq!(
        listed(&listing),
        [
            (".gitignore", FileKind::Text),
            ("README.md", FileKind::Text),
            ("assets/logo.png", FileKind::Binary),
            ("data/big.json", FileKind::TooLarge),
            ("notes/todo.md", FileKind::Text),
            ("src/__init__.py", FileKind::Empty),
            ("src/lib.rs", FileKind::Text),
        ]
    );
    assert_eq!(listing.files[3].size, MAX_AUDITED_FILE_BYTES + 1);
}

/// A directory that is not a Git work tree is walked, skipping the
/// directories `glob` skips and links, in path order.
#[tokio::test]
async fn a_directory_outside_git_is_walked_skipping_what_glob_skips() {
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path();
    write(
        root,
        &[
            ("b.py", b"print(1)\n"),
            ("a/z.rs", b"fn z() {}\n"),
            ("a/.hidden/c.txt", b"kept\n"),
            ("node_modules/pkg/index.js", b"skipped\n"),
            ("target/debug/out", b"skipped\n"),
            (".git/objects/lib.rs", b"skipped\n"),
            ("__pycache__/b.pyc", b"skipped\n"),
        ],
    );
    #[cfg(unix)]
    std::os::unix::fs::symlink("b.py", root.join("link.py")).unwrap();
    let listing = list_repository(root).await.unwrap();
    assert_eq!(listing.method, ListingMethod::Walk);
    assert_eq!(
        listed(&listing),
        [
            ("a/.hidden/c.txt", FileKind::Text),
            ("a/z.rs", FileKind::Text),
            ("b.py", FileKind::Text),
        ]
    );
    // Past the bound the listing stops and says so.
    let listing = list_repository_within(root, 2).await.unwrap();
    assert!(listing.capped);
    assert_eq!(listing.files.len(), 2);
    let listing = list_repository_within(root, 3).await.unwrap();
    assert!(!listing.capped);
    // A directory that cannot be listed is an error, never a short list.
    assert!(list_repository(&root.join("missing")).await.is_err());
}

/// A file name that is not UTF-8 cannot be named to `read_file`: it is
/// counted, never silently dropped. (macOS file systems refuse such names.)
#[cfg(target_os = "linux")]
#[tokio::test]
async fn names_that_are_not_utf8_are_counted() {
    use std::os::unix::ffi::OsStrExt;
    let repo = tempfile::tempdir().unwrap();
    write(repo.path(), &[("a.py", b"print(1)\n")]);
    std::fs::write(
        repo.path()
            .join(std::ffi::OsStr::from_bytes(b"latin\xe9.txt")),
        "text\n",
    )
    .unwrap();
    let listing = list_repository(repo.path()).await.unwrap();
    assert_eq!(listed(&listing), [("a.py", FileKind::Text)]);
    assert_eq!(listing.unnamed, 1);
}

fn area(name: &str, paths: &[&str]) -> AuditArea {
    AuditArea {
        name: name.into(),
        scope: format!("the {name} code"),
        paths: paths.iter().map(|path| path.to_string()).collect(),
    }
}

fn text(path: &str) -> RepoFile {
    RepoFile {
        path: path.into(),
        size: 100,
        kind: FileKind::Text,
    }
}

/// Each file goes to the first area whose paths match it; one no area's
/// paths match goes to the area sharing the most leading directories with
/// it, else to the host-made rest area; an area left without files is not
/// run.
#[test]
fn files_go_to_the_first_matching_area_then_by_directory_then_to_rest() {
    let plan = AuditPlan {
        areas: vec![
            area("auth-tokens", &["auth/tokens.py"]),
            area("billing", &["./billing/"]),
            area("api", &["src/api/**", "/src/api.rs"]),
            area("python", &["**/*.py"]),
            area("scripts", &["*.sh"]),
            area("docs", &[]),
        ],
    };
    let files: Vec<RepoFile> = [
        "README.md",
        "auth/__init__.py",
        "auth/tokens.py",
        "billing/pagination.py",
        "billing/sub/x.py",
        "src/api.rs",
        "src/api/routes.rs",
        "src/main.rs",
        "tests/test_billing.py",
        "tools/run.sh",
        "vendor/lib/a.c",
    ]
    .into_iter()
    .map(text)
    .collect();
    let assignment = assign(&plan, &files);
    let shown: Vec<(&str, bool, Vec<&str>, Vec<&str>)> = assignment
        .areas
        .iter()
        .map(|assigned| {
            (
                assigned.area.name.as_str(),
                assigned.host_made,
                assigned
                    .files
                    .iter()
                    .map(|file| file.path.as_str())
                    .collect(),
                assigned.by_path.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            ("auth-tokens", false, vec!["auth/tokens.py"], vec![]),
            (
                "billing",
                false,
                vec!["billing/pagination.py", "billing/sub/x.py"],
                vec![]
            ),
            (
                "api",
                false,
                vec!["src/api.rs", "src/api/routes.rs", "src/main.rs"],
                vec!["src/main.rs"]
            ),
            (
                "python",
                false,
                vec!["auth/__init__.py", "tests/test_billing.py"],
                vec![]
            ),
            ("scripts", false, vec!["tools/run.sh"], vec![]),
            ("rest", true, vec!["README.md", "vendor/lib/a.c"], vec![]),
        ]
    );
    // A pattern that matches wins over shared directories: auth/__init__.py
    // shares auth/ with auth-tokens, but python's pattern matches it.
    assert_eq!(assignment.without_files, ["docs"]);
    assert_eq!(
        assignment.names(),
        ["auth-tokens", "billing", "api", "python", "scripts", "rest"]
    );

    // Without a pattern that matches, an empty __init__.py goes by its
    // directory, and a tie goes to the first area.
    let plan = AuditPlan {
        areas: vec![
            area("auth-tokens", &["auth/tokens.py"]),
            area("a", &["src/a/**"]),
            area("b", &["src/b/**"]),
            area("rest", &["docs/**"]),
        ],
    };
    let mut init = text("auth/__init__.py");
    init.kind = FileKind::Empty;
    init.size = 0;
    let assignment = assign(
        &plan,
        &[
            init,
            text("src/main.rs"),
            text("LICENSE"),
            text("docs/a.md"),
        ],
    );
    let names: Vec<(&str, Vec<&str>)> = assignment
        .areas
        .iter()
        .map(|assigned| {
            (
                assigned.area.name.as_str(),
                assigned
                    .files
                    .iter()
                    .map(|file| file.path.as_str())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            ("auth-tokens", vec!["auth/__init__.py"]),
            ("a", vec!["src/main.rs"]),
            ("rest", vec!["docs/a.md"]),
            ("rest-2", vec!["LICENSE"]),
        ]
    );
    assert_eq!(assignment.without_files, ["b"]);
    assert!(assignment.areas[3].host_made && assignment.areas[3].area.paths.is_empty());
}

/// Area patterns hold what they match, and a literal one a directory.
#[test]
fn area_patterns_hold_what_they_match() {
    for (pattern, path, held) in [
        ("billing/**", "billing/x.py", true),
        ("./billing/**", "billing/x.py", true),
        ("/billing/**", "billing/sub/x.py", true),
        ("billing", "billing/x.py", true),
        ("billing/", "billing/x.py", true),
        ("billing", "billing", true),
        ("billing/*.py", "billing/sub/x.py", false),
        ("billing", "billing2/x.py", false),
        ("*.py", "deep/in/x.py", true),
        ("", "x.py", false),
    ] {
        assert_eq!(area_holds(pattern, path), held, "{pattern} {path}");
    }
}

/// A worker's file list names each file while it fits, else the
/// directories its files are in with counts, as deep as fits.
#[test]
fn a_file_list_names_files_while_they_fit_then_directories() {
    let few = ["src/a.rs", "src/b.rs"];
    assert_eq!(
        file_list(&few, 1024),
        ("- src/a.rs\n- src/b.rs\n".to_owned(), false)
    );
    let many: Vec<String> = (0..300)
        .map(|index| format!("src/module{}/part{index}.rs", index % 3))
        .chain((0..5).map(|index| format!("root{index}.txt")))
        .collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let (list, grouped) = file_list(&many, 1024);
    assert!(grouped);
    assert_eq!(
        list,
        "- the repository's root directory itself: 5 files\n\
         - src/module0/ and below: 100 files\n\
         - src/module1/ and below: 100 files\n\
         - src/module2/ and below: 100 files\n"
    );
    // Too many directories even at the top: as many as fit, then a count.
    let spread: Vec<String> = (0..400)
        .map(|index| format!("dir{index:03}/f.rs"))
        .collect();
    let spread: Vec<&str> = spread.iter().map(String::as_str).collect();
    let (list, grouped) = file_list(&spread, 512);
    assert!(grouped && list.len() <= 512, "{list}");
    assert!(list.starts_with("- dir000/ and below: 1 file\n"), "{list}");
    let shown = list.lines().count() - 1;
    assert!(
        list.ends_with(&format!(
            "- and {} more directories ({} files)\n",
            400 - shown,
            400 - shown
        )),
        "{list}"
    );
}

/// A `read_file` of `path` with `offset` (as the call gave it) that
/// returned `returned` bytes from `from` (the result's `offset`, which the
/// tool states for a read past the start), or failed.
fn read(
    path: &str,
    offset: Option<serde_json::Value>,
    returned: Option<(u64, u64)>,
) -> ToolCallRecord {
    let mut arguments = json!({ "path": path });
    if let Some(offset) = offset {
        arguments["offset"] = offset;
    }
    let result = match returned {
        Some((from, returned)) => {
            let mut result = json!({"content": "…", "returned_bytes": returned});
            if from > 0 {
                result["offset"] = json!(from);
            }
            result
        }
        None => serde_json::Value::Null,
    };
    ToolCallRecord {
        node_id: "n".into(),
        generation: 1,
        tool: "read_file".into(),
        arguments,
        succeeded: returned.is_some(),
        result,
    }
}

/// A read from the start that returned `returned` bytes.
fn whole(path: &str, returned: u64) -> ToolCallRecord {
    read(path, None, Some((0, returned)))
}

fn other(tool: &str, arguments: serde_json::Value) -> ToolCallRecord {
    ToolCallRecord {
        node_id: "n".into(),
        generation: 1,
        tool: tool.into(),
        arguments,
        succeeded: true,
        result: serde_json::Value::Null,
    }
}

/// A text file is read when the windows succeeded `read_file` calls of it
/// returned cover every byte, whatever its size; nothing else reads a file,
/// a call whose result was not kept reads nothing, and empty, binary and
/// too-large files need no read.
#[test]
fn a_file_is_read_only_when_read_file_returned_all_of_it() {
    let repo = Path::new("/work/repo");
    let unread_of = |files: &[RepoFile], calls: &[ToolCallRecord]| -> Vec<String> {
        let calls: Vec<&ToolCallRecord> = calls.iter().collect();
        unread(files, &calls, repo)
            .into_iter()
            .map(|file| file.path.clone())
            .collect()
    };
    // 100 bytes.
    let small = [text("billing/pagination.py")];
    let not_kept = {
        let mut call = whole("billing/pagination.py", 100);
        call.result = serde_json::Value::Null;
        call
    };
    for (calls, read_it) in [
        (vec![whole("billing/pagination.py", 100)], true),
        (vec![whole("/work/repo/billing/pagination.py", 100)], true),
        (vec![whole("./auth/../billing/pagination.py", 100)], true),
        // A partial read of a small file reads part of it (the 1.3.0 rc7
        // re-smoke's `limit: 64` read of an 802-byte file).
        (vec![whole("billing/pagination.py", 64)], false),
        (
            vec![
                whole("billing/pagination.py", 64),
                read("billing/pagination.py", Some(json!(64)), Some((64, 36))),
            ],
            true,
        ),
        (
            vec![read(
                "billing/pagination.py",
                Some(json!(40)),
                Some((40, 60)),
            )],
            false,
        ),
        // The call's own offset places a window whose result names none.
        (
            vec![
                read("billing/pagination.py", Some(json!(50)), Some((0, 50))),
                whole("billing/pagination.py", 50),
            ],
            true,
        ),
        (
            vec![read(
                "billing/pagination.py",
                Some(json!("50")),
                Some((0, 50)),
            )],
            false,
        ),
        (vec![read("billing/pagination.py", None, None)], false),
        (vec![not_kept], false),
        (vec![whole("billing/other.py", 100)], false),
        (vec![whole("/elsewhere/billing/pagination.py", 100)], false),
        (vec![whole("../billing/pagination.py", 100)], false),
        (
            vec![
                other(
                    "grep",
                    json!({"pattern": "def", "path": "billing/pagination.py"}),
                ),
                other("list_dir", json!({"path": "billing"})),
                other("glob", json!({"pattern": "billing/*.py"})),
                other("bash", json!({"command": "cat billing/pagination.py"})),
            ],
            false,
        ),
        (vec![], false),
    ] {
        assert_eq!(unread_of(&small, &calls).is_empty(), read_it, "{calls:?}");
    }

    // A file of 200 KiB read in 8 KiB windows, and in 64 KiB ones.
    let large = [RepoFile {
        path: "src/big.rs".into(),
        size: 200 * 1024,
        kind: FileKind::Text,
    }];
    let windows = |size: u64, starts: &[u64]| -> Vec<ToolCallRecord> {
        starts
            .iter()
            .map(|start| {
                let returned = size.min(200 * 1024 - start);
                read("src/big.rs", Some(json!(start)), Some((*start, returned)))
            })
            .collect()
    };
    let every = |size: u64| -> Vec<u64> {
        (0..(200 * 1024u64).div_ceil(size))
            .map(|n| n * size)
            .collect()
    };
    assert!(unread_of(&large, &windows(8 * 1024, &every(8 * 1024))).is_empty());
    assert!(unread_of(&large, &windows(64 * 1024, &every(64 * 1024))).is_empty());
    assert_eq!(
        unread_of(&large, &windows(8 * 1024, &every(8 * 1024)[1..])),
        ["src/big.rs"]
    );
    assert_eq!(
        unread_of(&large, &windows(64 * 1024, &[0, 64 * 1024, 2 * 64 * 1024])),
        ["src/big.rs"]
    );
    // Windows that overlap, as when a window ends before a cut character,
    // count once.
    let mut overlapping = windows(64 * 1024, &[0, 64 * 1024 - 3, 128 * 1024 - 6]);
    overlapping.extend(windows(64 * 1024, &[192 * 1024 - 9]));
    assert!(unread_of(&large, &overlapping).is_empty());
    let coverage = Coverage::of(&overlapping.iter().collect::<Vec<_>>(), repo);
    assert_eq!(coverage.covered(&large[0]), 200 * 1024);
    assert_eq!(coverage.covered_text(&large), 200 * 1024);
    let gap = Coverage::of(
        &windows(8 * 1024, &[0, 16 * 1024])
            .iter()
            .collect::<Vec<_>>(),
        repo,
    );
    assert_eq!(gap.covered(&large[0]), 16 * 1024);
    assert!(!gap.read_whole(&large[0]));

    // Only text files are ever unread.
    let kinds = [
        RepoFile {
            path: "a/__init__.py".into(),
            size: 0,
            kind: FileKind::Empty,
        },
        RepoFile {
            path: "a/logo.png".into(),
            size: 4096,
            kind: FileKind::Binary,
        },
        RepoFile {
            path: "a/data.json".into(),
            size: MAX_AUDITED_FILE_BYTES + 1,
            kind: FileKind::TooLarge,
        },
        text("a/main.py"),
    ];
    assert_eq!(unread_of(&kinds, &[]), ["a/main.py"]);
}

/// What one worker reads within its budget: its invocations less a fifth
/// (at least 4) held back, at two a read, at most a turn's 200, in windows
/// of its model's default read.
#[test]
fn a_read_budget_is_estimated_from_invocations_and_the_context() {
    let budget = ReadBudget::new(300, Some(32_768));
    assert_eq!(
        (
            budget.window,
            budget.reads,
            budget.invocations,
            budget.reserved,
            budget.capped
        ),
        (8 * 1024, 120, 300, 60, false)
    );
    assert_eq!(ReadBudget::new(300, None).window, READ_WINDOW_BYTES);
    assert_eq!(ReadBudget::new(10, None).reads, 3);
    // Never more than a turn is planned.
    let large = ReadBudget::new(2000, None);
    assert_eq!((large.reads, large.capped), (MAX_TURN_READS, true));
    assert_eq!(ReadBudget::new(1, None).reads, 1);
    let file = |size: u64, kind: FileKind| RepoFile {
        path: "f".into(),
        size,
        kind,
    };
    assert_eq!(budget.reads_of(&file(100, FileKind::Text)), 1);
    assert_eq!(budget.reads_of(&file(8 * 1024, FileKind::Text)), 1);
    assert_eq!(budget.reads_of(&file(8 * 1024 + 1, FileKind::Text)), 2);
    assert_eq!(budget.reads_of(&file(200 * 1024, FileKind::Text)), 25);
    for kind in [FileKind::Empty, FileKind::Binary, FileKind::TooLarge] {
        assert_eq!(budget.reads_of(&file(4096, kind)), 0);
    }
}

/// An area whose files need more reads than one worker makes becomes
/// numbered sub-areas of consecutive files, each within the budget and
/// about the same size, named apart from the other areas; an area within
/// the budget, and one with a single file, stay whole.
#[test]
fn an_area_too_large_for_one_worker_is_split_into_numbered_sub_areas() {
    let plan = AuditPlan {
        areas: vec![
            AuditArea {
                name: "big".into(),
                scope: "everything big".into(),
                paths: vec!["big/**".into()],
            },
            AuditArea {
                name: "big-2".into(),
                scope: "a planned area named like a part".into(),
                paths: vec!["other/**".into()],
            },
            AuditArea {
                name: "huge".into(),
                scope: "one huge file".into(),
                paths: vec!["huge/**".into()],
            },
        ],
    };
    let mut files: Vec<RepoFile> = (0..500)
        .map(|index| text(&format!("big/m{index:03}.py")))
        .collect();
    files.push(RepoFile {
        path: "big/zz_empty.py".into(),
        size: 0,
        kind: FileKind::Empty,
    });
    files.push(text("other/a.py"));
    files.push(RepoFile {
        path: "huge/data.txt".into(),
        size: 200 * 1024,
        kind: FileKind::Text,
    });
    let budget = ReadBudget::new(300, Some(32_768));
    let assignment = split(assign(&plan, &files), &budget);
    let names = assignment.names();
    assert_eq!(
        names,
        [
            "big-1",
            "big-part2",
            "big-3",
            "big-4",
            "big-5",
            "big-2",
            "huge"
        ]
    );
    let parts: Vec<&AssignedArea> = assignment.areas.iter().take(5).collect();
    let sizes: Vec<usize> = parts.iter().map(|part| part.text_files().count()).collect();
    assert_eq!(sizes, [100, 100, 100, 100, 100]);
    assert!(sizes.iter().all(|size| *size as u64 <= budget.reads));
    for (index, part) in parts.iter().enumerate() {
        assert_eq!(
            part.part,
            Some(Part {
                of: "big".into(),
                number: index + 1,
                count: 5
            })
        );
        assert_eq!(part.area.scope, "everything big");
        assert_eq!(part.area.paths, ["big/**"]);
    }
    // Consecutive by path: the first part starts the area, the last holds
    // its end, the empty file included.
    assert_eq!(parts[0].files[0].path, "big/m000.py");
    assert_eq!(parts[1].files[0].path, "big/m100.py");
    assert_eq!(parts[4].files.last().unwrap().path, "big/zz_empty.py");
    assert_eq!(
        parts.iter().map(|part| part.files.len()).sum::<usize>(),
        501
    );
    assert_eq!(
        assignment.split,
        [SplitArea {
            name: "big".into(),
            to_read: 500,
            reads: 500,
            parts: vec![
                "big-1".into(),
                "big-part2".into(),
                "big-3".into(),
                "big-4".into(),
                "big-5".into()
            ],
        }]
    );
    // An area within the budget stays whole: one file of 25 reads.
    assert!(assignment.areas[6].part.is_none());
    // A budget of a few reads gives a file that needs more a part of its
    // own; a file is never split.
    let tight = ReadBudget::new(10, Some(32_768));
    let plan = AuditPlan {
        areas: vec![AuditArea {
            name: "mixed".into(),
            scope: "s".into(),
            paths: vec!["m/**".into()],
        }],
    };
    let files = [
        text("m/a.py"),
        RepoFile {
            path: "m/b.txt".into(),
            size: 40 * 1024,
            kind: FileKind::Text,
        },
        text("m/c.py"),
        text("m/d.py"),
    ];
    let assignment = split(assign(&plan, &files), &tight);
    let parts: Vec<Vec<&str>> = assignment
        .areas
        .iter()
        .map(|area| area.files.iter().map(|file| file.path.as_str()).collect())
        .collect();
    assert_eq!(
        parts,
        [vec!["m/a.py"], vec!["m/b.txt"], vec!["m/c.py", "m/d.py"]]
    );
    // Within the budget, nothing changes.
    let small = split(assign(&plan, &files), &ReadBudget::new(300, None));
    assert_eq!(small.names(), ["mixed"]);
    assert!(small.split.is_empty() && small.areas[0].part.is_none());
}
