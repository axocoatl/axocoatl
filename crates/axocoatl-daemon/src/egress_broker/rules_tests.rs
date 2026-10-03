//! Route rules: the request table, compilation and credential sources.

use super::*;
use secrecy::ExposeSecret;

fn route_from(index: usize, yaml: &str) -> Route {
    let parsed: EgressRouteYaml = serde_yaml::from_str(yaml).unwrap();
    let mut credentials = BTreeMap::new();
    credentials.insert(
        "github".to_string(),
        CredentialSourceYaml {
            env: Some("AXOCOATL_RULES_TEST_TOKEN".into()),
            file: None,
        },
    );
    Route::compile(index, &parsed, &credentials, &[]).unwrap()
}

fn git_route() -> Route {
    route_from(
        0,
        r#"
host: GitHub.com
credential: github
inject: {basic: {username: x-access-token}}
for: [agent, terminal]
rules:
  - {methods: [GET], path: /acme/app.git/info/refs, query: {service: git-upload-pack}}
  - {methods: [GET], path: /acme/app.git/info/refs, query: {service: git-receive-pack}}
  - {methods: [POST], path: /acme/app.git/git-upload-pack}
  - {methods: [POST], path: /acme/app.git/git-receive-pack}
"#,
    )
}

fn api_route() -> Route {
    route_from(
        1,
        r#"
host: api.example.com
ports: [443, 8443]
rules:
  - {methods: [GET, HEAD], path: /repos/*/issues}
  - {methods: [POST], path: /repos/acme/*/pulls}
  - {methods: [GET], path: /v2/**}
  - {methods: [DELETE], path: /items/*/}
  - {methods: [GET], path: /search, query: {q: "*"}}
  - {methods: [PUT], path: /files/**}
  - {methods: [GET], path: /}
  - {methods: [GET], path: /a%3ab/x}
  - {methods: [GET], path: /**/raw/*}
"#,
    )
}

fn read_only_route() -> Route {
    route_from(2, "{host: registry.npmjs.org, access: read-only}")
}

fn full_route() -> Route {
    route_from(3, "{host: upload.example.com, access: full}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// Allowed by this rule label.
    Allow(&'static str),
    /// Canonical, but no rule allows it.
    Deny,
    /// Not canonical: refused before any rule.
    Bad,
}

fn decide(route: &Route, method: &str, target: &str) -> Expect {
    match canonicalize(target) {
        Err(_) => Expect::Bad,
        Ok(path) => match route.check(method, &path) {
            RuleDecision::Allowed { rule } => Expect::Allow(Box::leak(rule.into_boxed_str())),
            RuleDecision::Denied { .. } => Expect::Deny,
        },
    }
}

#[test]
fn the_rules_table() {
    use Expect::{Allow, Bad, Deny};
    let git = git_route();
    let api = api_route();
    let ro = read_only_route();
    let full = full_route();
    let upload = "route#0.rules[0]";
    let receive = "route#0.rules[1]";
    #[rustfmt::skip]
    let table: Vec<(&Route, &str, &str, Expect)> = vec![
        // Exact paths and required query values.
        (&git, "GET", "/acme/app.git/info/refs?service=git-upload-pack", Allow(upload)),
        (&git, "GET", "/acme/app.git/info/refs?service=git-receive-pack", Allow(receive)),
        (&git, "POST", "/acme/app.git/git-upload-pack", Allow("route#0.rules[2]")),
        (&git, "POST", "/acme/app.git/git-receive-pack", Allow("route#0.rules[3]")),
        (&git, "GET", "/acme/app.git/info/refs", Deny),
        (&git, "GET", "/acme/app.git/info/refs?service=git-upload-archive", Deny),
        (&git, "GET", "/acme/app.git/info/refs?service=", Deny),
        (&git, "GET", "/acme/app.git/info/refs?service", Deny),
        (&git, "GET", "/acme/app.git/info/refs?other=1&service=git-upload-pack", Allow(upload)),
        (&git, "GET", "/acme/app.git/info/refs?service=git-upload-pack&service=git-receive-pack", Deny),
        (&git, "GET", "/acme/app.git/info/refs?%73ervice=git-receive-pack&service=git-upload-pack", Deny),
        (&git, "GET", "/acme/app.git/info/refs?service=git%2Dupload%2Dpack", Allow(upload)),
        (&git, "GET", "/acme/app.git/info/refs?service=git-upload-pack%zz", Deny),
        (&git, "GET", "/acme/app.git/info/refs?service=git+upload+pack", Deny),
        (&git, "GET", "/acme/app.git/info/refs?service=GIT-UPLOAD-PACK", Deny),
        // Methods are case-sensitive and never implied.
        (&git, "get", "/acme/app.git/info/refs?service=git-upload-pack", Deny),
        (&git, "Get", "/acme/app.git/info/refs?service=git-upload-pack", Deny),
        (&git, "HEAD", "/acme/app.git/info/refs?service=git-upload-pack", Deny),
        (&git, "PUT", "/acme/app.git/git-receive-pack", Deny),
        (&git, "DELETE", "/acme/app.git", Deny),
        // Paths are case-sensitive and exact.
        (&git, "POST", "/ACME/app.git/git-receive-pack", Deny),
        (&git, "POST", "/acme/app.git/git-receive-pack/", Deny),
        (&git, "POST", "/acme/other.git/git-receive-pack", Deny),
        (&git, "POST", "/acme/app.git/git-receive-pack/extra", Deny),
        // Dot segments, empty segments and escaped separators never reach a rule.
        (&git, "POST", "/acme/app.git/../other.git/git-receive-pack", Bad),
        (&git, "POST", "/acme/other.git/../app.git/git-receive-pack", Bad),
        (&git, "POST", "/acme/./app.git/git-receive-pack", Bad),
        (&git, "POST", "/acme//app.git/git-receive-pack", Bad),
        (&git, "POST", "//acme/app.git/git-receive-pack", Bad),
        (&git, "POST", "/acme/app.git%2Fgit-receive-pack", Bad),
        (&git, "POST", "/acme/app.git%2fgit-receive-pack", Bad),
        (&git, "POST", "/acme/%2e%2e/acme/app.git/git-receive-pack", Bad),
        (&git, "POST", "/acme/%2E%2E/acme/app.git/git-receive-pack", Bad),
        (&git, "POST", "/acme/.%2e/acme/app.git/git-receive-pack", Bad),
        (&git, "POST", "/acme/app.git%5Cgit-receive-pack", Bad),
        (&git, "POST", "/acme\\app.git/git-receive-pack", Bad),
        (&git, "POST", "/acme/app.git/git-receive-pack%00", Bad),
        (&git, "POST", "/acme/app.git/git-receive-pack%0a", Bad),
        (&git, "POST", "/acme/app.git/git-%zzreceive-pack", Bad),
        (&git, "POST", "/acme/app.git/git-receive-pack%", Bad),
        (&git, "POST", "/acme/app.git/git-receive-pack#frag", Bad),
        (&git, "POST", "/acme/app.git/gït-receive-pack", Bad),
        (&git, "POST", "acme/app.git/git-receive-pack", Bad),
        (&git, "POST", "*", Bad),
        (&git, "POST", "/acme/app.git/git receive-pack", Bad),
        (&git, "POST", "/acme/app.git/<script>", Bad),
        // One-segment and any-segment globs.
        (&api, "GET", "/repos/widget/issues", Allow("route#1.rules[0]")),
        (&api, "HEAD", "/repos/widget/issues", Allow("route#1.rules[0]")),
        (&api, "POST", "/repos/widget/issues", Deny),
        (&api, "GET", "/repos/a/b/issues", Deny),
        (&api, "GET", "/repos/issues", Deny),
        (&api, "GET", "/repos//issues", Bad),
        (&api, "POST", "/repos/acme/widget/pulls", Allow("route#1.rules[1]")),
        (&api, "POST", "/repos/other/widget/pulls", Deny),
        (&api, "GET", "/v2", Allow("route#1.rules[2]")),
        (&api, "GET", "/v2/", Allow("route#1.rules[2]")),
        (&api, "GET", "/v2/library/node/manifests/22", Allow("route#1.rules[2]")),
        (&api, "GET", "/v3/library", Deny),
        (&api, "GET", "/v2/../admin", Bad),
        // A trailing slash is a segment of its own.
        (&api, "DELETE", "/items/5/", Allow("route#1.rules[3]")),
        (&api, "DELETE", "/items/5", Deny),
        (&api, "DELETE", "/items//", Bad),
        // A required parameter with any value, exactly once.
        (&api, "GET", "/search?q=", Allow("route#1.rules[4]")),
        (&api, "GET", "/search?q=rust+tls", Allow("route#1.rules[4]")),
        (&api, "GET", "/search", Deny),
        (&api, "GET", "/search?q=a&q=b", Deny),
        (&api, "GET", "/search?query=a", Deny),
        (&api, "PUT", "/files/a/b/c.txt", Allow("route#1.rules[5]")),
        (&api, "PUT", "/files", Allow("route#1.rules[5]")),
        (&api, "POST", "/files/a", Deny),
        (&api, "GET", "/", Allow("route#1.rules[6]")),
        (&api, "GET", "/?page=2", Allow("route#1.rules[6]")),
        // Escapes compare in uppercase hex; a literal is not its escape.
        (&api, "GET", "/a%3Ab/x", Allow("route#1.rules[7]")),
        (&api, "GET", "/a%3ab/x", Allow("route#1.rules[7]")),
        (&api, "GET", "/a:b/x", Deny),
        (&api, "GET", "/deep/down/raw/file", Allow("route#1.rules[8]")),
        (&api, "GET", "/raw/file", Allow("route#1.rules[8]")),
        (&api, "GET", "/deep/raw/a/b", Deny),
        // Presets.
        (&ro, "GET", "/express/-/express-5.0.0.tgz", Allow("route#2.access=read-only")),
        (&ro, "HEAD", "/express", Allow("route#2.access=read-only")),
        (&ro, "OPTIONS", "/", Allow("route#2.access=read-only")),
        (&ro, "PUT", "/express", Deny),
        (&ro, "POST", "/-/v1/login", Deny),
        (&ro, "get", "/express", Deny),
        (&ro, "GET", "/express/../../etc", Bad),
        (&full, "DELETE", "/anything/at/all", Allow("route#3.access=full")),
        (&full, "PATCH", "/x?y=z", Allow("route#3.access=full")),
        (&full, "CONNECT", "/x", Deny),
        (&full, "GET", "/x/%2F/y", Bad),
    ];
    assert!(table.len() >= 60, "{} rows", table.len());
    let mut failures = Vec::new();
    for (route, method, target, expected) in &table {
        let got = decide(route, method, target);
        if got != *expected {
            failures.push(format!(
                "{} {method} {target}: expected {expected:?}, got {got:?}",
                route.host
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_refusal_names_the_rule_that_would_allow_the_request() {
    let git = git_route();
    let path = canonicalize("/acme/app.git").unwrap();
    let RuleDecision::Denied { reason, hint } = git.check("DELETE", &path) else {
        panic!("allowed")
    };
    assert_eq!(
        reason,
        "no rule of route#0 (github.com) allows DELETE /acme/app.git"
    );
    assert!(
        hint.contains("sandbox.egress.routes[0]")
            && hint.contains("{methods: [DELETE], path: \"/acme/app.git\"}"),
        "{hint}"
    );
    assert_eq!(
        git.describe(),
        vec![
            "GET /acme/app.git/info/refs?service=git-upload-pack",
            "GET /acme/app.git/info/refs?service=git-receive-pack",
            "POST /acme/app.git/git-upload-pack",
            "POST /acme/app.git/git-receive-pack",
        ]
    );
    assert_eq!(read_only_route().describe(), vec!["GET|HEAD|OPTIONS /**"]);
}

#[test]
fn routes_compile_with_their_defaults_and_are_found_by_host_and_port() {
    let git = git_route();
    assert_eq!(git.host, "github.com");
    assert_eq!(git.ports, vec![443]);
    assert!(git.allows_binding(BindingKind::Agent) && git.allows_binding(BindingKind::Terminal));
    assert!(!git.allows_binding(BindingKind::Setup) && !git.allows_binding(BindingKind::Browser));
    let credential = git.credential.as_ref().unwrap();
    assert_eq!(credential.name, "github");
    assert_eq!(
        credential.source,
        CredentialSource::Env("AXOCOATL_RULES_TEST_TOKEN".into())
    );
    assert_eq!(
        credential.inject.header_name(),
        hyper::header::AUTHORIZATION
    );
    assert_eq!(git.max_request_bytes, 1 << 30);
    let ro = read_only_route();
    assert_eq!(ro.bindings, vec![BindingKind::Agent]);
    assert!(ro.credential.is_none());

    let header: EgressRouteYaml = serde_yaml::from_str(
        "{host: a.example, credential: github, inject: {header: X-Api-Key, format: 'Token {} v1'}, access: read-only}",
    )
    .unwrap();
    let mut credentials = BTreeMap::new();
    credentials.insert(
        "github".to_string(),
        CredentialSourceYaml {
            env: None,
            file: Some("/etc/axocoatl/token".into()),
        },
    );
    let route = Route::compile(4, &header, &credentials, &[]).unwrap();
    let credential = route.credential.unwrap();
    assert_eq!(
        credential.inject,
        Injection::Header {
            name: HeaderName::from_static("x-api-key"),
            prefix: "Token ".into(),
            suffix: " v1".into(),
        }
    );
    assert_eq!(
        credential.source,
        CredentialSource::File(PathBuf::from("/etc/axocoatl/token"))
    );

    let routes = vec![
        serde_yaml::from_str::<EgressRouteYaml>(
            "{host: a.example, ports: [443, 8443], access: read-only}",
        )
        .unwrap(),
        serde_yaml::from_str::<EgressRouteYaml>("{host: b.example, access: full}").unwrap(),
    ];
    let table = RouteTable::compile(&routes, &BTreeMap::new(), &[]).unwrap();
    assert_eq!(table.routes().len(), 2);
    assert_eq!(table.find("A.Example.", 8443).unwrap().index, 0);
    assert_eq!(table.find("b.example", 443).unwrap().index, 1);
    assert!(table.find("b.example", 8443).is_none());
    assert!(table.find("c.example", 443).is_none());
    assert!(RouteTable::default().is_empty());
}

#[cfg(unix)]
#[test]
fn credentials_are_read_at_use_and_errors_never_show_the_value() {
    use std::os::unix::fs::PermissionsExt;
    const VALUE: &str = "ghp_rules_test_value_0123456789";
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let write = |path: &Path, text: &str, mode: u32| {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    let good = dir.path().join("token");
    write(&good, &format!("{VALUE}\n"), 0o600);
    let source = CredentialSource::File(good.clone());
    let read = source
        .read("github", std::slice::from_ref(&workspace))
        .unwrap();
    assert_eq!(read.expose_secret(), VALUE);
    // Replacing the file rotates the credential.
    write(&good, "rotated-value\r\n", 0o600);
    assert_eq!(
        source.read("github", &[]).unwrap().expose_secret(),
        "rotated-value"
    );

    let mut failures = Vec::new();
    let open = dir.path().join("open");
    write(&open, VALUE, 0o644);
    failures.push(
        CredentialSource::File(open)
            .read("github", &[])
            .unwrap_err(),
    );
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&good, &link).unwrap();
    failures.push(
        CredentialSource::File(link)
            .read("github", &[])
            .unwrap_err(),
    );
    let inside = workspace.join("token");
    write(&inside, VALUE, 0o600);
    failures.push(
        CredentialSource::File(inside)
            .read("github", std::slice::from_ref(&workspace))
            .unwrap_err(),
    );
    let two_lines = dir.path().join("two-lines");
    write(&two_lines, &format!("{VALUE}\nsecond\n"), 0o600);
    failures.push(
        CredentialSource::File(two_lines)
            .read("github", &[])
            .unwrap_err(),
    );
    let empty = dir.path().join("empty");
    write(&empty, "", 0o600);
    failures.push(
        CredentialSource::File(empty)
            .read("github", &[])
            .unwrap_err(),
    );
    failures.push(
        CredentialSource::File(dir.path().join("missing"))
            .read("github", &[])
            .unwrap_err(),
    );
    std::env::remove_var("AXOCOATL_RULES_TEST_UNSET");
    failures.push(
        CredentialSource::Env("AXOCOATL_RULES_TEST_UNSET".into())
            .read("github", &[])
            .unwrap_err(),
    );
    std::env::set_var(
        "AXOCOATL_RULES_TEST_NEWLINE",
        format!("{VALUE}\nInjected: 1"),
    );
    failures.push(
        CredentialSource::Env("AXOCOATL_RULES_TEST_NEWLINE".into())
            .read("github", &[])
            .unwrap_err(),
    );
    for failure in &failures {
        let text = failure.to_string();
        assert!(
            text.starts_with("credential github is unavailable"),
            "{text}"
        );
        assert!(!text.contains(VALUE), "the error shows the value: {text}");
    }
    assert!(failures[0].reason.contains("chmod 600"), "{}", failures[0]);
    assert!(
        failures[1].reason.contains("symbolic link"),
        "{}",
        failures[1]
    );
    assert!(
        failures[2].reason.contains("inside the Workspace"),
        "{}",
        failures[2]
    );
    assert!(failures[3].reason.contains("line break"), "{}", failures[3]);
    assert!(failures[4].reason.contains("empty"), "{}", failures[4]);
    assert!(
        failures[6].reason.contains("no AXOCOATL_RULES_TEST_UNSET"),
        "{}",
        failures[6]
    );
    std::env::set_var("AXOCOATL_RULES_TEST_TOKEN", VALUE);
    assert_eq!(
        CredentialSource::Env("AXOCOATL_RULES_TEST_TOKEN".into())
            .read("github", &[])
            .unwrap()
            .expose_secret(),
        VALUE
    );
    std::env::remove_var("AXOCOATL_RULES_TEST_NEWLINE");
}

#[cfg(unix)]
#[test]
fn an_upstream_ca_is_read_when_the_route_compiles() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let ca = super::super::SessionCa::new("ses-upstream-ca").unwrap();
    let path = dir.path().join("upstream-ca.pem");
    std::fs::write(&path, ca.pem()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let yaml = format!(
        "{{host: registry.internal.example, access: read-only, upstream_ca: '{}'}}",
        path.display()
    );
    let route: EgressRouteYaml = serde_yaml::from_str(&yaml).unwrap();
    let compiled = Route::compile(0, &route, &BTreeMap::new(), &[]).unwrap();
    assert_eq!(compiled.upstream_roots, vec![ca.der().clone()]);
    assert!(compiled.upstream_roots_id.is_some());
    let refused =
        Route::compile(0, &route, &BTreeMap::new(), &[dir.path().to_path_buf()]).unwrap_err();
    assert!(refused.contains("inside the Workspace"), "{refused}");
}
