//! `axocoatl connect openrouter` end to end against a fake OpenRouter
//! (wiremock serving `/api/v1/models`, `/api/v1/auth/keys` and
//! `/api/v1/key`) and a fake browser that reads the sign-in page's address
//! and returns to the loopback server the way OpenRouter's redirect does.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::openrouter::*;
use super::openrouter_api::{Api, ApiKey};
use super::openrouter_pkce::challenge_for;
use super::{exit, Failure, Interrupt};

/// A key of OpenRouter's shape, unique to these tests.
const KEY: &str = "sk-or-v1-0f1e2d3c4b5a69788796a5b4c3d2e1f0TESTKEYfedcba9876543210ab";
const CODE: &str = "the-authorization-code";

fn model(id: &str, tools: bool, output: &[&str], prompt: &str, completion: &str) -> Value {
    let parameters: Vec<&str> = if tools {
        vec!["max_tokens", "tools", "tool_choice"]
    } else {
        vec!["max_tokens"]
    };
    json!({
        "id": id,
        "context_length": 262144,
        "supported_parameters": parameters,
        "architecture": {"input_modalities": ["text"], "output_modalities": output},
        "pricing": {"prompt": prompt, "completion": completion},
    })
}

fn catalog() -> Value {
    json!({"data": [
        model("qwen/qwen3-coder", true, &["text"], "0.00000022", "0.00000095"),
        model("openai/gpt-oss-120b", true, &["text"], "0.00000005", "0.00000025"),
        model("vendor/no-tools", false, &["text"], "0", "0"),
        model("vendor/draws", true, &["text", "image"], "0", "0"),
        model("vendor/gratis:free", true, &["text"], "0", "0"),
    ]})
}

fn key_answer() -> Value {
    json!({"data": {
        "label": "Axocoatl 2026-10-19",
        "limit": 10.0,
        "limit_remaining": 7.5,
        "limit_reset": "monthly",
        "usage": 2.5,
        "usage_daily": 0.0123,
        "is_free_tier": false,
        "free_model_daily_requests": {"used": 3, "limit": 1000, "remaining": 997},
    }})
}

/// A fake OpenRouter: the catalog, the exchange and the key's description.
async fn openrouter(exchange: ResponseTemplate, key: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(catalog()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/auth/keys"))
        .and(header("content-type", "application/json"))
        .respond_with(exchange)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/key"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(key)
        .mount(&server)
        .await;
    server
}

fn issued() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"key": KEY, "user_id": "user_1"}))
}

fn described() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(key_answer())
}

fn endpoints(server: &MockServer) -> Endpoints {
    Endpoints {
        auth_page: format!("{}/auth", server.uri()),
        api: Api::for_test(&format!("{}/api/v1", server.uri()), Duration::from_secs(5)),
    }
}

/// What the fake browser saw: the sign-in page's address.
#[derive(Default)]
struct Seen {
    address: Mutex<Option<String>>,
}

impl Seen {
    fn query(&self, name: &str) -> Option<String> {
        let address = self.address.lock().unwrap().clone()?;
        reqwest::Url::parse(&address)
            .ok()?
            .query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }

    fn opened(&self) -> bool {
        self.address.lock().unwrap().is_some()
    }
}

/// A browser that, given the sign-in page, returns to its callback with
/// the query `answer(state)` (`None`: it never returns).
fn browser(
    answer: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> (Browser, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let browser = Browser::Fake(Box::new({
        let seen = seen.clone();
        move |address: String| {
            *seen.address.lock().unwrap() = Some(address.clone());
            let url = reqwest::Url::parse(&address).unwrap();
            let get = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
                    .unwrap()
            };
            let (callback, state) = (get("callback_url"), get("state"));
            if let Some(query) = answer(&state) {
                tokio::spawn(async move {
                    super::openrouter_pkce::tests::browse(&format!("{callback}?{query}")).await;
                });
            }
        }
    }));
    (browser, seen)
}

/// A browser that signs in and returns OpenRouter's code with the state.
fn signing_in() -> (Browser, Arc<Seen>) {
    browser(|state| Some(format!("code={CODE}&state={state}")))
}

fn data_root() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    axocoatl_daemon::AxocoatlDaemon::initialize_data_root(directory.path()).unwrap();
    directory
}

fn stored(data_dir: &Path, name: &str) -> Option<String> {
    axocoatl_daemon::secret_store::lock_secrets(data_dir, false)
        .unwrap()
        .and_then(|lock| lock.read(name).unwrap())
        .map(|value| String::from_utf8(value.to_vec()).unwrap())
}

fn request(
    data_dir: &Path,
    models: &[(&str, &str)],
    browser: Browser,
    endpoints: Endpoints,
    sign_in_time: Duration,
) -> SignIn {
    SignIn {
        data_dir: data_dir.to_path_buf(),
        secret: OPENROUTER_SECRET.to_string(),
        models: models
            .iter()
            .map(|(role, id)| (role.to_string(), id.to_string()))
            .collect(),
        browser,
        sign_in_time,
        endpoints,
    }
}

/// Neither the output, the result nor a failure holds the key or the
/// code.
fn assert_never_shown(out: &[u8], shown: &str) {
    let out = String::from_utf8_lossy(out);
    for secret in [KEY, "TESTKEY", CODE] {
        assert!(!out.contains(secret), "output shows {secret}: {out}");
        assert!(!shown.contains(secret), "result shows {secret}: {shown}");
    }
}

async fn exchange_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/api/v1/auth/keys")
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

#[tokio::test]
async fn a_browser_sign_in_stores_the_key_it_exchanges_and_shows_only_what_openrouter_says() {
    let server = openrouter(issued(), described()).await;
    let root = data_root();
    let (browser, seen) = signing_in();
    let mut out = Vec::new();
    let connected = sign_in(
        request(
            root.path(),
            &[
                ("writer", "qwen/qwen3-coder"),
                ("reviewer", "openai/gpt-oss-120b"),
            ],
            browser,
            endpoints(&server),
            Duration::from_secs(30),
        ),
        &mut out,
        &Interrupt::default(),
    )
    .await
    .unwrap();

    // The key is the secret, and nothing else holds it.
    assert_eq!(stored(root.path(), OPENROUTER_SECRET).as_deref(), Some(KEY));
    let lines = connected_lines(&connected).join("\n");
    assert_never_shown(&out, &format!("{connected:?}\n{lines}"));
    // The sign-in page: OpenRouter's, with a loopback callback, S256 and a
    // state; the address was printed too.
    let output = String::from_utf8_lossy(&out);
    let address = seen.address.lock().unwrap().clone().unwrap();
    assert!(address.starts_with(&format!("{}/auth?", server.uri())));
    assert!(output.contains(&address), "{output}");
    let callback = seen.query("callback_url").unwrap();
    assert!(callback.starts_with("http://127.0.0.1:"), "{callback}");
    assert!(callback.ends_with("/callback"), "{callback}");
    assert_eq!(seen.query("code_challenge_method").as_deref(), Some("S256"));
    assert!(seen.query("key_label").unwrap().starts_with("Axocoatl "));
    // The exchange sent the code and the verifier whose S256 challenge
    // the page carried.
    let bodies = exchange_bodies(&server).await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["code"], CODE);
    assert_eq!(bodies[0]["code_challenge_method"], "S256");
    let verifier = bodies[0]["code_verifier"].as_str().unwrap();
    assert_eq!(verifier.len(), 43);
    assert_eq!(
        seen.query("code_challenge").unwrap(),
        challenge_for(verifier)
    );
    assert!(!address.contains(verifier));
    // What OpenRouter says about the key, and the models.
    let info = connected.key.as_ref().unwrap();
    assert_eq!(info.label.as_deref(), Some("Axocoatl 2026-10-19"));
    assert_eq!(connected.key_unread, None);
    for line in [
        "Connected OpenRouter: stored the key as secret openrouter (it was never shown)",
        "Key label: Axocoatl 2026-10-19",
        "Credit limit: $10.00 on this key, $7.50 left (resets monthly)",
        "Used by this key: $2.50 in all, $0.0123 today (UTC)",
        "Free tier: no (the account has bought credits)",
        "writer qwen/qwen3-coder: 262,144 tokens of context, $0.22 in / $0.95 out per million \
         tokens",
        "reviewer openai/gpt-oss-120b: 262,144 tokens of context, $0.05 in / $0.25 out per \
         million tokens",
    ] {
        assert!(lines.contains(line), "{line}\n{lines}");
    }
    assert!(output.contains("✓ writer qwen/qwen3-coder"), "{output}");
}

#[tokio::test]
async fn a_free_tier_account_and_a_key_without_a_limit_are_said_plainly() {
    let mut answer = key_answer();
    answer["data"]["limit"] = Value::Null;
    answer["data"]["limit_remaining"] = Value::Null;
    answer["data"]["is_free_tier"] = json!(true);
    answer["data"]["free_model_daily_requests"] = json!({"used": 3, "limit": 50});
    let server = openrouter(issued(), ResponseTemplate::new(200).set_body_json(answer)).await;
    let root = data_root();
    let (browser, _) = signing_in();
    let connected = sign_in(
        request(
            root.path(),
            &[],
            browser,
            endpoints(&server),
            Duration::from_secs(30),
        ),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap();
    let lines = connected_lines(&connected).join("\n");
    assert!(
        lines.contains("Credit limit: none on this key (the account's credits apply)"),
        "{lines}"
    );
    assert!(
        lines.contains(
            "Free tier: yes, this account has not bought credits. Requests to paid models fail \
             until you add credits at https://openrouter.ai/settings/credits; `:free` models \
             have a daily request limit (3 of 50 used today)."
        ),
        "{lines}"
    );
    // No models were named: the catalog was not read.
    assert!(!server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .any(|request| request.url.path() == "/api/v1/models"));
}

#[tokio::test]
async fn a_refused_exchange_stores_nothing_and_shows_openrouters_reason_not_the_code() {
    let server = openrouter(
        ResponseTemplate::new(403).set_body_json(
            json!({"error": {"code": 403, "message": "Invalid code or code_verifier"}}),
        ),
        described(),
    )
    .await;
    let root = data_root();
    let (browser, _) = signing_in();
    let mut out = Vec::new();
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[],
            browser,
            endpoints(&server),
            Duration::from_secs(30),
        ),
        &mut out,
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code, exit::REJECTED);
    assert!(
        message.contains("OpenRouter answered HTTP 403: Invalid code or code_verifier"),
        "{message}"
    );
    assert!(message.contains("nothing was stored"), "{message}");
    assert_never_shown(&out, &message);
    assert_eq!(stored(root.path(), OPENROUTER_SECRET), None);
}

#[tokio::test]
async fn an_exchange_answer_that_echoes_the_code_is_not_shown() {
    let server = openrouter(
        ResponseTemplate::new(400)
            .set_body_json(json!({"error": {"message": format!("bad code {CODE}")}})),
        described(),
    )
    .await;
    let root = data_root();
    let (browser, _) = signing_in();
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[],
            browser,
            endpoints(&server),
            Duration::from_secs(30),
        ),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code, exit::REJECTED);
    assert!(
        message.contains("OpenRouter answered HTTP 400)"),
        "{message}"
    );
    assert!(!message.contains(CODE), "{message}");
}

#[tokio::test]
async fn a_key_openrouter_then_refuses_is_not_stored() {
    let server = openrouter(issued(), ResponseTemplate::new(401)).await;
    let root = data_root();
    let (browser, _) = signing_in();
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[],
            browser,
            endpoints(&server),
            Duration::from_secs(30),
        ),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code, exit::REJECTED);
    assert!(
        message.contains("OpenRouter refused the key it had just issued (HTTP 401)"),
        "{message}"
    );
    assert_eq!(stored(root.path(), OPENROUTER_SECRET), None);
}

#[tokio::test]
async fn a_key_whose_description_cannot_be_read_is_stored_and_says_so() {
    for (template, said) in [
        (ResponseTemplate::new(503), "answered HTTP 503"),
        (
            ResponseTemplate::new(200).set_body_string("not json"),
            "something other than the key's description",
        ),
        (
            described().set_delay(Duration::from_secs(3)),
            "did not answer within 1 seconds",
        ),
    ] {
        let server = openrouter(issued(), template).await;
        let root = data_root();
        let (browser, _) = signing_in();
        let mut endpoints = endpoints(&server);
        endpoints.api = Api::for_test(&format!("{}/api/v1", server.uri()), Duration::from_secs(1));
        let connected = sign_in(
            request(
                root.path(),
                &[],
                browser,
                endpoints,
                Duration::from_secs(30),
            ),
            &mut Vec::new(),
            &Interrupt::default(),
        )
        .await
        .unwrap();
        assert_eq!(stored(root.path(), OPENROUTER_SECRET).as_deref(), Some(KEY));
        assert_eq!(connected.key, None);
        let unread = connected.key_unread.clone().unwrap();
        assert!(unread.contains(said), "{unread}");
        let lines = connected_lines(&connected).join("\n");
        assert!(lines.contains("--status"), "{lines}");
        assert!(!lines.contains(KEY));
    }
}

#[tokio::test]
async fn an_exchange_that_times_out_or_cannot_connect_stores_nothing() {
    // OpenRouter does not answer in time.
    let server = openrouter(issued().set_delay(Duration::from_secs(3)), described()).await;
    let root = data_root();
    let (browser, _) = signing_in();
    let endpoints = Endpoints {
        auth_page: format!("{}/auth", server.uri()),
        api: Api::for_test(&format!("{}/api/v1", server.uri()), Duration::from_secs(1)),
    };
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[],
            browser,
            endpoints,
            Duration::from_secs(30),
        ),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code, exit::UNVERIFIED);
    assert!(
        message.contains("did not answer within 1 seconds"),
        "{message}"
    );
    assert!(message.contains("Nothing was stored"), "{message}");
    assert_eq!(stored(root.path(), OPENROUTER_SECRET), None);

    // Nothing listens where the API should be.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let (browser, _) = signing_in();
    let endpoints = Endpoints {
        auth_page: "https://openrouter.ai/auth".into(),
        api: Api::for_test(
            &format!("http://127.0.0.1:{port}/api/v1"),
            Duration::from_secs(5),
        ),
    };
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[],
            browser,
            endpoints,
            Duration::from_secs(30),
        ),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code, exit::UNVERIFIED);
    assert!(message.contains("could not be reached"), "{message}");
    assert_eq!(stored(root.path(), OPENROUTER_SECRET), None);
}

#[tokio::test]
async fn a_sign_in_cancelled_in_the_browser_exchanges_nothing() {
    let server = openrouter(issued(), described()).await;
    let root = data_root();
    let (browser, _) = browser(|_| Some("error=access_denied".to_string()));
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[],
            browser,
            endpoints(&server),
            Duration::from_secs(30),
        ),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code, exit::NO_TOKEN);
    assert_eq!(
        message,
        "The OpenRouter sign-in was cancelled in the browser (OpenRouter said: access_denied), \
         so no key was made and nothing was stored. Run `axocoatl connect openrouter` again to \
         retry."
    );
    assert!(exchange_bodies(&server).await.is_empty());
    assert_eq!(stored(root.path(), OPENROUTER_SECRET), None);
}

#[tokio::test]
async fn a_return_with_another_state_exchanges_nothing() {
    let server = openrouter(issued(), described()).await;
    let root = data_root();
    let (browser, _) = browser(|_| Some(format!("code={CODE}&state=someone-elses")));
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[],
            browser,
            endpoints(&server),
            Duration::from_secs(30),
        ),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code, exit::NO_TOKEN);
    assert!(message.contains("its state did not match"), "{message}");
    assert!(exchange_bodies(&server).await.is_empty());
    assert_eq!(stored(root.path(), OPENROUTER_SECRET), None);
}

#[tokio::test]
async fn a_sign_in_that_never_returns_times_out_and_a_signal_stops_it() {
    let server = openrouter(issued(), described()).await;
    let root = data_root();
    let (browser, seen) = browser(|_| None);
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[],
            browser,
            endpoints(&server),
            Duration::from_millis(300),
        ),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert!(seen.opened());
    assert_eq!(code, exit::NO_TOKEN);
    assert!(
        message.starts_with(
            "The OpenRouter sign-in was not completed within 1 minute; nothing was stored."
        ),
        "{message}"
    );
    assert!(message.contains("--sign-in-timeout MINUTES"), "{message}");

    let interrupt = Interrupt::default();
    let browser = Browser::Fake(Box::new({
        let interrupt = interrupt.clone();
        move |_| interrupt.raise()
    }));
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[],
            browser,
            endpoints(&server),
            Duration::from_secs(30),
        ),
        &mut Vec::new(),
        &interrupt,
    )
    .await
    .unwrap_err();
    assert_eq!(code, exit::INTERRUPTED);
    assert_eq!(
        message,
        "interrupted: the OpenRouter sign-in was stopped and nothing was stored"
    );
    assert!(exchange_bodies(&server).await.is_empty());
    assert_eq!(stored(root.path(), OPENROUTER_SECRET), None);
}

#[tokio::test]
async fn models_are_checked_in_the_catalog_before_any_sign_in() {
    let server = openrouter(issued(), described()).await;
    let root = data_root();
    let (browser, seen) = signing_in();
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[
                ("writer", "qwen/qwen3-coder"),
                ("reviewer", "vendor/missing"),
                ("reviewer", "vendor/no-tools"),
                ("reviewer", "vendor/draws"),
            ],
            browser,
            endpoints(&server),
            Duration::from_secs(30),
        ),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code, exit::USAGE);
    for said in [
        "reviewer: vendor/missing is not in OpenRouter's model catalog",
        "reviewer: vendor/no-tools does not support tool calls on OpenRouter",
        "reviewer: vendor/draws does not read and write text only",
        "Nothing was signed in or stored",
    ] {
        assert!(message.contains(said), "{said}: {message}");
    }
    assert!(!message.contains("qwen/qwen3-coder"), "{message}");
    assert!(!seen.opened());
    assert!(exchange_bodies(&server).await.is_empty());

    // A catalog that cannot be read stops it too.
    let down = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&down)
        .await;
    let (browser, seen) = signing_in();
    let Failure { code, message } = sign_in(
        request(
            root.path(),
            &[("reviewer", "openai/gpt-oss-120b")],
            browser,
            endpoints(&down),
            Duration::from_secs(30),
        ),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(code, exit::UNVERIFIED);
    assert!(
        message.contains("could not read OpenRouter's model catalog"),
        "{message}"
    );
    assert!(!seen.opened());
}

#[test]
fn the_chosen_models_are_openrouter_ids_never_repeated_when_they_may_be_a_key() {
    assert_eq!(
        chosen_models(
            Some("openrouter:qwen/qwen3-coder"),
            Some("openai/gpt-oss-120b")
        )
        .unwrap(),
        vec![
            ("writer".to_string(), "qwen/qwen3-coder".to_string()),
            ("reviewer".to_string(), "openai/gpt-oss-120b".to_string()),
        ]
    );
    assert_eq!(chosen_models(None, None).unwrap(), Vec::new());
    let same = chosen_models(
        Some("qwen/qwen3-coder"),
        Some("openrouter:qwen/qwen3-coder"),
    )
    .unwrap_err();
    assert_eq!(same.code, exit::USAGE);
    assert!(same
        .message
        .contains("the reviewer must be a different model"));
    for (given, said) in [
        (KEY, "is not an OpenRouter model id"),
        ("sk-or-v1/abc", "is not an OpenRouter model id"),
        ("ollama:gpt-oss:20b", "is not an OpenRouter model id"),
        ("anthropic:x/y", "names another provider"),
        ("qwen/qwen3-coder:online", "web search"),
        ("qwen/qwen3-coder:nitro", "service tier"),
        ("openrouter/auto", "is not an OpenRouter model id"),
        ("a/b/c", "is not an OpenRouter model id"),
    ] {
        let refused = chosen_models(None, Some(given)).unwrap_err();
        assert_eq!(refused.code, exit::USAGE, "{given}");
        assert!(
            refused.message.contains("--reviewer"),
            "{given}: {}",
            refused.message
        );
        assert!(
            refused.message.contains(said),
            "{given}: {}",
            refused.message
        );
        assert!(!refused.message.contains("TESTKEY"), "{}", refused.message);
    }
    // A `:free` variant is a model of its own.
    assert_eq!(
        chosen_models(Some("vendor/gratis:free"), None).unwrap()[0].1,
        "vendor/gratis:free"
    );
}

const CONFIG: &str = "# Axocoatl — local Ollama setup\nagents: []\n\nproviders:\n  ollama:\n    \
                      base_url: \"http://127.0.0.1:11434\"\n\nserver:\n  port: 8080\n";

fn flags(config: &Path) -> Flags {
    Flags {
        config: config.to_path_buf(),
        secret: OPENROUTER_SECRET.to_string(),
        writer: None,
        reviewer: Some("openai/gpt-oss-120b".into()),
        no_browser: false,
        sign_in_timeout: None,
        credits_only: true,
        no_config: false,
        status: false,
        remove: false,
    }
}

#[tokio::test]
async fn connect_stores_the_key_and_points_the_configuration_at_it() {
    let server = openrouter(issued(), described()).await;
    let root = data_root();
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.yaml");
    std::fs::write(&config, CONFIG).unwrap();
    let (browser, _) = signing_in();
    let flags = flags(&config);
    let models = chosen_models(flags.writer.as_deref(), flags.reviewer.as_deref()).unwrap();
    let mut out = Vec::new();
    let (code, text) = connect_and_configure(
        &flags,
        root.path().to_path_buf(),
        models,
        browser,
        endpoints(&server),
        &mut out,
        &Interrupt::default(),
    )
    .await;
    assert_eq!(code, exit::CONNECTED, "{text}");
    assert_never_shown(&out, &text);
    let written = std::fs::read_to_string(&config).unwrap();
    assert_eq!(
        written,
        CONFIG.replace(
            "    base_url: \"http://127.0.0.1:11434\"\n",
            "    base_url: \"http://127.0.0.1:11434\"\n  openrouter_billing: credits\n  \
             openrouter:\n    api_key_secret: openrouter\n"
        )
    );
    assert!(!written.contains(KEY));
    let loaded = axocoatl_config::parse_config(&written, &config).unwrap();
    assert_eq!(
        loaded
            .providers
            .openrouter
            .unwrap()
            .api_key_secret
            .as_deref(),
        Some("openrouter")
    );
    assert_eq!(stored(root.path(), OPENROUTER_SECRET).as_deref(), Some(KEY));
    for said in [
        "✓ Connected OpenRouter: stored the key as secret openrouter (it was never shown)",
        "providers.openrouter reads the key from secret openrouter, and openrouter_billing: \
         credits",
        "A running Axocoatl reads provider keys when it starts",
        "Next, in your repository, with OpenRouter as the reviewer (fix-codex takes it the \
         same way):\n    axocoatl run fix-claude-code --task \"Describe the bug\" --model \
         reviewer=openrouter:openai/gpt-oss-120b",
    ] {
        assert!(text.contains(said), "{said}\n{text}");
    }

    // Again: the file already says it, and it is left as it is.
    let (browser, _) = signing_in();
    let (code, text) = connect_and_configure(
        &flags,
        root.path().to_path_buf(),
        Vec::new(),
        browser,
        endpoints(&server),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await;
    assert_eq!(code, exit::CONNECTED, "{text}");
    assert!(
        text.contains("already reads the key from secret openrouter"),
        "{text}"
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), written);
}

#[tokio::test]
async fn with_no_config_only_the_key_is_stored_and_the_lines_are_shown() {
    let server = openrouter(issued(), described()).await;
    let root = data_root();
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.yaml");
    std::fs::write(&config, CONFIG).unwrap();
    let (browser, _) = signing_in();
    let mut flags = flags(&config);
    flags.no_config = true;
    flags.writer = Some("qwen/qwen3-coder".into());
    let models = chosen_models(flags.writer.as_deref(), flags.reviewer.as_deref()).unwrap();
    let (code, text) = connect_and_configure(
        &flags,
        root.path().to_path_buf(),
        models,
        browser,
        endpoints(&server),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await;
    assert_eq!(code, exit::CONNECTED, "{text}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), CONFIG);
    assert!(text.contains("api_key_secret: openrouter"), "{text}");
    assert!(
        text.contains(
            "Next, in your repository:\n    axocoatl run fix --task \"Describe the bug\" \
             --model writer=openrouter:qwen/qwen3-coder --model \
             reviewer=openrouter:openai/gpt-oss-120b"
        ),
        "{text}"
    );
    assert_eq!(stored(root.path(), OPENROUTER_SECRET).as_deref(), Some(KEY));
}

#[tokio::test]
async fn status_shows_the_stored_keys_limits_or_why_it_cannot() {
    let root = data_root();
    let server = openrouter(issued(), described()).await;
    let api = Api::for_test(&format!("{}/api/v1", server.uri()), Duration::from_secs(5));
    let (code, lines) = status_lines(root.path(), OPENROUTER_SECRET, &api).await;
    assert_eq!(code, exit::NO_TOKEN);
    assert!(lines[0].contains("No OpenRouter key is stored as secret openrouter"));

    axocoatl_daemon::secret_store::set_secret(root.path(), OPENROUTER_SECRET, KEY.as_bytes())
        .unwrap();
    let (code, lines) = status_lines(root.path(), OPENROUTER_SECRET, &api).await;
    assert_eq!(code, exit::CONNECTED, "{lines:?}");
    let text = lines.join("\n");
    assert!(text.contains("OpenRouter key: stored as secret openrouter (never shown)"));
    assert!(text.contains("Credit limit: $10.00 on this key, $7.50 left (resets monthly)"));
    assert!(!text.contains(KEY));

    let refusing = openrouter(issued(), ResponseTemplate::new(401)).await;
    let api = Api::for_test(
        &format!("{}/api/v1", refusing.uri()),
        Duration::from_secs(5),
    );
    let (code, lines) = status_lines(root.path(), OPENROUTER_SECRET, &api).await;
    assert_eq!(code, exit::REJECTED);
    assert!(lines
        .join("\n")
        .contains("OpenRouter refuses this key (HTTP 401)"));

    let failing = openrouter(issued(), ResponseTemplate::new(500)).await;
    let api = Api::for_test(&format!("{}/api/v1", failing.uri()), Duration::from_secs(5));
    let (code, lines) = status_lines(root.path(), OPENROUTER_SECRET, &api).await;
    assert_eq!(code, exit::UNVERIFIED);
    assert!(lines
        .join("\n")
        .contains("Could not check it with OpenRouter"));
}

#[test]
fn remove_deletes_the_secret_and_the_configurations_reference() {
    let root = data_root();
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.yaml");
    let connected = CONFIG.replace(
        "    base_url: \"http://127.0.0.1:11434\"\n",
        "    base_url: \"http://127.0.0.1:11434\"\n  openrouter_billing: credits\n  \
         openrouter:\n    api_key_secret: openrouter\n",
    );
    std::fs::write(&config, &connected).unwrap();
    axocoatl_daemon::secret_store::set_secret(root.path(), OPENROUTER_SECRET, KEY.as_bytes())
        .unwrap();
    let (code, text) = remove(root.path(), &config, OPENROUTER_SECRET, false);
    assert_eq!(code, exit::CONNECTED, "{text}");
    assert!(text.contains("Removed the stored OpenRouter key (secret openrouter)"));
    assert!(text.contains("https://openrouter.ai/settings/keys"));
    assert_eq!(stored(root.path(), OPENROUTER_SECRET), None);
    let after = std::fs::read_to_string(&config).unwrap();
    assert!(!after.contains("api_key_secret"), "{after}");
    axocoatl_config::parse_config(&after, &config).unwrap();
    // Nothing left to remove.
    let (code, text) = remove(root.path(), &config, OPENROUTER_SECRET, false);
    assert_eq!(code, exit::NO_TOKEN, "{text}");
}

#[test]
fn doctor_names_where_the_key_is_and_how_to_store_it() {
    let root = data_root();
    let credentials = |api_key: &str, secret: Option<&str>| axocoatl_config::ProviderCredentials {
        api_key: api_key.into(),
        api_key_secret: secret.map(str::to_string),
        base_url: None,
        fallback: None,
    };
    assert_eq!(
        doctor_key_line(
            "OpenRouter",
            &credentials("", Some("openrouter")),
            root.path()
        ),
        Some(Err((
            "OpenRouter API key secret openrouter is not stored".to_string(),
            "Run `axocoatl connect openrouter`, which signs you in to OpenRouter in the browser \
             and stores the key it issues without showing it"
                .to_string()
        )))
    );
    axocoatl_daemon::secret_store::set_secret(root.path(), "openrouter", KEY.as_bytes()).unwrap();
    assert_eq!(
        doctor_key_line(
            "OpenRouter",
            &credentials("", Some("openrouter")),
            root.path()
        ),
        Some(Ok(
            "OpenRouter API key present (secret openrouter, never shown)".to_string()
        ))
    );
    // No key at all: OpenRouter's is connected in the browser.
    assert!(matches!(
        doctor_key_line("OpenRouter", &credentials("", None), root.path()),
        Some(Err((line, hint))) if line == "OpenRouter API key not set"
            && hint.contains("axocoatl connect openrouter")
    ));
    // A key in the file, or another provider's: doctor's own check.
    assert_eq!(
        doctor_key_line("OpenRouter", &credentials("sk-or-x", None), root.path()),
        None
    );
    assert_eq!(
        doctor_key_line("Anthropic", &credentials("", None), root.path()),
        None
    );
    assert!(matches!(
        doctor_key_line("Anthropic", &credentials("", Some("ant")), root.path()),
        Some(Err((_, hint))) if hint.contains("axocoatl secret set ant")
    ));
}

#[test]
fn an_api_key_never_shows_in_debug() {
    let key = ApiKey::from_stored(KEY.to_string());
    assert_eq!(format!("{key:?}"), "ApiKey(<hidden>)");
}

#[tokio::test]
async fn onboarding_signs_in_and_stores_the_key_without_touching_a_configuration() {
    let server = openrouter(issued(), described()).await;
    let directory = tempfile::tempdir().unwrap();
    // A data root onboarding has not created yet.
    let data_dir = directory.path().join("data");
    let (browser, _) = signing_in();
    let connected = onboarding_sign_in(
        &data_dir,
        Some("qwen/qwen3-coder"),
        Some("openai/gpt-oss-120b"),
        browser,
        endpoints(&server),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap();
    assert_eq!(connected.secret, OPENROUTER_SECRET);
    assert_eq!(connected.models.len(), 2);
    assert_eq!(stored(&data_dir, OPENROUTER_SECRET).as_deref(), Some(KEY));
    assert_eq!(
        provider_yaml(&connected.secret),
        "  openrouter_billing: credits\n  openrouter:\n    api_key_secret: openrouter\n"
    );
    // The same model twice is refused before anything happens.
    let (browser, seen) = signing_in();
    let refused = onboarding_sign_in(
        &data_dir,
        Some("qwen/qwen3-coder"),
        Some("qwen/qwen3-coder"),
        browser,
        endpoints(&server),
        &mut Vec::new(),
        &Interrupt::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(refused.code, exit::USAGE);
    assert!(!seen.opened());
}

/// A key or another value on `connect openrouter`'s command line is never
/// repeated: clap takes a bare value as the hidden positional (refused at
/// run time with a message that holds none), and its refusals of flags
/// name only the flags.
#[test]
fn a_value_on_the_command_line_is_never_repeated() {
    use clap::Parser as _;
    let argv = |words: &[&str]| -> Vec<std::ffi::OsString> {
        ["axocoatl", "connect", "openrouter"]
            .iter()
            .chain(words)
            .map(std::ffi::OsString::from)
            .collect()
    };
    let cli = crate::Cli::try_parse_from(argv(&[KEY])).unwrap();
    match cli.command {
        crate::Commands::Connect {
            command:
                super::ConnectCommands::Openrouter {
                    on_command_line, ..
                },
        } => assert_eq!(on_command_line, vec![KEY.to_string()]),
        _ => panic!("not connect openrouter"),
    }
    assert!(!VALUE_ON_COMMAND_LINE.contains("sk-or"));
    let with_equals = format!("--credits-only={KEY}");
    let one_argument = format!("--no-browser {KEY}");
    for (words, said) in [
        (
            vec!["--sign-in-timeout", KEY],
            "the value given to --sign-in-timeout",
        ),
        (
            vec!["--sign-in-timeout", "61"],
            "--sign-in-timeout takes 1 to 60 minutes",
        ),
        (vec![with_equals.as_str()], "--credits-only takes no value"),
        (
            vec![one_argument.as_str()],
            "an argument that starts with --no-browser is not one axocoatl connect takes",
        ),
        (vec!["--no-browsr"], "Did you mean --no-browser?"),
    ] {
        let argv = argv(&words);
        let error = crate::Cli::try_parse_from(&argv).err().unwrap();
        let message =
            crate::connect_parse_error(&error, &argv).unwrap_or_else(|| error.render().to_string());
        assert!(message.contains(said), "{words:?}: {message}");
        assert!(!message.contains("TESTKEY"), "{words:?}: {message}");
    }
    // --status and --remove take no sign-in flags.
    for words in [
        vec!["--status", "--writer", "a/b"],
        vec!["--remove", "--status"],
    ] {
        assert!(
            crate::Cli::try_parse_from(argv(&words)).is_err(),
            "{words:?}"
        );
    }
}

#[tokio::test]
async fn the_receipt_key_and_the_chatgpt_sign_in_are_never_replaced_or_removed() {
    let root = data_root();
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.yaml");
    std::fs::write(&config, CONFIG).unwrap();
    for name in ["receipt-signing-key", "codex-chatgpt"] {
        axocoatl_daemon::secret_store::set_secret(root.path(), name, b"kept-value").unwrap();
        for remove in [false, true] {
            let mut flags = flags(&config);
            flags.secret = name.to_string();
            flags.remove = remove;
            assert_eq!(cmd_connect_openrouter(flags).await, exit::USAGE, "{name}");
        }
        assert_eq!(stored(root.path(), name).as_deref(), Some("kept-value"));
    }
    assert_eq!(std::fs::read_to_string(&config).unwrap(), CONFIG);
}

#[tokio::test]
async fn status_and_remove_before_any_data_root_say_nothing_is_stored() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("no-data-yet");
    let api = Api::for_test("http://127.0.0.1:9/api/v1", Duration::from_secs(1));
    let (code, lines) = status_lines(&missing, OPENROUTER_SECRET, &api).await;
    assert_eq!(code, exit::NO_TOKEN, "{lines:?}");
    let (code, text) = remove(
        &missing,
        &directory.path().join("no-config.yaml"),
        OPENROUTER_SECRET,
        false,
    );
    assert_eq!(code, exit::NO_TOKEN, "{text}");
    assert!(!missing.exists());
}
