//! The interactive paths found in live runs at a terminal: an update the
//! person asked for, a computer too small for the tested local pair,
//! OpenRouter without a key, and what Steps 4 and 5 show.

use super::*;

/// The models a fake Ollama loaded (each `POST /api/generate` loads one).
async fn loads(ollama: &FakeOllama, model: &str) -> usize {
    ollama
        .server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == "/api/generate")
        .filter(|request| {
            serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| body["model"] == model)
        })
        .count()
}

/// I2. "Update this setup" with a new writer: the change shown follows
/// from what the person chose, so Enter writes it (the old default, No,
/// ended in "run `axocoatl onboard` again", which asked the same again).
#[cfg(unix)]
#[tokio::test]
async fn enter_writes_the_update_the_person_chose() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    ollama_shim(&shims);
    let ollama = FakeOllama::start(
        "0.40.6",
        shims.home.path(),
        shims.dir.path(),
        &["qwen3-coder:30b", "gpt-oss:20b"],
    )
    .await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    std::fs::write(shims.dir.path().join("big-context"), "").unwrap();
    let report = podman_report(&shims).await;
    let facts = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::scripted(vec![
            Answer::Pick(0), // update this setup
            Answer::Pick(0), // Claude Code writes now
            Answer::Enter,   // the sandbox check container
            Answer::No,      // do not build the claude-code image now
            Answer::Enter,   // write these changes
        ]),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            start: Some(StartChoice::None),
            no_smoke_test: true,
            ..OnboardArgs::default()
        },
    );
    create_private_dir(&o.paths.config_dir);
    let old = config::render(&config::ConfigPlan {
        ollama_base_url: Some(ollama.url()),
        openrouter_key: None,
        lead: config::ModelRef::new(config::Provider::Ollama, "qwen3-coder:30b"),
        reviewer: config::ModelRef::new(config::Provider::Ollama, "gpt-oss:20b"),
        run_writer: Some(config::ModelRef::new(
            config::Provider::Ollama,
            "qwen3-coder:30b",
        )),
        run_reviewer: Some(config::ModelRef::new(
            config::Provider::Ollama,
            "gpt-oss:20b",
        )),
        port: o.args.port.unwrap(),
        note: "local Ollama setup".into(),
        writer: "ollama",
        codex_credential: None,
    });
    std::fs::write(&o.paths.config_path, &old).unwrap();
    run(&mut o).await;
    let output = o.ui.output();
    assert!(
        output.contains("next to it. [Y/n] Enter"),
        "Enter answers yes: {output}"
    );
    let new = config_text(&o);
    assert_eq!(
        config::writer_marker(&new).map(|marker| marker.writer),
        Some("claude-code".to_string()),
        "{output}"
    );
    assert!(output.contains("Configuration written"), "{output}");
    assert!(
        !o.steps
            .iter()
            .any(|step| step.contains("The configuration was not changed")),
        "{:?}",
        o.steps
    );
}

/// I3. On a 12 GB computer the tested pair is too large: onboarding says
/// so, suggests neither model, and a model the person names anyway is not
/// pulled on Enter; the step left says it is too large (as `doctor` would)
/// and how to choose again, never a bare `ollama pull`.
#[cfg(unix)]
#[tokio::test]
async fn a_12_gb_computer_is_not_offered_models_that_do_not_fit() {
    let shims = Shims::new();
    ollama_shim(&shims);
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    let facts = facts(
        ubuntu(),
        12,
        no_podman(),
        &shims,
        &ollama.url(),
        OllamaService::Systemd,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::scripted(vec![
            Answer::Pick(4), // local models for both
            Answer::Text("qwen3-coder:30b".into()),
            Answer::Text("gpt-oss:20b".into()),
            Answer::Enter, // the pull question
        ]),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            ..OnboardArgs::default()
        },
    );
    let mut choice = Choice::default();
    let switched = ollama::writer_and_reviewer(&mut o, &mut choice).await;
    let output = o.ui.output();
    assert_eq!(switched, None);
    assert!(
        output.contains("both are too large for this computer's 12 GB"),
        "{output}"
    );
    assert!(!output.contains("Suggested: writer"), "{output}");
    assert!(
        output.contains(
            "Not suggested: qwen3-coder:30b (18.6 GB) and gpt-oss:20b (13.8 GB) are too large \
             for this computer's 12 GB"
        ),
        "{output}"
    );
    assert!(
        output.contains("→ Local models for both (not tested at 12 GB)"),
        "{output}"
    );
    assert!(
        output.contains("are too large for this computer's 12 GB of memory")
            && output.contains("[y/N] Enter"),
        "the pull question names the memory and defaults to no: {output}"
    );
    assert!(
        !shims
            .calls()
            .iter()
            .any(|call| call.starts_with("ollama pull")),
        "{:?}",
        shims.calls()
    );
    assert_eq!(o.steps.len(), 2, "{:?}", o.steps);
    // This run writes the team, whose Agents run the model: `--replace`
    // (`--update` would keep them), with the writer named so Step 1 is not
    // asked again.
    assert_eq!(
        o.steps[0],
        "qwen3-coder:30b (18.6 GB) is too large for this computer's 12 GB, so it was not \
         pulled: choose a smaller model with `axocoatl onboard --replace --writer ollama`, or a \
         hosted writer and reviewer with `axocoatl onboard --replace --writer openrouter` (or \
         `--writer claude-code`, or `--writer codex`) (--replace backs up the configuration and \
         writes the team for the new models; --update would keep the Agents that run \
         qwen3-coder:30b)",
        "{:?}",
        o.steps
    );
    assert!(
        !o.steps.iter().any(|step| step.starts_with("Pull ")),
        "{:?}",
        o.steps
    );
}

/// I3. The low-memory menu lists the hosted writers, and choosing Claude
/// Code there sets Claude Code up (its reviewer defaults to OpenRouter
/// here). With no OpenRouter key nothing is saved, and the end says so.
#[cfg(unix)]
#[tokio::test]
async fn the_low_memory_menu_hands_the_writing_to_claude_code() {
    let shims = Shims::new();
    let facts = facts(
        ubuntu(),
        12,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let missing = format!("AXOCOATL_ONBOARD_TEST_NO_KEY_CC_{}", std::process::id());
    let mut o = onboarding(
        &shims,
        Ui::scripted(vec![
            Answer::Pick(2), // a local model writes
            Answer::Pick(1), // no tested pair fits: Claude Code writes
            Answer::Enter,   // the reviewer: OpenRouter, the default here
            Answer::Yes,     // credits, no BYOK
            Answer::Enter,   // the reviewer model
        ]),
        facts,
        OnboardArgs {
            podman: Some(PodmanChoice::Skip),
            openrouter_key_env: Some(missing.clone()),
            start: Some(StartChoice::None),
            ..OnboardArgs::default()
        },
    );
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::NOT_READY);
    assert!(
        output.contains("? No local pair has been tested on 12 GB of memory. Who writes the code, and who reviews it? → Claude Code writes"),
        "{output}"
    );
    assert!(output.contains("\nStep 3 · Claude Code"), "{output}");
    assert!(
        output.contains("? Choose the reviewer → OpenRouter"),
        "{output}"
    );
    assert!(!output.contains("Step 4 · The configuration"), "{output}");
    assert!(
        output.contains("Nothing was saved: no configuration was written."),
        "{output}"
    );
    assert!(!output.contains("keeps what is already set up"), "{output}");
    assert!(!o.paths.config_path.exists());
    // The step left repeats the writer chosen, not only the reviewer:
    // without --writer, running it again would ask Step 1 again.
    assert_eq!(
        o.steps.last().cloned(),
        Some(format!(
            "Export {missing} with your OpenRouter key in this shell, then run `axocoatl onboard \
             --writer claude-code --reviewer openrouter:openai/gpt-oss-120b --openrouter-key-env \
             {missing}`"
        )),
        "{:?}",
        o.steps
    );
}

/// I4. OpenRouter with no key: BYOK is explained before the question, the
/// key prompt says where keys and credits are, no empty Step 4 heading is
/// printed, the step left names the models chosen, and the end says that
/// nothing was saved.
#[cfg(unix)]
#[tokio::test]
async fn openrouter_without_a_key_says_where_to_get_one_and_saves_nothing() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    let report = podman_report(&shims).await;
    let facts = facts(
        mac("26.6"),
        16,
        report,
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let mut answers = vec![
        Answer::Pick(3), // OpenRouter writes
        Answer::Enter,   // credits, no BYOK
        Answer::Enter,   // the writer model
        Answer::Enter,   // the reviewer model
    ];
    if std::env::var("OPENROUTER_API_KEY").is_ok_and(|value| !value.trim().is_empty()) {
        answers.push(Answer::No);
    }
    answers.push(Answer::Pick(2)); // the hidden prompt
    answers.push(Answer::Text(String::new())); // no key
    let mut o = onboarding(
        &shims,
        Ui::scripted(answers),
        facts,
        OnboardArgs {
            podman: Some(PodmanChoice::Skip),
            start: Some(StartChoice::None),
            ..OnboardArgs::default()
        },
    );
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::NOT_READY);
    assert!(
        output.contains("BYOK (\"bring your own key\") is another provider's API key"),
        "{output}"
    );
    assert!(
        output.contains("Create one at https://openrouter.ai/settings/keys"),
        "{output}"
    );
    assert!(!output.contains("Step 4"), "{output}");
    assert_eq!(o.steps.len(), 1, "{:?}", o.steps);
    assert_eq!(
        o.steps[0],
        "Run `axocoatl onboard --writer openrouter --writer-model openrouter:qwen/qwen3-coder \
         --reviewer openrouter:openai/gpt-oss-120b` and sign in to OpenRouter in your browser \
         when it asks (add credits at https://openrouter.ai/settings/credits), or create a key \
         at https://openrouter.ai/settings/keys and enter it at its hidden prompt"
    );
    // The browser sign-in is offered first.
    assert!(
        output.contains(
            "? How should Axocoatl get your OpenRouter key? → Enter it at a hidden prompt"
        ),
        "{output}"
    );
    assert!(
        output.contains("Nothing was saved: no configuration was written."),
        "{output}"
    );
    assert!(!output.contains("keeps what is already set up"), "{output}");
    assert!(!o.paths.config_path.exists());
}

/// I7b and I7e. A new configuration is shown before it is written, and
/// Step 5 does not load again the models Step 3 loaded.
#[cfg(unix)]
#[tokio::test]
async fn step_5_reuses_step_3_and_a_new_configuration_is_shown() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    ollama_shim(&shims);
    let ollama = FakeOllama::start(
        "0.40.6",
        shims.home.path(),
        shims.dir.path(),
        &["qwen3-coder:30b", "gpt-oss:20b"],
    )
    .await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    std::fs::write(shims.dir.path().join("big-context"), "").unwrap();
    let report = podman_report(&shims).await;
    let facts = facts(
        mac("26.6"),
        48,
        report,
        &shims,
        &ollama.url(),
        OllamaService::Brew,
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            ollama_url: Some(ollama.url()),
            ..yes(WriterChoice::Ollama)
        },
    );
    let code = run(&mut o).await;
    let output = o.ui.output();
    assert_eq!(code, exit::READY, "{output}\n{:?}", o.steps);
    for model in ["qwen3-coder:30b", "gpt-oss:20b"] {
        assert_eq!(
            loads(&ollama, model).await,
            1,
            "{model} loaded once: {output}"
        );
    }
    assert!(
        output.contains(&format!(
            "  The new configuration sets:\n    `axocoatl run fix`: writer \
             ollama:qwen3-coder:30b, reviewer ollama:gpt-oss:20b\n    Workbench Sessions: Lead, \
             Scout and the chat Assistant on ollama:qwen3-coder:30b, Reviewer on \
             ollama:gpt-oss:20b\n    Ollama server: {}\n    Workbench port: {}\n  ✓ \
             Configuration written",
            ollama.url(),
            o.args.port.unwrap()
        )),
        "{output}"
    );
    // The optional writers that are not set up are not failures.
    assert!(
        output.contains("  · axocoatl run fix-claude-code (not set up"),
        "{output}"
    );
    assert!(!output.contains("✗ axocoatl run fix-codex"), "{output}");
}

/// I3. A model an earlier setup chose that is too large for this computer
/// is asked again at the terminal (doctor's fix for it is `axocoatl onboard
/// --update`); with --yes it is kept, with a warning.
#[cfg(unix)]
#[tokio::test]
async fn update_asks_again_for_a_kept_model_too_large_for_this_computer() {
    let shims = Shims::new();
    let ollama = FakeOllama::start(
        "0.40.6",
        shims.home.path(),
        shims.dir.path(),
        &["qwen3-coder:30b", "gpt-oss:20b"],
    )
    .await;
    for interactive in [true, false] {
        let facts = facts(
            ubuntu(),
            12,
            no_podman(),
            &shims,
            &ollama.url(),
            OllamaService::Systemd,
        )
        .await;
        let ui = if interactive {
            Ui::scripted(Vec::new())
        } else {
            Ui::new(Mode::Yes, false, None)
        };
        let mut o = onboarding(&shims, ui, facts, OnboardArgs::default());
        o.kept = Kept {
            writer: Some(WriterChoice::Ollama),
            writer_model: Some("ollama:qwen3-coder:30b".into()),
            reviewer: Some("ollama:gpt-oss:20b".into()),
            ..Kept::default()
        };
        keep_models(&mut o, WriterChoice::Ollama);
        let output = o.ui.output();
        if interactive {
            assert_eq!(o.args.writer_model, None, "{output}");
            assert_eq!(o.args.reviewer, None, "{output}");
            assert!(
                output.contains(
                    "Writer model in your configuration: ollama:qwen3-coder:30b, too large for \
                     this computer's 12 GB; choose it again below"
                ),
                "{output}"
            );
        } else {
            assert_eq!(
                o.args.writer_model.as_deref(),
                Some("ollama:qwen3-coder:30b")
            );
            assert!(
                output.contains(
                    "! Writer model: ollama:qwen3-coder:30b (kept from your configuration; too \
                     large for this computer's 12 GB)"
                ),
                "{output}"
            );
        }
    }
}

/// I3. With --yes, a model too large for this computer's memory is not
/// pulled unless --pull says so; the steps say why.
#[cfg(unix)]
#[tokio::test]
async fn yes_never_pulls_a_model_too_large_for_the_memory() {
    let shims = Shims::new();
    ollama_shim(&shims);
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    for pull in [false, true] {
        let facts = facts(
            ubuntu(),
            16,
            no_podman(),
            &shims,
            &ollama.url(),
            OllamaService::Systemd,
        )
        .await;
        let mut o = onboarding(
            &shims,
            Ui::new(Mode::Yes, false, None),
            facts,
            OnboardArgs {
                ollama_url: Some(ollama.url()),
                writer_model: Some("ollama:qwen3-coder:30b".into()),
                reviewer: Some("ollama:gpt-oss:20b".into()),
                pull,
                ..yes(WriterChoice::Ollama)
            },
        );
        let mut choice = Choice::default();
        ollama::writer_and_reviewer(&mut o, &mut choice).await;
        let pulled = shims
            .calls()
            .iter()
            .filter(|call| call.starts_with("ollama pull"))
            .count();
        if pull {
            assert_eq!(pulled, 2, "--pull pulls them anyway");
        } else {
            assert_eq!(pulled, 0);
            assert!(
                o.ui.output().contains(
                    "no: it does not fit this computer's memory (--pull pulls it anyway)"
                ),
                "{}",
                o.ui.output()
            );
            assert!(
                o.steps
                    .iter()
                    .all(|step| step.contains("is too large for this computer's 16 GB")),
                "{:?}",
                o.steps
            );
        }
    }
}

/// I7f. Ollama's Linux installer warns that it found no GPU: onboarding
/// says what that means for speed.
#[cfg(unix)]
#[tokio::test]
async fn the_installers_cpu_only_warning_is_explained() {
    // What the installer prints (install.sh's `warning` and `status`), and
    // what onboarding then says.
    for (said, note) in [
        (
            "WARNING: No NVIDIA/AMD GPU detected. Ollama will run in CPU-only mode.",
            Some(ollama::CPU_ONLY),
        ),
        (
            "WARNING: Unable to detect NVIDIA/AMD GPU. Install lspci or lshw to automatically \
             detect and install GPU dependencies.",
            Some(ollama::GPU_UNKNOWN),
        ),
        (">>> NVIDIA GPU installed.", None),
    ] {
        let shims = Shims::new();
        shims.add("zstd", "exit 0");
        shims.add("sh", "exec /bin/sh \"$@\"");
        let warning = said;
        shims.add(
            "curl",
            &format!("printf '%s\\n' 'echo \">>> Install complete.\"' 'echo \"{warning}\"'"),
        );
        let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
        std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
        let mut facts = facts(
            ubuntu(),
            64,
            no_podman(),
            &shims,
            &ollama.url(),
            OllamaService::Systemd,
        )
        .await;
        facts.ollama_cli = None;
        facts.ollama_server = OllamaServer::Unreachable("not installed".into());
        let mut o = onboarding(
            &shims,
            Ui::new(Mode::Yes, true, None),
            facts,
            OnboardArgs {
                ollama_url: Some(ollama.url()),
                allow_sudo: true,
                ..yes(WriterChoice::Ollama)
            },
        );
        assert_eq!(
            ollama::server(&mut o).await,
            Some(ollama.url()),
            "{}",
            o.ui.output()
        );
        let output = o.ui.output();
        assert!(
            shims
                .calls()
                .iter()
                .any(|call| call.starts_with("curl -fsSL")),
            "{:?}",
            shims.calls()
        );
        for possible in [ollama::CPU_ONLY, ollama::GPU_UNKNOWN] {
            assert_eq!(
                output.contains(possible),
                note == Some(possible),
                "{said}: {output}"
            );
        }
    }
}

/// The interactive path can name an Ollama server at another address on
/// this computer when nothing answers at the default one, and refuses one
/// that is not on this computer.
#[cfg(unix)]
#[tokio::test]
async fn an_ollama_at_another_address_can_be_entered() {
    let shims = Shims::new();
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    let facts = facts(
        mac("26.6"),
        48,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    let address = ollama.url().trim_start_matches("http://").to_string();
    let mut o = onboarding(
        &shims,
        Ui::scripted(vec![
            Answer::Pick(1), // it runs at another address
            Answer::Text("http://ollama.example.com:11434".into()),
            Answer::Text(address),
        ]),
        facts,
        OnboardArgs::default(),
    );
    assert_eq!(ollama::server(&mut o).await, Some(ollama.url()));
    let output = o.ui.output();
    assert!(
        output.contains("native Sessions use only an Ollama server on this computer"),
        "{output}"
    );
    assert!(
        output.contains(&format!("Ollama answers at {}", ollama.url())),
        "{output}"
    );
    assert_eq!(o.facts.ollama_url, ollama.url());
}

/// I3, doctor's side: a configured model that is not pulled and is too
/// large for this computer gets a smaller model or OpenRouter as its fix,
/// never `ollama pull`; on a computer it fits, the pull.
#[cfg(unix)]
#[tokio::test]
async fn doctor_does_not_tell_a_small_computer_to_pull_a_model_too_large_for_it() {
    let shims = Shims::new();
    let ollama = FakeOllama::start("0.40.6", shims.home.path(), shims.dir.path(), &[]).await;
    std::fs::write(shims.dir.path().join("no-cloud"), "").unwrap();
    let config_path = shims.home.path().join("config.yaml");
    let text = config::render(&config::ConfigPlan {
        ollama_base_url: Some(ollama.url()),
        openrouter_key: None,
        lead: config::ModelRef::new(config::Provider::Ollama, "qwen3-coder:30b"),
        reviewer: config::ModelRef::new(config::Provider::Ollama, "gpt-oss:20b"),
        run_writer: Some(config::ModelRef::new(
            config::Provider::Ollama,
            "qwen3-coder:30b",
        )),
        run_reviewer: Some(config::ModelRef::new(
            config::Provider::Ollama,
            "gpt-oss:20b",
        )),
        port: free_port(),
        note: "local Ollama setup".into(),
        writer: "ollama",
        codex_credential: None,
    });
    std::fs::write(&config_path, text).unwrap();
    for gb in [12, 48] {
        let mut env = doctor_env(&shims);
        env.memory_bytes = Some(gb * GIB);
        env.default_config = config_path.clone();
        let report = crate::doctor::collect(&config_path, &env).await;
        let item = report
            .sections
            .models
            .iter()
            .find(|item| item.name == "Model qwen3-coder:30b")
            .unwrap_or_else(|| panic!("{:?}", report.sections.models));
        assert_eq!(item.status, crate::doctor::Status::Fail);
        let fix = item.fix.clone().unwrap_or_default();
        if gb == 12 {
            assert_eq!(
                item.detail,
                "qwen3-coder:30b is not pulled, and at 18.6 GB it is too large for this \
                 computer's 12 GB"
            );
            // The Agents onboarding wrote run it, and `--update` keeps
            // Agents: `--replace` is the command that moves them.
            assert_eq!(
                fix,
                "Choose a smaller model (`axocoatl onboard --replace --writer ollama`), or use \
                 OpenRouter for it (`axocoatl onboard --replace --writer openrouter`) (`--update` \
                 would keep the Agents that run qwen3-coder:30b; `--replace` backs this \
                 configuration up first)"
            );
            assert!(!fix.contains("ollama pull"), "{fix}");
        } else {
            assert_eq!(item.detail, "qwen3-coder:30b is not pulled");
            assert!(
                fix.contains("ollama pull qwen3-coder:30b (18.6 GB)"),
                "{fix}"
            );
        }
    }
}
