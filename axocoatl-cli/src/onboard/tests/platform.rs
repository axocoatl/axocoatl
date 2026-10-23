//! What onboarding says about the platform and the login it runs in: a
//! platform the release gate does not verify and a proxy Sessions do not
//! use, at Step 0, and the Always-On Service, which a login without a
//! systemd user session cannot run.

use super::*;

/// An Ubuntu 24.04 whose systemd runs, as onboarding found it, with
/// `user_systemd` for this login.
async fn ubuntu_login(shims: &Shims, user_systemd: detect::UserSystemd) -> Facts {
    let mut facts = facts(
        ubuntu(),
        64,
        no_podman(),
        shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    facts.user_systemd = Some(user_systemd);
    facts
}

fn no_session() -> detect::UserSystemd {
    detect::UserSystemd {
        reachable: false,
        state: "Failed to connect to bus: No medium found".into(),
        linger: None,
    }
}

/// After `su -` there is no user bus: Step 6 offers no Always-On Service,
/// says how to get a session, and `--start service` leaves that as a step
/// instead of a service install that would fail.
#[tokio::test]
async fn the_service_needs_a_systemd_user_session() {
    let shims = Shims::new();
    let facts = ubuntu_login(&shims, no_session()).await;
    let mut o = onboarding(
        &shims,
        Ui::scripted(vec![ui::Answer::Pick(1)]),
        facts.clone(),
        OnboardArgs::default(),
    );
    assert_eq!(choose_start(&mut o), StartChoice::None);
    let output = o.ui.output();
    assert!(
        output.contains(
            "Always on, starting at login (not available: this login has no systemd user session)"
        ),
        "{output}"
    );
    assert!(
        output.contains("`sudo loginctl enable-linger alex`")
            && output.contains("Until then, start Axocoatl with `axocoatl dev`"),
        "{output}"
    );

    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        OnboardArgs {
            start: Some(StartChoice::Service),
            ..OnboardArgs::default()
        },
    );
    assert_eq!(start_service(&mut o).await, None);
    assert_eq!(o.steps.len(), 1);
    assert!(
        o.steps[0].starts_with(
            "The Always-On Service cannot run here: this login has no systemd user session. Log \
             in as alex directly"
        ),
        "{:?}",
        o.steps
    );
    assert!(shims.calls().is_empty(), "{:?}", shims.calls());

    // With a session, the service is offered as before.
    let facts = ubuntu_login(
        &shims,
        detect::UserSystemd {
            reachable: true,
            state: "running".into(),
            linger: Some(false),
        },
    )
    .await;
    let mut o = onboarding(
        &shims,
        Ui::scripted(vec![ui::Answer::Pick(1)]),
        facts,
        OnboardArgs::default(),
    );
    assert_eq!(choose_start(&mut o), StartChoice::Service);
}

/// Step 0 names a platform not verified yet and a proxy Sessions and runs
/// do not use; Step 5's check, which has the same lines, does not repeat
/// them.
#[tokio::test]
async fn step_0_names_an_unverified_platform_and_an_unused_proxy() {
    let shims = Shims::new();
    let mut facts = facts(
        Os::Linux {
            release: OsRelease::parse(
                "ID=debian\nVERSION_ID=\"12\"\nPRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\n",
            ),
            wsl: None,
            systemd: true,
        },
        64,
        no_podman(),
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    facts.selinux_enforcing = true;
    facts.proxy_variables = vec!["HTTPS_PROXY".into()];
    let notes: Vec<String> = step_0_notes(&facts)
        .iter()
        .map(|item| item.line())
        .collect();
    assert_eq!(
        notes,
        [
            "Platform: Debian GNU/Linux 12 (bookworm): Axocoatl has not been verified on it yet \
             (Ubuntu 24.04 is)",
            "Platform: SELinux is enforcing: Sessions under SELinux enforcing have not been \
             verified yet",
            "Proxy: HTTPS_PROXY is set, but Axocoatl does not support a proxy for Sessions and \
             runs: a Session's network (a run's dependency install, web tools, Claude Code's and \
             Codex's model calls) connects directly, and its containers do not get the variables; \
             only the daemon's own model calls to OpenRouter or another hosted provider follow \
             them, untested behind a proxy",
        ]
    );
    // Apple silicon and nothing set: nothing to say.
    let shims = Shims::new();
    let facts = facts_on_a_mac(&shims).await;
    assert!(step_0_notes(&facts).is_empty());
}

async fn facts_on_a_mac(shims: &Shims) -> Facts {
    facts(
        mac("26.6"),
        48,
        no_podman(),
        shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await
}

/// Step 5 leaves out the platform and proxy lines Step 0 said.
#[cfg(unix)]
#[tokio::test]
async fn the_check_does_not_repeat_step_0() {
    let shims = Shims::new();
    podman_shim(&shims);
    podman_state(&shims, &["machine", "running"]);
    let report = podman_report(&shims).await;
    let mut facts = facts(
        Os::MacOs {
            version: Some("26.6".into()),
            apple_silicon: false,
            chip: None,
        },
        48,
        report,
        &shims,
        "http://127.0.0.1:9",
        OllamaService::Manual,
    )
    .await;
    facts.proxy_variables = vec!["https_proxy".into()];
    let mut o = onboarding(
        &shims,
        Ui::new(Mode::Yes, false, None),
        facts,
        yes(WriterChoice::Openrouter),
    );
    std::fs::create_dir_all(&o.paths.config_dir).unwrap();
    std::fs::write(&o.paths.config_path, "agents: []\n").unwrap();
    check(&mut o, &Default::default()).await;
    let output = o.ui.output();
    assert!(!output.contains("Intel Mac"), "{output}");
    assert!(!output.contains("https_proxy"), "{output}");
}

/// `onboard --help` says that `--podman create` refuses Ubuntu 22.04.
#[test]
fn onboard_help_names_the_ubuntu_22_04_refusal() {
    use clap::CommandFactory;
    let mut cli = crate::Cli::command();
    let help = cli
        .find_subcommand_mut("onboard")
        .unwrap()
        .render_long_help()
        .to_string();
    assert!(
        help.contains("Ubuntu 22.04 is refused, its Podman 3.4.4 is too old"),
        "{help}"
    );
    assert!(help.contains("whose Podman 3.4.4 is too old"), "{help}");
}
