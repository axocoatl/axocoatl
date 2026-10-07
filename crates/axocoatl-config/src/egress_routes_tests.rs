use super::*;
use crate::egress::network_warnings;
use crate::parse_config;
use crate::types::RouteAccessYaml;

fn parse(yaml: &str) -> Result<AxocoatlConfig, String> {
    parse_config(yaml, Path::new("test.yaml")).map_err(|error| error.to_string())
}

/// A config with one route whose body is `route` (YAML flow mapping
/// contents, without the braces) and the credentials `github` (env) and
/// `registry` (file).
fn with_route(route: &str) -> String {
    format!(
        "credentials:\n  github: {{env: GITHUB_TOKEN}}\n  registry: {{file: ~/.config/axocoatl/credentials/registry}}\n\
         sandbox:\n  network: egress\n  egress:\n    allow: [npm]\n    routes:\n      - {{{route}}}\n"
    )
}

fn warnings_of(config: &AxocoatlConfig) -> Vec<String> {
    network_warnings(config)
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn the_documented_example_parses() {
    let yaml = r#"
credentials:
  github: {env: GITHUB_TOKEN}
  registry: {file: ~/.config/axocoatl/credentials/registry}
sandbox:
  network: egress
  egress:
    allow: [npm]
    routes:
      - host: github.com
        ports: [443]
        credential: github
        inject: {basic: {username: x-access-token}}
        for: [agent, terminal]
        upstream_ca: null
        rules:
          - {methods: [GET], path: "/acme/app.git/info/refs", query: {service: git-receive-pack}}
          - {methods: [POST], path: "/acme/app.git/git-receive-pack"}
      - host: registry.npmjs.org
        access: read-only
        env_placeholders: []
"#;
    let config = parse(yaml).unwrap();
    let routes = &config.sandbox.egress.as_ref().unwrap().routes;
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].credential.as_deref(), Some("github"));
    assert_eq!(
        routes[0].bindings,
        Some(vec![RouteForYaml::Agent, RouteForYaml::Terminal])
    );
    assert_eq!(routes[0].rules[0].query["service"], "git-receive-pack");
    assert_eq!(routes[0].max_request_bytes, 1 << 30);
    assert_eq!(routes[1].access, Some(RouteAccessYaml::ReadOnly));
    assert!(!routes[1].allow_encoded_responses);
    assert_eq!(
        config.credentials["github"].env.as_deref(),
        Some("GITHUB_TOKEN")
    );
    let warnings = warnings_of(&config);
    // registry.npmjs.org is in the npm preset; the route decides.
    assert!(
        warnings
            .iter()
            .any(|w| w.starts_with("sandbox.egress.routes[1].host")
                && w.contains("preset npm")
                && w.contains(
                    "on the route's ports (443) Sessions reach it only through the route"
                )),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.starts_with("credentials.registry") && w.contains("no route uses")),
        "{warnings:?}"
    );
    // Serializing skips empty route and credential blocks, so a config
    // without them round-trips unchanged.
    let plain = parse("sandbox:\n  network: egress\n  egress:\n    allow: [npm]\n").unwrap();
    let text = serde_yaml::to_string(&plain).unwrap();
    assert!(
        !text.contains("routes") && !text.contains("credentials"),
        "{text}"
    );
}

/// `Ok(())`, or `Err((field, reason fragment))`.
type Expected = Result<(), (&'static str, &'static str)>;

#[test]
fn route_validation_table() {
    // (route body, expected: Ok(()) or Err((field, reason fragment)))
    let cases: &[(&str, Expected)] = &[
        ("host: api.example.com, access: read-only", Ok(())),
        ("host: API.Example.COM., access: read-only", Ok(())),
        (
            "host: '*.example.com', access: read-only",
            Err(("routes[0].host", "wildcards are not allowed")),
        ),
        (
            "host: 203.0.113.7, access: read-only",
            Err(("routes[0].host", "not an IP address")),
        ),
        (
            "host: '[2001:db8::1]', access: read-only",
            Err(("routes[0].host", "not an IP address")),
        ),
        (
            "host: bad_name.example, access: read-only",
            Err(("routes[0].host", "letters, digits or hyphens")),
        ),
        (
            "host: a.example, ports: [0], access: read-only",
            Err(("routes[0].ports", "1-65535")),
        ),
        (
            "host: a.example, ports: [], access: read-only",
            Err(("routes[0].ports", "cannot be empty")),
        ),
        (
            "host: a.example, ports: [443, 8443], access: read-only",
            Ok(()),
        ),
        (
            "host: a.example, credential: nope, inject: {header: Authorization}, access: read-only",
            Err(("routes[0].credential", "no credentials entry")),
        ),
        (
            "host: a.example, credential: github, access: read-only",
            Err(("routes[0].inject", "says how to add it")),
        ),
        (
            "host: a.example, inject: {header: X-Api-Key}, access: read-only",
            Err(("routes[0].inject", "needs a credential")),
        ),
        (
            "host: a.example, credential: github, inject: {header: X-Api-Key}, access: read-only",
            Ok(()),
        ),
        (
            "host: a.example, credential: github, inject: {header: Authorization, format: 'Bearer {}'}, access: read-only",
            Ok(()),
        ),
        (
            "host: a.example, credential: github, inject: {header: Authorization, format: 'Bearer'}, access: read-only",
            Err(("routes[0].inject.format", "exactly one {}")),
        ),
        (
            "host: a.example, credential: github, inject: {header: Authorization, format: '{} {}'}, access: read-only",
            Err(("routes[0].inject.format", "exactly one {}")),
        ),
        (
            "host: a.example, credential: github, inject: {header: Host}, access: read-only",
            Err(("routes[0].inject.header", "cannot carry a credential")),
        ),
        (
            "host: a.example, credential: github, inject: {header: Proxy-Authorization}, access: read-only",
            Err(("routes[0].inject.header", "cannot carry a credential")),
        ),
        (
            "host: a.example, credential: github, inject: {header: 'Bad Header'}, access: read-only",
            Err(("routes[0].inject.header", "1-64 letters")),
        ),
        (
            "host: a.example, credential: github, inject: {basic: {username: x-access-token}}, access: read-only",
            Ok(()),
        ),
        (
            "host: a.example, credential: github, inject: {basic: {username: 'a:b'}}, access: read-only",
            Err(("routes[0].inject.basic.username", "without ':'")),
        ),
        (
            "host: a.example, credential: github, inject: {basic: {username: u}, header: X-A}, access: read-only",
            Err(("routes[0].inject", "exactly one of basic and header")),
        ),
        (
            "host: a.example, credential: github, inject: {basic: {username: u}, format: '{}'}, access: read-only",
            Err(("routes[0].inject.format", "only to inject.header")),
        ),
        (
            "host: a.example",
            Err(("routes[0].rules", "needs rules or access")),
        ),
        (
            "host: a.example, access: read-only, rules: [{methods: [GET], path: /}]",
            Err(("routes[0].access", "cannot both be set")),
        ),
        (
            "host: a.example, access: write",
            Err(("parse error", "")),
        ),
        (
            "host: a.example, rules: [{methods: [get], path: /}]",
            Err(("routes[0].rules[0].methods", "case-sensitive")),
        ),
        (
            "host: a.example, rules: [{methods: [CONNECT], path: /}]",
            Err(("routes[0].rules[0].methods", "CONNECT is never forwarded")),
        ),
        (
            "host: a.example, rules: [{methods: [], path: /}]",
            Err(("routes[0].rules[0].methods", "list 1-16 methods")),
        ),
        (
            "host: a.example, rules: [{methods: [GET, GET], path: /}]",
            Err(("routes[0].rules[0].methods", "listed twice")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: v2/x}]",
            Err(("routes[0].rules[0].path", "starts with '/'")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: /a//b}]",
            Err(("routes[0].rules[0].path", "'//'")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: /a/../b}]",
            Err(("routes[0].rules[0].path", "'..' segments")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: /a/./b}]",
            Err(("routes[0].rules[0].path", "'..' segments")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: /a%2Fb}]",
            Err(("routes[0].rules[0].path", "escaped '/'")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: /a%2eb}]",
            Err(("routes[0].rules[0].path", "escaped '/'")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: /a%zz}]",
            Err(("routes[0].rules[0].path", "two-digit escape")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: '/a b'}]",
            Err(("routes[0].rules[0].path", "not allowed in a path")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: '/a\\b'}]",
            Err(("routes[0].rules[0].path", "not allowed in a path")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: /repos/*.git}]",
            Err(("routes[0].rules[0].path", "whole segments")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: '/a/..;/b'}]",
            Err(("routes[0].rules[0].path", "';' parameter")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: '/a/.;x=1/b'}]",
            Err(("routes[0].rules[0].path", "';' parameter")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: '/a/;/b'}]",
            Err(("routes[0].rules[0].path", "starts with ';'")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: '/a/..%3b/b'}]",
            Err(("routes[0].rules[0].path", "';' parameter")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: '/a%252Fb'}]",
            Err(("routes[0].rules[0].path", "twice-escaped")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: '/a/v;version=1/b'}]",
            Ok(()),
        ),
        (
            "host: a.example, rules: [{methods: [GET, HEAD], path: '/v2/*/manifests/**'}]",
            Ok(()),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: /info, query: {service: '*'}}]",
            Ok(()),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: /info, query: {service: 'a&b'}}]",
            Err(("routes[0].rules[0].query.service", "without spaces")),
        ),
        (
            "host: a.example, rules: [{methods: [GET], path: /info, query: {'': x}}]",
            Err(("routes[0].rules[0].query.", "1-512 characters")),
        ),
        (
            "host: a.example, access: read-only, for: []",
            Err(("routes[0].for", "cannot be empty")),
        ),
        (
            "host: a.example, access: read-only, for: [agent, agent]",
            Err(("routes[0].for", "listed twice")),
        ),
        (
            "host: a.example, access: read-only, for: [browser]",
            Err(("parse error", "")),
        ),
        (
            "host: a.example, access: read-only, for: [setup, terminal, agent]",
            Ok(()),
        ),
        (
            "host: a.example, access: read-only, env_placeholders: [GITHUB_TOKEN, GH_TOKEN]",
            Ok(()),
        ),
        (
            "host: a.example, access: read-only, env_placeholders: [HTTPS_PROXY]",
            Err(("routes[0].env_placeholders", "does not set itself")),
        ),
        (
            "host: a.example, access: read-only, env_placeholders: [AXOCOATL_EGRESS_TOKEN]",
            Err(("routes[0].env_placeholders", "does not set itself")),
        ),
        (
            "host: a.example, access: read-only, env_placeholders: [lower]",
            Err(("routes[0].env_placeholders", "uppercase variable name")),
        ),
        (
            "host: a.example, access: read-only, max_request_bytes: 10",
            Err(("routes[0].max_request_bytes", "must be 1024-")),
        ),
        (
            "host: a.example, access: read-only, upstream_ca: relative/ca.pem",
            Err(("routes[0].upstream_ca", "absolute path")),
        ),
        (
            "host: a.example, access: read-only, upstream_ca: /nonexistent/axocoatl-test/ca.pem",
            Err(("routes[0].upstream_ca", "cannot be read")),
        ),
        (
            "host: a.example, access: read-only, unknown: 1",
            Err(("parse error", "")),
        ),
    ];
    for (route, expected) in cases {
        let result = parse(&with_route(route));
        match (expected, &result) {
            (Ok(()), Ok(_)) => {}
            (Err((field, reason)), Err(message)) => {
                let field_matches = if *field == "parse error" {
                    message.starts_with("Config parse error")
                } else {
                    message.contains(&format!("'sandbox.egress.{field}"))
                };
                assert!(
                    field_matches && message.contains(reason),
                    "route {{{route}}}: expected {field} / {reason}, got {message}"
                );
            }
            _ => panic!("route {{{route}}}: expected {expected:?}, got {result:?}"),
        }
    }
}

#[test]
fn routes_cannot_repeat_a_host_and_port_or_a_placeholder() {
    let yaml = "sandbox:\n  network: egress\n  egress:\n    routes:\n      \
                - {host: a.example, ports: [443, 8443], access: read-only}\n      \
                - {host: A.example, ports: [8443], access: full}\n";
    let error = parse(yaml).unwrap_err();
    assert!(
        error.contains("routes[0] already covers a.example:8443"),
        "{error}"
    );
    let yaml = "sandbox:\n  network: egress\n  egress:\n    routes:\n      \
                - {host: a.example, access: read-only, env_placeholders: [TOKEN]}\n      \
                - {host: b.example, access: read-only, env_placeholders: [TOKEN]}\n";
    let error = parse(yaml).unwrap_err();
    assert!(error.contains("routes[0] already sets TOKEN"), "{error}");
    let many: String = (0..=MAX_ROUTES)
        .map(|index| format!("      - {{host: h{index}.example, access: read-only}}\n"))
        .collect();
    let error = parse(&format!(
        "sandbox:\n  network: egress\n  egress:\n    routes:\n{many}"
    ))
    .unwrap_err();
    assert!(error.contains("at most 64 routes"), "{error}");
    let rules: String = (0..=MAX_ROUTE_RULES)
        .map(|index| format!("{{methods: [GET], path: /r{index}}}, "))
        .collect();
    let error = parse(&with_route(&format!("host: a.example, rules: [{rules}]"))).unwrap_err();
    assert!(error.contains("at most 64 rules"), "{error}");
}

#[test]
fn route_warnings_name_broad_credentials_and_compressed_responses() {
    let config = parse(&with_route(
        "host: a.example, credential: github, inject: {header: X-Api-Key}, \
         rules: [{methods: [GET], path: '/**'}, {methods: [POST], path: /upload/**}], \
         allow_encoded_responses: true, allow_set_cookie: true",
    ))
    .unwrap();
    let warnings = warnings_of(&config);
    let has = |field: &str, fragment: &str| {
        warnings
            .iter()
            .any(|w| w.starts_with(field) && w.contains(fragment))
    };
    assert!(
        has("sandbox.egress.routes[0].rules[0].path", "every path"),
        "{warnings:?}"
    );
    assert!(
        !has("sandbox.egress.routes[0].rules[1].path", "every path"),
        "{warnings:?}"
    );
    assert!(
        has(
            "sandbox.egress.routes[0].allow_encoded_responses",
            "compressed responses"
        ),
        "{warnings:?}"
    );
    // A host that issues tokens or sessions hands them to the container.
    assert!(
        has("sandbox.egress.routes[0].rules[0].path", "token endpoint"),
        "{warnings:?}"
    );
    assert!(
        has("sandbox.egress.routes[0].allow_set_cookie", "Set-Cookie"),
        "{warnings:?}"
    );
    // Without a credential, cookies pass anyway and nothing is said.
    let config = parse(&with_route(
        "host: a.example, access: read-only, allow_set_cookie: true",
    ))
    .unwrap();
    assert!(!warnings_of(&config)
        .iter()
        .any(|w| w.contains("allow_set_cookie")));
    // Both presets add the credential to requests for every path.
    for access in ["full", "read-only"] {
        let config = parse(&with_route(&format!(
            "host: a.example, credential: github, inject: {{header: X-Api-Key}}, access: {access}"
        )))
        .unwrap();
        assert!(
            warnings_of(&config)
                .iter()
                .any(|w| w.starts_with("sandbox.egress.routes[0].access")
                    && w.contains("every path")),
            "{access}"
        );
    }
    // Without a credential a broad route is ordinary L7 filtering.
    let config = parse(&with_route("host: a.example, access: full")).unwrap();
    assert!(!warnings_of(&config)
        .iter()
        .any(|w| w.contains("every path")));
    // A wildcard allow entry covering the route host is named.
    let config = parse(
        "sandbox:\n  network: egress\n  egress:\n    allow: [{host: '*.example.org'}]\n    routes:\n      - {host: api.example.org, access: read-only}\n",
    )
    .unwrap();
    assert!(warnings_of(&config)
        .iter()
        .any(|w| w
            .contains("api.example.org is also allowed by sandbox.egress.allow (*.example.org)")));
}

#[test]
fn stdio_mcp_servers_that_inherit_env_credentials_are_named() {
    let mcp = "mcp_servers:\n  - {name: inherits, transport: stdio, command: npx}\n  \
               - {name: clean, transport: stdio, command: npx, inherit_env: false}\n  \
               - {name: remote, transport: http, url: 'https://mcp.example.com'}\n";
    let config = parse(&format!(
        "{mcp}{}",
        with_route("host: a.example, credential: github, inject: {header: X-Api-Key}, rules: [{methods: [GET], path: /x}]")
    ))
    .unwrap();
    let warnings = warnings_of(&config);
    let named: Vec<&String> = warnings
        .iter()
        .filter(|w| w.starts_with("mcp_servers["))
        .collect();
    assert_eq!(named.len(), 1, "{warnings:?}");
    assert!(
        named[0].starts_with("mcp_servers[inherits].inherit_env")
            && named[0].contains("GITHUB_TOKEN")
            && named[0].contains("inherit_env: false"),
        "{warnings:?}"
    );
    // File credentials are not in the daemon's environment.
    let config = parse(&format!(
        "{mcp}credentials:\n  registry: {{file: ~/.config/axocoatl/credentials/registry}}\n"
    ))
    .unwrap();
    assert!(
        !warnings_of(&config)
            .iter()
            .any(|w| w.starts_with("mcp_servers[")),
        "{:?}",
        warnings_of(&config)
    );
}

#[test]
fn credentials_name_a_source_never_a_value() {
    for (yaml, fragment) in [
        (
            "credentials:\n  github: ghp_literal_value\n",
            "does not take credential values",
        ),
        (
            "credentials:\n  github: {value: ghp_literal_value}\n",
            "unknown field",
        ),
        ("credentials:\n  github: {}\n", "exactly one source"),
        (
            "credentials:\n  github: {env: A, file: /b}\n",
            "exactly one source",
        ),
        (
            "credentials:\n  github: {env: ghp_literal_value}\n",
            "not its value",
        ),
        (
            "credentials:\n  github: {file: relative/path}\n",
            "absolute path",
        ),
        ("credentials:\n  github: {file: /a/../b}\n", "'..'"),
        (
            "credentials:\n  github: {file: ghp_literal_value}\n",
            "absolute path",
        ),
        ("credentials:\n  'has space': {env: A}\n", "credential name"),
    ] {
        let error = parse(yaml).unwrap_err();
        assert!(error.contains(fragment), "{yaml}: {error}");
        assert!(
            !error.contains("ghp_literal_value"),
            "the refusal repeats the value: {error}"
        );
    }
    let config = parse(
        "credentials:\n  a: {env: TOKEN_A}\n  b: {file: /etc/axocoatl/b}\n  c: {file: ~/c}\n",
    )
    .unwrap();
    assert_eq!(config.credentials.len(), 3);
    let many: String = (0..=MAX_CREDENTIALS)
        .map(|index| format!("  c{index}: {{env: T{index}}}\n"))
        .collect();
    assert!(parse(&format!("credentials:\n{many}"))
        .unwrap_err()
        .contains("at most 64 credentials"));
}

#[test]
fn inject_refusals_never_repeat_a_value() {
    let value = "ghp_literal_value";
    for (route, field, fragment) in [
        (
            format!("host: a.example, credential: github, inject: {{header: Authorization, format: \"{value}\"}}, access: read-only"),
            "sandbox.egress.routes[0].inject.format",
            "exactly one {}",
        ),
        (
            format!("host: a.example, credential: github, inject: {{basic: {{username: x}}, format: \"Bearer {value} {{}}\"}}, access: read-only"),
            "sandbox.egress.routes[0].inject.format",
            "only to inject.header",
        ),
        (
            format!("host: a.example, credential: github, inject: {{basic: {{username: \"x:{value}\"}}}}, access: read-only"),
            "sandbox.egress.routes[0].inject.basic.username",
            "without ':'",
        ),
        (
            format!("host: a.example, credential: github, inject: {{header: X-Api-Key, basic: {{username: {value}}}, format: \"{value} {{}}\"}}, access: read-only"),
            "sandbox.egress.routes[0].inject",
            "exactly one of basic and header",
        ),
    ] {
        let error = parse(&with_route(&route)).unwrap_err();
        assert!(error.contains(field) && error.contains(fragment), "{route}: {error}");
        assert!(error.contains("not shown") || error.contains("{..}"), "{error}");
        assert!(
            !error.contains(value),
            "the refusal repeats the value: {error}"
        );
    }
}

#[test]
fn substitution_is_refused_in_credentials_and_routes_before_it_happens() {
    std::env::set_var("AXOCOATL_ROUTES_TEST_SECRET", "s3cr3t-value");
    for yaml in [
        "credentials:\n  github: {env: ${AXOCOATL_ROUTES_TEST_SECRET}}\n",
        "credentials:\n  github:\n    file: /tmp/${AXOCOATL_ROUTES_TEST_SECRET}\n",
        "credentials:\n  ${AXOCOATL_ROUTES_TEST_SECRET}: {env: A}\n",
        // Only valid YAML after substitution: the line scan catches it.
        "credentials: {github: {env: ${AXOCOATL_ROUTES_TEST_SECRET}}}\n",
        "\"credentials\":\n  github: {env: \"${AXOCOATL_ROUTES_TEST_SECRET}\"}\n",
        // An alias defined elsewhere is followed.
        "x-anchor: &secret \"${AXOCOATL_ROUTES_TEST_SECRET}\"\ncredentials:\n  github: {env: *secret}\n",
        "sandbox:\n  network: egress\n  egress:\n    routes:\n      - {host: a.example, access: read-only, upstream_ca: \"${AXOCOATL_ROUTES_TEST_SECRET}\"}\n",
        "sandbox:\n  network: egress\n  egress:\n    routes:\n      - host: a.example\n        rules: [{methods: [GET], path: /, query: {t: \"${AXOCOATL_ROUTES_TEST_SECRET}\"}}]\n",
    ] {
        let error = parse(yaml).unwrap_err();
        assert!(error.contains("${...} is not substituted here"), "{yaml}: {error}");
        assert!(!error.contains("s3cr3t-value"), "{error}");
    }
    // Elsewhere substitution still works, and a comment inside the block is
    // not a value.
    let config = parse(
        "credentials:\n  # github: ${AXOCOATL_ROUTES_TEST_SECRET}\n  github: {env: GITHUB_TOKEN} # not ${X}\nserver:\n  auth:\n    api_keys: [\"${AXOCOATL_ROUTES_TEST_SECRET}\"]\n",
    )
    .unwrap();
    assert_eq!(
        config.server.auth.api_keys[0].expose_secret(),
        "s3cr3t-value"
    );
    std::env::remove_var("AXOCOATL_ROUTES_TEST_SECRET");
}

#[cfg(unix)]
#[test]
fn upstream_ca_files_must_be_owner_only_pem() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let pem = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    let good = dir.path().join("ca.pem");
    std::fs::write(&good, pem).unwrap();
    std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o600)).unwrap();
    let route = |path: &Path| {
        with_route(&format!(
            "host: a.example, access: read-only, upstream_ca: '{}'",
            path.display()
        ))
    };
    parse(&route(&good)).unwrap();

    let open = dir.path().join("open.pem");
    std::fs::write(&open, pem).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(parse(&route(&open)).unwrap_err().contains("chmod 600"));

    let link = dir.path().join("link.pem");
    std::os::unix::fs::symlink(&good, &link).unwrap();
    assert!(parse(&route(&link)).unwrap_err().contains("symbolic link"));

    let text = dir.path().join("text.pem");
    std::fs::write(&text, "not a certificate").unwrap();
    std::fs::set_permissions(&text, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(parse(&route(&text))
        .unwrap_err()
        .contains("holds no PEM certificate"));

    assert!(parse(&route(dir.path()))
        .unwrap_err()
        .contains("not a regular file"));
}

#[test]
fn path_globs_match_whole_segments() {
    let glob = |text: &str| parse_path_glob(text).unwrap();
    let segments = |path: &str| -> Vec<String> {
        path.strip_prefix('/')
            .unwrap()
            .split('/')
            .map(String::from)
            .collect()
    };
    let matches = |pattern: &str, path: &str| {
        let parts = segments(path);
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        glob(pattern).matches(&parts)
    };
    assert!(matches("/", "/"));
    assert!(!matches("/", "/a"));
    assert!(matches("/**", "/"));
    assert!(matches("/**", "/a/b/c"));
    assert!(matches("/a/*", "/a/b"));
    assert!(!matches("/a/*", "/a/"));
    assert!(!matches("/a/*", "/a/b/c"));
    assert!(matches("/a/**", "/a"));
    assert!(matches("/a/**/z", "/a/z"));
    assert!(matches("/a/**/z", "/a/b/c/z"));
    assert!(!matches("/a/**/z", "/a/b/c/y"));
    assert!(matches("/a/b/", "/a/b/"));
    assert!(!matches("/a/b", "/a/b/"));
    assert_eq!(glob("/a%3a/**").to_string(), "/a%3A/**");
    assert!(glob("/**/x").starts_with_any());
    assert!(!glob("/x/**").starts_with_any());
}

/// Axocoatl's own route names cannot be claimed by a configured route, and
/// `host_ollama` parses and validates through the whole configuration.
#[test]
fn routes_cannot_claim_axocoatl_names_and_host_ollama_validates_in_the_whole_config() {
    for host in [
        "ollama.host.axocoatl.internal",
        "OLLAMA.HOST.AXOCOATL.INTERNAL.",
        "x.axocoatl.internal",
    ] {
        let error = parse(&with_route(&format!("host: {host}, access: full"))).unwrap_err();
        assert!(error.contains("are Axocoatl's own"), "{host}: {error}");
    }
    let yaml = "sandbox:\n  network: egress\n  egress:\n    host_ollama: {port: 11434}\n";
    let config = parse(yaml).unwrap();
    assert_eq!(
        config
            .sandbox
            .egress
            .as_ref()
            .and_then(|egress| egress.host_ollama.as_ref())
            .map(|route| route.port),
        Some(11434)
    );
    assert!(
        warnings_of(&config).is_empty(),
        "{:?}",
        warnings_of(&config)
    );
    let error =
        parse("sandbox:\n  network: egress\n  egress:\n    host_ollama: {port: 0}\n").unwrap_err();
    assert!(error.contains("host_ollama.port"), "{error}");
    let bridge =
        parse("sandbox:\n  network: bridge\n  egress:\n    host_ollama: {port: 11434}\n").unwrap();
    assert!(
        warnings_of(&bridge)
            .iter()
            .any(|warning| warning.contains("only loadout Sessions")),
        "{:?}",
        warnings_of(&bridge)
    );
}
