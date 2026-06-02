// Tells cargo to rebuild whenever the dashboard HTML changes.
// `routes.rs` embeds `static/index.html` via `include_str!`, but cargo
// does not always notice changes inside `include_str!` targets — so an
// edit to the dashboard could go un-recompiled. This makes the dependency
// explicit.
fn main() {
    println!("cargo:rerun-if-changed=static/index.html");
    println!("cargo:rerun-if-changed=build.rs");
}
