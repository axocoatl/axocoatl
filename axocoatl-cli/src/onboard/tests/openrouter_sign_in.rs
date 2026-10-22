//! Step 3D's key from a browser sign-in, against the fake OpenRouter and
//! fake browser `axocoatl connect openrouter`'s tests use: signed in from
//! onboarding, connected before with `connect`, and, under `--yes`, the
//! step left when there is none. Nobody types, pastes or exports a key.

use super::*;
use crate::connect_cmd::openrouter::{connect_and_configure, Flags};
use crate::connect_cmd::openrouter_tests::{
    described, endpoints, issued, openrouter, signing_in, stored, CODE, KEY,
};

/// The models onboarding suggests, which the fake catalog lists.
const AGAIN: &str = "axocoatl onboard --writer openrouter --writer-model \
                     openrouter:qwen/qwen3-coder --reviewer openrouter:openai/gpt-oss-120b";

/// A Mac whose Podman machine runs, with no Ollama: onboarding with `ui`
/// and `args`, its OpenRouter API the fake's.
async fn on_a_mac(
    shims: &Shims,
    ui: Ui,
    args: OnboardArgs,
    server: &wiremock::MockServer,
) -> Onboarding {
    podman_shim(shims);
    podman_state(shims, &["machine", "running"]);
    let report = podman_report(shims).await;
    let facts = facts(
        mac("26.6"),
        16,
        report,
        shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut o = onboarding(shims, ui, facts, args);
    o.openrouter_api = format!("{}/api/v1", server.uri());
    o
}

/// `onboard --update --writer openrouter --start none`, as the release
/// gate runs it, without the first run.
fn gate_args(yes: bool) -> OnboardArgs {
    OnboardArgs {
        yes,
        update: true,
        writer: Some(WriterChoice::Openrouter),
        start: Some(StartChoice::None),
        no_smoke_test: true,
        ..OnboardArgs::default()
    }
}

/// Neither the key nor the sign-in's code is in `text`.
fn assert_no_key(text: &str, what: &str) {
    for secret in [KEY, "TESTKEY", CODE] {
        assert!(!text.contains(secret), "{what} shows {secret}:\n{text}");
    }
}

/// The configuration names the secret `openrouter`, never a key, and runs
/// the suggested models; `axocoatl doctor` (Step 5's code) says ready and
/// finds the key in the secret store.
async fn assert_connected_and_ready(o: &Onboarding) {
    let text = config_text(o);
    assert_no_key(&text, "the configuration");
    assert!(text.contains("  openrouter_billing: credits\n"), "{text}");
    assert!(text.contains("    api_key_secret: openrouter\n"), "{text}");
    assert!(!text.contains("api_key:"), "{text}");
    let config = axocoatl_config::parse_config(&text, &o.paths.config_path).unwrap();
    let openrouter = config.providers.openrouter.as_ref().unwrap();
    assert_eq!(openrouter.api_key_secret.as_deref(), Some("openrouter"));
    assert_eq!(openrouter.api_key.expose_secret(), "");
    assert_eq!(
        config::run_models(&text),
        RunModels {
            writer: Some("openrouter:qwen/qwen3-coder".into()),
            reviewer: Some("openrouter:openai/gpt-oss-120b".into()),
        }
    );
    assert_eq!(stored(&o.data_dir(), "openrouter").as_deref(), Some(KEY));
    let report =
        crate::doctor::collect(&o.paths.config_path, &o.doctor_env(&Default::default())).await;
    let item = report
        .sections
        .models
        .iter()
        .find(|item| item.name == "OpenRouter")
        .unwrap_or_else(|| panic!("{:?}", report.sections.models));
    assert_eq!(item.status, crate::doctor::Status::Ok, "{item:?}");
    assert_eq!(
        item.detail,
        "OpenRouter API key present (secret openrouter, never shown)"
    );
    assert!(report.ready, "{:?}", report.steps_left);
}

/// How many times the fake OpenRouter exchanged a sign-in's code.
async fn exchanges(server: &wiremock::MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == "/api/v1/auth/keys")
        .count()
}

/// At a terminal with no key anywhere, onboarding offers the browser
/// sign-in first; chosen, it runs `connect openrouter`'s flow (the page
/// opened and its address printed, the browser's return, the exchange),
/// stores the key as the secret `openrouter`, writes `api_key_secret` and
/// never shows the key.
#[cfg(unix)]
#[tokio::test]
async fn an_interactive_browser_sign_in_stores_the_key_as_a_secret() {
    let server = openrouter(issued(), described()).await;
    let shims = Shims::new();
    let mut answers = vec![
        Answer::Yes,   // the sandbox check container
        Answer::Yes,   // credits, no BYOK
        Answer::Enter, // the writer model
        Answer::Enter, // the reviewer model
    ];
    if std::env::var("OPENROUTER_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        answers.push(Answer::No);
    }
    answers.push(Answer::Enter); // the key: the default, the browser sign-in
    let mut o = on_a_mac(&shims, Ui::scripted(answers), gate_args(false), &server).await;
    let (browser, seen) = signing_in();
    o.openrouter_sign_in = Some((browser, endpoints(&server)));
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
    assert!(o.ui.answers.lock().unwrap().is_empty(), "{output}");
    assert_no_key(&output, "the output");
    assert!(
        output.contains(
            "? How should Axocoatl get your OpenRouter key? → Sign in to OpenRouter in your \
             browser (recommended)"
        ),
        "{output}"
    );
    for expected in [
        "  ✓ writer qwen/qwen3-coder",
        "  ✓ reviewer openai/gpt-oss-120b",
        "  Sign in to OpenRouter in your browser and allow Axocoatl to create a key. The browser \
         then returns to http://127.0.0.1:",
        "  If no browser opens, open this address in a browser on this computer:",
        "  Waiting up to 10 minutes for the sign-in (Ctrl-C stops it)…",
        "  The browser returned to Axocoatl. Getting the key from OpenRouter (it is not shown)…",
        "  ✓ Connected OpenRouter: stored the key as secret openrouter (it was never shown)",
        "    Credit limit: $10.00 on this key, $7.50 left (resets monthly)",
        "OpenRouter key: read from Axocoatl's secret store (secret openrouter) when Axocoatl \
         starts, never written in this file",
    ] {
        assert!(output.contains(expected), "{expected}\n{output}");
    }
    // The page the browser opened, and its address printed with it.
    assert!(seen.opened());
    assert_eq!(seen.query("code_challenge_method").as_deref(), Some("S256"));
    let printed = format!("    {}/auth?", server.uri());
    assert!(output.contains(&printed), "{printed}\n{output}");
    assert_eq!(exchanges(&server).await, 1);
    // The sign-in checked the key and the models: no second check.
    assert!(!output.contains("OpenRouter accepted the key"), "{output}");
    assert_connected_and_ready(&o).await;
}

/// A key `axocoatl connect openrouter` stored is used by `onboard --update
/// --writer openrouter`, at a terminal and under --yes, with no question
/// about the key: no sign-in, no `$OPENROUTER_API_KEY`, no hidden prompt.
/// The configuration `connect` wrote keeps naming the secret.
#[cfg(unix)]
#[tokio::test]
async fn a_connected_key_is_used_without_asking() {
    for interactive in [true, false] {
        let server = openrouter(issued(), described()).await;
        let shims = Shims::new();
        let ui = if interactive {
            Ui::scripted(vec![
                Answer::Yes,   // the sandbox check container
                Answer::Yes,   // credits, no BYOK
                Answer::Enter, // the writer model
                Answer::Enter, // the reviewer model
                Answer::Enter, // write the changes
            ])
        } else {
            Ui::new(Mode::Yes, false, None)
        };
        let mut o = on_a_mac(&shims, ui, gate_args(!interactive), &server).await;
        connect(&o, &server).await;
        let connected = config_text(&o);
        let code = run(&mut o).await;
        let output = o.ui.output();
        assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
        assert!(o.ui.answers.lock().unwrap().is_empty(), "{output}");
        assert_no_key(&output, "the output");
        assert!(
            output.contains(
                "  OpenRouter key: the one you connected, stored as secret openrouter (never \
                 shown)"
            ),
            "{output}"
        );
        for asked in [
            "How should Axocoatl get your OpenRouter key?",
            "$OPENROUTER_API_KEY",
            "OpenRouter API key (hidden",
            "Sign in to OpenRouter in your browser and allow Axocoatl",
        ] {
            assert!(!output.contains(asked), "{asked}\n{output}");
        }
        // Checked with OpenRouter, with no second sign-in.
        assert!(output.contains("✓ OpenRouter accepted the key"), "{output}");
        assert!(
            output.contains("✓ OpenRouter lists openai/gpt-oss-120b"),
            "{output}"
        );
        assert_eq!(exchanges(&server).await, 1);
        // Updated: the team and the run models added, the secret's lines
        // as `connect` wrote them.
        let text = config_text(&o);
        assert_ne!(text, connected);
        assert!(
            output.contains("Changes to ") && !output.contains("api_key_secret: (hidden)"),
            "{output}"
        );
        assert_connected_and_ready(&o).await;
    }
}

/// `axocoatl connect openrouter` against the fake, for the configuration
/// and data directory `o` uses: the configuration is written new.
async fn connect(o: &Onboarding, server: &wiremock::MockServer) {
    let flags = Flags {
        config: o.paths.config_path.clone(),
        secret: "openrouter".to_string(),
        writer: None,
        reviewer: None,
        no_browser: false,
        sign_in_timeout: None,
        credits_only: true,
        no_config: false,
        status: false,
        remove: false,
    };
    axocoatl_daemon::AxocoatlDaemon::initialize_data_root(&o.data_dir()).unwrap();
    let (browser, _) = signing_in();
    let (code, said) = connect_and_configure(
        &flags,
        Some(&o.paths),
        o.data_dir(),
        Vec::new(),
        browser,
        endpoints(server),
        &mut Vec::new(),
        &crate::connect_cmd::Interrupt::default(),
    )
    .await;
    assert_eq!(code, crate::connect_cmd::exit::CONNECTED, "{said}");
    assert!(said.contains("Wrote a new configuration"), "{said}");
}

/// Under --yes with no key, nothing is asked or saved and the step left
/// names `axocoatl connect openrouter`, then onboarding again; `--plan`
/// says it would offer the sign-in. Doing what the step says makes the
/// same command ready.
#[cfg(unix)]
#[tokio::test]
async fn yes_without_a_key_names_connect_then_onboard() {
    let server = openrouter(issued(), described()).await;
    let shims = Shims::new();
    // `--yes` takes a key in $OPENROUTER_API_KEY (as it always has), so the
    // step is left only without one.
    let exported = std::env::var("OPENROUTER_API_KEY").is_ok_and(|value| !value.trim().is_empty());
    if !exported {
        let mut plan = on_a_mac(
            &shims,
            Ui::new(Mode::Plan, false, None),
            OnboardArgs {
                yes: false,
                plan: true,
                ..gate_args(false)
            },
            &server,
        )
        .await;
        assert_eq!(run(&mut plan).await, exit::READY);
        let output = plan.ui.output();
        assert!(
            output.contains(
                "  would offer to sign in to OpenRouter in your browser (the key it issues is \
                 stored as secret openrouter, never shown), or to read the key from an \
                 environment variable or a hidden prompt, and check it and both models with \
                 OpenRouter"
            ),
            "{output}"
        );
        assert!(
            output.contains("+     api_key_secret: openrouter"),
            "{output}"
        );
        assert!(!plan.paths.config_path.exists());

        let mut o = on_a_mac(
            &shims,
            Ui::new(Mode::Yes, false, None),
            gate_args(true),
            &server,
        )
        .await;
        let code = run(&mut o).await;
        let output = o.ui.output();
        assert_eq!(code, exit::NOT_READY, "{output}");
        assert_eq!(
            o.steps,
            [format!(
                "Sign in to OpenRouter in your browser with `axocoatl connect openrouter`, which \
                 stores the key without showing it (add credits at \
                 https://openrouter.ai/settings/credits), then run `{AGAIN}`"
            )]
        );
        assert!(
            output.contains("Nothing was saved: no configuration was written."),
            "{output}"
        );
        assert!(!o.paths.config_path.exists());
        assert!(
            !axocoatl_daemon::secret_store::has_secret(&o.data_dir(), "openrouter")
                .unwrap_or(false)
        );
        assert_eq!(exchanges(&server).await, 0);
    }
    // The step: connect, then the same onboarding.
    let mut o = on_a_mac(
        &shims,
        Ui::new(Mode::Yes, false, None),
        gate_args(true),
        &server,
    )
    .await;
    connect(&o, &server).await;
    let mut plan = on_a_mac(
        &shims,
        Ui::new(Mode::Plan, false, None),
        OnboardArgs {
            yes: false,
            plan: true,
            ..gate_args(false)
        },
        &server,
    )
    .await;
    assert_eq!(run(&mut plan).await, exit::READY);
    assert!(
        plan.ui.output().contains(
            "  would use the OpenRouter key you connected (secret openrouter, never shown), and \
             check it and both models with OpenRouter"
        ),
        "{}",
        plan.ui.output()
    );
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
    assert_no_key(&output, "the output");
    assert_connected_and_ready(&o).await;
}

/// A connected key that the store holds but OpenRouter refuses, or a
/// secret the configuration names that is not stored: the step left signs
/// in again with `connect` rather than asking for the same key again.
#[cfg(unix)]
#[tokio::test]
async fn a_refused_or_missing_connected_key_names_connect_again() {
    let server = openrouter(issued(), described()).await;
    // Any other key is refused, as OpenRouter refuses a deleted one.
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/api/v1/key"))
        .respond_with(wiremock::ResponseTemplate::new(401))
        .with_priority(10)
        .mount(&server)
        .await;
    let shims = Shims::new();
    let mut o = on_a_mac(
        &shims,
        Ui::new(Mode::Yes, false, None),
        gate_args(true),
        &server,
    )
    .await;
    connect(&o, &server).await;
    // OpenRouter no longer knows the stored key.
    axocoatl_daemon::secret_store::set_secret(&o.data_dir(), "openrouter", b"sk-or-v1-revoked")
        .unwrap();
    let before = config_text(&o);
    assert_eq!(run(&mut o).await, exit::NOT_READY);
    assert_eq!(
        o.steps,
        [format!(
            "OpenRouter refused the key you connected (secret openrouter, HTTP 401): run \
             `axocoatl connect openrouter` to sign in again (it replaces the stored key), then \
             run `{AGAIN}`"
        )]
    );
    assert_eq!(config_text(&o), before);

    // The configuration names a secret the store does not hold.
    let exported = std::env::var("OPENROUTER_API_KEY").is_ok_and(|value| !value.trim().is_empty());
    if !exported {
        let mut o = on_a_mac(
            &shims,
            Ui::new(Mode::Yes, false, None),
            gate_args(true),
            &server,
        )
        .await;
        axocoatl_daemon::secret_store::remove_secret(&o.data_dir(), "openrouter").unwrap();
        let code = run(&mut o).await;
        let output = o.ui.output();
        assert_eq!(code, exit::NOT_READY, "{output}");
        assert!(
            output.contains(
                "  ! Your configuration reads the OpenRouter key from the secret openrouter, \
                 which is not stored"
            ),
            "{output}"
        );
        assert_eq!(o.steps.len(), 1, "{:?}", o.steps);
        assert!(
            o.steps[0].starts_with(
                "Sign in to OpenRouter in your browser with `axocoatl connect openrouter`"
            ),
            "{:?}",
            o.steps
        );
        assert_eq!(config_text(&o), before);
    }
}
