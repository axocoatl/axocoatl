fn usage() -> ! {
    eprintln!(
        "usage: axocoatl-exec-supervisor --serve [--harden] | --version\n\
         \x20      | --serve --harden --helper UID:GID --writer UID:GID --workspace PATH\n\
         \x20      | --egress-proxy --socket <path> [--identity-socket <path>] [--max-connections N]\n\
         \x20      | --bridge [--max-connections N]\n\
         \x20                 [--tcp-to-unix <ip:port>=<path> [--http-errors] [--peer-identity]]...\n\
         \x20                 [--unix-to-tcp <path>=<ip:port>]... [--allow-nonloopback-listen]\n\
         \x20      | --probe-unix <path>"
    );
    std::process::exit(2);
}

#[cfg(unix)]
fn egress_proxy(arguments: &[String]) -> i32 {
    use axocoatl_exec::egress::protocol::{DEFAULT_MAX_CONNECTIONS, MAX_MAX_CONNECTIONS};
    let mut socket = None;
    let mut identity_socket = None;
    let mut max_connections = DEFAULT_MAX_CONNECTIONS as usize;
    let mut index = 0;
    while index < arguments.len() {
        match (arguments[index].as_str(), arguments.get(index + 1)) {
            ("--socket", Some(path)) if path.starts_with('/') && socket.is_none() => {
                socket = Some(std::path::PathBuf::from(path))
            }
            ("--identity-socket", Some(path))
                if path.starts_with('/') && identity_socket.is_none() =>
            {
                identity_socket = Some(std::path::PathBuf::from(path))
            }
            ("--max-connections", Some(count)) => match count.parse::<usize>() {
                Ok(count) if (1..=MAX_MAX_CONNECTIONS as usize).contains(&count) => {
                    max_connections = count
                }
                _ => usage(),
            },
            _ => usage(),
        }
        index += 2;
    }
    let Some(socket) = socket else { usage() };
    if identity_socket.as_ref() == Some(&socket) {
        usage();
    }
    axocoatl_exec::egress::proxy::main(socket, identity_socket, max_connections)
}

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments == ["--version"] {
        println!("axocoatl-exec-supervisor {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    #[cfg(unix)]
    match arguments.first().map(String::as_str) {
        Some("--egress-proxy") => std::process::exit(egress_proxy(&arguments[1..])),
        Some("--bridge") => {
            std::process::exit(axocoatl_exec::egress::bridge::main(&arguments[1..]))
        }
        Some("--probe-unix") if arguments.len() == 2 && arguments[1].starts_with('/') => {
            let reachable = axocoatl_exec::egress::bridge::probe_unix(arguments[1].clone().into());
            std::process::exit(if reachable { 0 } else { 1 });
        }
        _ => {}
    }
    let (harden, helper) = match arguments.as_slice() {
        [serve] if serve == "--serve" => (false, None),
        [serve, harden] if serve == "--serve" && harden == "--harden" => (true, None),
        [serve, harden, view @ ..] if serve == "--serve" && harden == "--harden" => {
            match axocoatl_exec::protocol::HelperView::parse(view) {
                Ok(view) => (true, Some(view)),
                Err(_) => usage(),
            }
        }
        _ => usage(),
    };
    #[cfg(target_os = "linux")]
    let result = axocoatl_exec::supervisor::serve_with(axocoatl_exec::supervisor::ServeOptions {
        harden,
        helper,
    });
    #[cfg(not(target_os = "linux"))]
    let result: Result<(), String> = {
        let _ = (harden, helper);
        Err("execution supervision requires Linux".into())
    };
    if let Err(error) = result {
        eprintln!("execution supervisor: {error}");
        std::process::exit(1);
    }
}
