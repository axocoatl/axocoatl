// The app shell and its native modules are embedded in the server binary.
// Keep both the include_str! entrypoint and rust-embed's UI directory in
// Cargo's dependency graph so a browser-app edit cannot leave a stale binary.
fn main() {
    println!("cargo:rerun-if-changed=static/index.html");
    println!("cargo:rerun-if-changed=static/ui");
    println!("cargo:rerun-if-changed=build.rs");
}
