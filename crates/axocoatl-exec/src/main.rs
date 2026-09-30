fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments == ["--version"] {
        println!("axocoatl-exec-supervisor {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if arguments != ["--serve"] {
        eprintln!("usage: axocoatl-exec-supervisor --serve | --version");
        std::process::exit(2);
    }
    #[cfg(target_os = "linux")]
    let result = axocoatl_exec::supervisor::serve();
    #[cfg(not(target_os = "linux"))]
    let result: Result<(), String> = Err("execution supervision requires Linux".into());
    if let Err(error) = result {
        eprintln!("execution supervisor: {error}");
        std::process::exit(1);
    }
}
