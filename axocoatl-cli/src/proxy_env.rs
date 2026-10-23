//! Requests to this computer never go through a proxy. reqwest, which every
//! HTTP client in this process uses (rmcp's, for an MCP server over HTTP,
//! too), sends a request through `HTTP_PROXY`, `HTTPS_PROXY` or `ALL_PROXY`
//! (or their lowercase forms) even to `localhost` and `127.0.0.1` unless
//! `NO_PROXY` names the host. A proxy cannot reach this computer's loopback,
//! and it would see such a request, with any key or token it carries, in
//! plain text: a provider whose `base_url` is on this computer (LM Studio, a
//! local OpenAI-compatible server), a webhook or an MCP server there. So when
//! a proxy variable is set, `main` adds this computer to `NO_PROXY` (and
//! `no_proxy`) before anything else runs, after what they already list.
//! Requests to other hosts follow the variables as before.

/// This computer as `NO_PROXY` names it for reqwest: `localhost` and its
/// subdomains, and the loopback addresses.
const THIS_COMPUTER: [&str; 3] = ["localhost", "127.0.0.0/8", "::1"];

/// The variables reqwest takes a proxy from.
const PROXY_VARIABLES: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// The `NO_PROXY` that keeps requests to this computer off a proxy, given
/// the environment `lookup` reads: what `NO_PROXY` lists (or `no_proxy`
/// when `NO_PROXY` is not set, as reqwest reads them), then each name of
/// this computer it does not list. `None` when no proxy variable is set, or
/// when both variables already hold that value.
pub(crate) fn no_proxy(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    if !PROXY_VARIABLES.iter().any(|name| lookup(name).is_some()) {
        return None;
    }
    let upper = lookup("NO_PROXY");
    let lower = lookup("no_proxy");
    let current = upper.clone().or_else(|| lower.clone()).unwrap_or_default();
    let mut entries: Vec<&str> = current
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    for host in THIS_COMPUTER {
        if !entries.contains(&host) {
            entries.push(host);
        }
    }
    let value = entries.join(",");
    (upper.as_deref() != Some(&value) || lower.as_deref() != Some(&value)).then_some(value)
}

/// Sets [`no_proxy`]'s value as `NO_PROXY` and `no_proxy`, which every HTTP
/// client this process builds then reads. Only `main` calls it, before the
/// runtime starts any thread: changing the environment is sound only while
/// no other thread reads it.
pub(crate) fn keep_this_computer_off_proxies() {
    if let Some(value) = no_proxy(|name| std::env::var(name).ok()) {
        std::env::set_var("NO_PROXY", &value);
        std::env::set_var("no_proxy", &value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    #[test]
    fn this_computer_is_added_only_when_a_proxy_is_set() {
        assert_eq!(no_proxy(with(&[])), None);
        assert_eq!(no_proxy(with(&[("NO_PROXY", "corp.example")])), None);
        for variable in PROXY_VARIABLES {
            assert_eq!(
                no_proxy(with(&[(variable, "http://proxy.example:3128")])).as_deref(),
                Some("localhost,127.0.0.0/8,::1"),
                "{variable}"
            );
        }
    }

    #[test]
    fn what_no_proxy_lists_is_kept_first() {
        let proxy = ("HTTPS_PROXY", "http://proxy.example:3128");
        assert_eq!(
            no_proxy(with(&[proxy, ("NO_PROXY", " corp.example, 10.0.0.0/8 ,")])).as_deref(),
            Some("corp.example,10.0.0.0/8,localhost,127.0.0.0/8,::1")
        );
        // reqwest reads no_proxy only when NO_PROXY is not set.
        assert_eq!(
            no_proxy(with(&[proxy, ("no_proxy", "corp.example")])).as_deref(),
            Some("corp.example,localhost,127.0.0.0/8,::1")
        );
        assert_eq!(
            no_proxy(with(&[
                proxy,
                ("NO_PROXY", "a.example"),
                ("no_proxy", "b.example")
            ]))
            .as_deref(),
            Some("a.example,localhost,127.0.0.0/8,::1")
        );
        assert_eq!(
            no_proxy(with(&[proxy, ("NO_PROXY", "localhost,corp.example")])).as_deref(),
            Some("localhost,corp.example,127.0.0.0/8,::1")
        );
        // Already set as this would set it (a child of this process): no
        // change.
        let done = "corp.example,localhost,127.0.0.0/8,::1";
        assert_eq!(
            no_proxy(with(&[proxy, ("NO_PROXY", done), ("no_proxy", done)])),
            None
        );
        assert_eq!(
            no_proxy(with(&[proxy, ("NO_PROXY", done)])).as_deref(),
            Some(done)
        );
    }

    /// Set in the child process of
    /// [`a_default_client_reaches_this_computer_directly_and_other_hosts_through_the_proxy`].
    const PROXY_CHILD: &str = "AXOCOATL_CLI_PROXY_ENV_TEST_CHILD";

    /// With `HTTP_PROXY` set and no `NO_PROXY`, a client built as reqwest's
    /// default (as rmcp's and the webhooks' are) after
    /// [`keep_this_computer_off_proxies`] connects straight to a server on
    /// `127.0.0.1`, `localhost` and `[::1]`, while a request to another host
    /// still goes to the proxy. The variables are set only in a child
    /// process of this test binary, which runs this one test.
    #[test]
    fn a_default_client_reaches_this_computer_directly_and_other_hosts_through_the_proxy() {
        if std::env::var_os(PROXY_CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proxy_env::tests::a_default_client_reaches_this_computer_directly_and_other_hosts_through_the_proxy",
                    "--nocapture",
                ])
                .env(PROXY_CHILD, "1")
                .env("HTTP_PROXY", "http://127.0.0.1:9")
                .env("HTTPS_PROXY", "http://127.0.0.1:9")
                .env_remove("NO_PROXY")
                .env_remove("no_proxy")
                .env_remove("ALL_PROXY")
                .env_remove("all_proxy")
                .output()
                .unwrap();
            let shown = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{shown}");
            assert!(shown.contains("1 passed"), "{shown}");
            return;
        }
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::<(&str, String)>::new()));
        let serve = |name: &'static str, address: &str| {
            let seen = seen.clone();
            let listener = std::net::TcpListener::bind(address).ok()?;
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let mut stream = stream.unwrap();
                    let mut buffer = [0u8; 2048];
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let line = head.lines().next().unwrap_or_default().to_string();
                    seen.lock().unwrap().push((name, line));
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                    );
                }
            });
            Some(port)
        };
        let server = serve("server", "127.0.0.1:0").unwrap();
        let server6 = serve("server", "[::1]:0");
        let proxy = serve("proxy", "127.0.0.1:0").unwrap();
        std::env::set_var("HTTP_PROXY", format!("http://127.0.0.1:{proxy}"));
        keep_this_computer_off_proxies();
        assert_eq!(
            std::env::var("NO_PROXY").unwrap(),
            "localhost,127.0.0.0/8,::1"
        );
        assert_eq!(
            std::env::var("no_proxy").unwrap(),
            "localhost,127.0.0.0/8,::1"
        );
        let mut urls = vec![
            format!("http://127.0.0.1:{server}/models"),
            format!("http://localhost:{server}/models"),
        ];
        if let Some(port) = server6 {
            urls.push(format!("http://[::1]:{port}/models"));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let client = reqwest::Client::new();
            for url in &urls {
                let response = client.get(url).send().await.unwrap();
                assert_eq!(response.status(), 200, "{url}");
                let got = seen.lock().unwrap().drain(..).collect::<Vec<_>>();
                assert_eq!(
                    got,
                    [("server", "GET /models HTTP/1.1".to_string())],
                    "{url}"
                );
            }
            let response = client
                .get("http://models.example.invalid/v1/models")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let got = seen.lock().unwrap().drain(..).collect::<Vec<_>>();
            assert_eq!(
                got,
                [(
                    "proxy",
                    "GET http://models.example.invalid/v1/models HTTP/1.1".to_string()
                )]
            );
        });
    }
}
