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
                     openrouter:qwen/qwen3-coder-30b-a3b-instruct --reviewer openrouter:openai/gpt-oss-120b";

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
            writer: Some("openrouter:qwen/qwen3-coder-30b-a3b-instruct".into()),
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
        "  ✓ writer qwen/qwen3-coder-30b-a3b-instruct",
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
        crate::connect_cmd::openrouter::Hooks::default(),
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

/// Any key but the one the fake issues is refused, as OpenRouter refuses a
/// deleted one.
async fn refuse_other_keys(server: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/api/v1/key"))
        .respond_with(wiremock::ResponseTemplate::new(401))
        .with_priority(10)
        .mount(server)
        .await;
}

/// At a terminal, a connected key OpenRouter refuses, or a stored secret
/// that holds no OpenRouter key, is replaced by onboarding's own browser
/// sign-in when the person agrees, instead of a step left that says to run
/// `connect` and onboarding again.
#[cfg(unix)]
#[tokio::test]
async fn at_a_terminal_an_unusable_connected_key_signs_in_again() {
    for stored_value in ["sk-or-v1-revoked", "not an OpenRouter key"] {
        let server = openrouter(issued(), described()).await;
        refuse_other_keys(&server).await;
        let shims = Shims::new();
        let ui = Ui::scripted(vec![
            Answer::Yes,   // the sandbox check container
            Answer::Yes,   // credits, no BYOK
            Answer::Enter, // the writer model
            Answer::Enter, // the reviewer model
            Answer::Yes,   // sign in again
            Answer::Enter, // write the changes
        ]);
        let mut o = on_a_mac(&shims, ui, gate_args(false), &server).await;
        connect(&o, &server).await;
        axocoatl_daemon::secret_store::set_secret(
            &o.data_dir(),
            "openrouter",
            stored_value.as_bytes(),
        )
        .unwrap();
        let (browser, seen) = signing_in();
        o.openrouter_sign_in = Some((browser, endpoints(&server)));
        let code = run(&mut o).await;
        let output = o.ui.output();
        assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
        assert!(o.ui.answers.lock().unwrap().is_empty(), "{output}");
        let problem = if stored_value.starts_with("sk-or-") {
            "  ✗ OpenRouter refused the key you connected (secret openrouter, HTTP 401)"
        } else {
            "  ✗ The OpenRouter key secret openrouter cannot be used (it does not hold an \
             OpenRouter key (they start with sk-or-), so it was not sent to OpenRouter)"
        };
        assert!(output.contains(problem), "{problem}\n{output}");
        assert!(
            output.contains(
                "? Sign in to OpenRouter in your browser now? OpenRouter issues a new key"
            ),
            "{output}"
        );
        assert!(seen.opened());
        assert_eq!(exchanges(&server).await, 2);
        assert_no_key(&output, "the output");
        assert!(!output.contains(stored_value), "{output}");
        assert_connected_and_ready(&o).await;
    }
}

/// A model whose every endpoint OpenRouter reports unavailable right now
/// is said before anything is written: at a terminal another model is
/// asked for (Enter takes the next one onboarding suggests that can run
/// now), and under --yes the step left says why and names that model, with
/// nothing saved.
#[cfg(unix)]
#[tokio::test]
async fn a_model_openrouter_reports_unavailable_is_said_before_the_setup_is_written() {
    use crate::connect_cmd::openrouter_tests::mount_unavailable;
    use crate::onboard::openrouter::{DEFAULT_WRITER, SUGGESTED};
    for interactive in [true, false] {
        let server = openrouter(issued(), described()).await;
        mount_unavailable(&server, "/api/v1", DEFAULT_WRITER).await;
        // Under --yes the next suggestion is down too: the one after it is
        // named.
        if !interactive {
            mount_unavailable(&server, "/api/v1", SUGGESTED[1]).await;
        }
        let instead = if interactive {
            SUGGESTED[1]
        } else {
            SUGGESTED[2]
        };
        let shims = Shims::new();
        let ui = if interactive {
            Ui::scripted(vec![
                Answer::Yes,   // the sandbox check container
                Answer::Yes,   // credits, no BYOK
                Answer::Enter, // the writer model
                Answer::Enter, // the reviewer model
                Answer::Enter, // another writer: the one onboarding suggests
                Answer::Enter, // write the changes
            ])
        } else {
            Ui::new(Mode::Yes, false, None)
        };
        let mut o = on_a_mac(&shims, ui, gate_args(!interactive), &server).await;
        connect(&o, &server).await;
        let before = config_text(&o);
        let code = run(&mut o).await;
        let output = o.ui.output();
        let why = format!(
            "The writer model cannot run now: OpenRouter reports every endpoint that could serve \
             {DEFAULT_WRITER} as unavailable or degraded right now (fake/exact: status -5, 0% of \
             requests succeeded over the last 30 minutes). This is OpenRouter's provider status, \
             not your setup: try again later, or choose another model"
        );
        assert!(output.contains(&why), "{why}\n{output}");
        if interactive {
            assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
            assert!(
                output.contains(&format!(
                    "✓ OpenRouter has an endpoint for {instead} that native Sessions can use now"
                )),
                "{output}"
            );
            assert_eq!(
                config::run_models(&config_text(&o)).writer.as_deref(),
                Some(format!("openrouter:{instead}").as_str())
            );
        } else {
            assert_eq!(code, exit::NOT_READY, "{output}");
            assert_eq!(o.steps.len(), 1, "{:?}", o.steps);
            assert!(o.steps[0].starts_with(&why), "{:?}", o.steps);
            assert!(
                o.steps[0].ends_with(&format!(
                    "Run `axocoatl onboard` again later, or with another writer model \
                     (--writer-model openrouter:VENDOR/MODEL, such as openrouter:{instead})"
                )),
                "{:?}",
                o.steps
            );
            assert_eq!(config_text(&o), before);
        }
    }
}

/// Moving a setup with a key written in the file to the browser sign-in:
/// the configuration names the secret, its comment says the key is in the
/// secret store, and the backup leaves the old key out, saying so.
#[cfg(unix)]
#[tokio::test]
async fn a_typed_key_moved_to_the_sign_in_is_left_out_of_the_backup() {
    const OLD: &str = "sk-or-v1-typed-before-the-sign-in-0123456789";
    let server = openrouter(issued(), described()).await;
    let shims = Shims::new();
    // The models are kept from the configuration, unasked.
    let mut answers = vec![
        Answer::Yes, // the sandbox check container
        Answer::Yes, // credits, no BYOK
        Answer::No,  // keep the key in the configuration
    ];
    if std::env::var("OPENROUTER_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        answers.push(Answer::No);
    }
    answers.push(Answer::Enter); // the key: the browser sign-in
    answers.push(Answer::Enter); // write the changes
    let mut o = on_a_mac(&shims, Ui::scripted(answers), gate_args(false), &server).await;
    let old = config::render(&config::ConfigPlan {
        ollama_base_url: None,
        openrouter_key: Some(config::KeyRef::Value(zeroize::Zeroizing::new(OLD.into()))),
        lead: config::ModelRef::new(
            config::Provider::OpenRouter,
            "qwen/qwen3-coder-30b-a3b-instruct",
        ),
        reviewer: config::ModelRef::new(config::Provider::OpenRouter, "openai/gpt-oss-120b"),
        run_writer: Some(config::ModelRef::new(
            config::Provider::OpenRouter,
            "qwen/qwen3-coder-30b-a3b-instruct",
        )),
        run_reviewer: Some(config::ModelRef::new(
            config::Provider::OpenRouter,
            "openai/gpt-oss-120b",
        )),
        port: o.args.port.unwrap(),
        note: "OpenRouter setup".into(),
        writer: "openrouter",
        codex_credential: None,
    });
    assert!(old.contains("# OpenRouter uses your normal API key"));
    std::fs::create_dir_all(&o.paths.config_dir).unwrap();
    std::fs::write(&o.paths.config_path, &old).unwrap();
    let (browser, _) = signing_in();
    o.openrouter_sign_in = Some((browser, endpoints(&server)));
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
    assert!(o.ui.answers.lock().unwrap().is_empty(), "{output}");
    let text = config_text(&o);
    assert!(!text.contains(OLD), "{text}");
    assert!(
        text.starts_with(
            "# Axocoatl — OpenRouter setup\n# OpenRouter's key is in Axocoatl's secret store \
             (secret openrouter), never in this file.\n"
        ),
        "{text}"
    );
    assert!(!text.contains("normal API key"), "{text}");
    let backups: Vec<_> = std::fs::read_dir(&o.paths.config_dir)
        .unwrap()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("config.yaml.bak-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    let backup = std::fs::read_to_string(backups[0].path()).unwrap();
    assert!(!backup.contains(OLD), "{backup}");
    assert!(
        backup.contains("    api_key: \"\"  # the key written here was left out of this backup"),
        "{backup}"
    );
    assert!(
        output.contains("The backup leaves out the OpenRouter key that was written in the file"),
        "{output}"
    );
    assert!(!output.contains(OLD), "{output}");
    assert_connected_and_ready(&o).await;
}

/// `--plan` reads the secret store without the store's own reads, which
/// make its directory owner-only first: a directory with other permissions
/// stays as it was.
#[cfg(unix)]
#[tokio::test]
async fn plan_leaves_the_secret_store_as_it_is() {
    use std::os::unix::fs::PermissionsExt;
    let server = openrouter(issued(), described()).await;
    let shims = Shims::new();
    let o = on_a_mac(
        &shims,
        Ui::new(Mode::Yes, false, None),
        gate_args(true),
        &server,
    )
    .await;
    connect(&o, &server).await;
    let secrets = o.data_dir().join("secrets");
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o750)).unwrap();
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
            "  would use the OpenRouter key you connected (secret openrouter, never shown)"
        ),
        "{}",
        plan.ui.output()
    );
    assert_eq!(
        std::fs::metadata(&secrets).unwrap().permissions().mode() & 0o777,
        0o750
    );
}

/// Endpoints that cannot be read (OpenRouter answers an error) do not stop
/// onboarding: it says so, and a run reads them again before it starts.
#[cfg(unix)]
#[tokio::test]
async fn unreadable_endpoints_are_said_and_do_not_stop_the_setup() {
    use crate::onboard::openrouter::DEFAULT_WRITER;
    let server = openrouter(issued(), described()).await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(format!(
            "/api/v1/models/{DEFAULT_WRITER}/endpoints"
        )))
        .respond_with(wiremock::ResponseTemplate::new(500))
        .with_priority(1)
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
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
    assert!(
        output.contains(&format!(
            "  ! The endpoints of {DEFAULT_WRITER} could not be read (openrouter API error: 500"
        )),
        "{output}"
    );
    assert!(output.contains("a run checks them again"), "{output}");
}
