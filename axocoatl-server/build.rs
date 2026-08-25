use std::path::{Path, PathBuf};

const LATTICE_FILES: &[&str] = &[
    "index.js",
    "lattice.js",
    "node.js",
    "handle.js",
    "edge.js",
    "minimap.js",
    "controls.js",
    "viewport.js",
    "selection.js",
    "geometry.js",
    "history.js",
    "layout.js",
];

const BRAND_FILES: &[&str] = &[
    "mark.png",
    "favicon.png",
    "wordmark.png",
    "wordmark-ink.png",
    "wordmark-vellum.png",
    "colors.json",
    "mcp-catalog.json",
];

fn directory_file_names(directory: &Path, extension: Option<&str>) -> Vec<String> {
    let mut names = std::fs::read_dir(directory)
        .unwrap_or_else(|error| {
            panic!(
                "could not list package embedded-asset directory {}: {error}",
                directory.display()
            )
        })
        .map(|entry| {
            let entry = entry.expect("embedded-asset directory entry is readable");
            if !entry
                .file_type()
                .expect("embedded-asset file type is readable")
                .is_file()
            {
                panic!(
                    "embedded-asset directory {} contains non-regular entry {}; run scripts/sync-server-embedded-assets.sh",
                    directory.display(),
                    entry.path().display()
                );
            }
            entry
        })
        .filter_map(|entry| {
            let path = entry.path();
            if extension.is_some_and(|expected| {
                path.extension().and_then(|value| value.to_str()) != Some(expected)
            }) {
                return None;
            }
            Some(entry.file_name().to_string_lossy().into_owned())
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn require_exact_file_set(directory: &Path, extension: Option<&str>, expected: &[&str]) {
    let actual = directory_file_names(directory, extension);
    let mut expected = expected
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    expected.sort();
    if actual != expected {
        panic!(
            "embedded-asset file set in {} is not exact; expected {expected:?}, found {actual:?}; run scripts/sync-server-embedded-assets.sh",
            directory.display()
        );
    }
}

fn require_synced_assets(canonical: &Path, packaged: &Path, files: &[&str]) {
    // A published crate intentionally has no workspace siblings. Its checked-in
    // package-local copies are the complete build input.
    if !canonical.is_dir() {
        return;
    }

    for file in files {
        let canonical_file = canonical.join(file);
        let packaged_file = packaged.join(file);
        let canonical_bytes = std::fs::read(&canonical_file).unwrap_or_else(|error| {
            panic!(
                "could not read canonical embedded asset {}: {error}",
                canonical_file.display()
            )
        });
        let packaged_bytes = std::fs::read(&packaged_file).unwrap_or_else(|error| {
            panic!(
                "could not read package-local embedded asset {}: {error}; run scripts/sync-server-embedded-assets.sh",
                packaged_file.display()
            )
        });
        if packaged_bytes != canonical_bytes {
            panic!(
                "package-local embedded asset {} drifted from {}; run scripts/sync-server-embedded-assets.sh",
                packaged_file.display(),
                canonical_file.display()
            );
        }
    }
}

// The app shell and its native modules are embedded in the server binary.
// Keep every embedded directory in Cargo's dependency graph, and reject drift
// between package-local mirrors and their canonical workspace sources. Cargo
// packages cannot access sibling workspace directories when they are verified.
fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let workspace_root = manifest_dir
        .parent()
        .expect("axocoatl-server has a workspace parent");

    let canonical_lattice = workspace_root.join("packages/lattice/src");
    let packaged_lattice = manifest_dir.join("static/lattice");
    let canonical_brand = workspace_root.join("branding");
    let packaged_brand = manifest_dir.join("static/brand");
    require_exact_file_set(&packaged_lattice, None, LATTICE_FILES);
    require_exact_file_set(&packaged_brand, None, BRAND_FILES);
    if canonical_lattice.is_dir() {
        require_exact_file_set(&canonical_lattice, Some("js"), LATTICE_FILES);
    }
    require_synced_assets(&canonical_lattice, &packaged_lattice, LATTICE_FILES);
    require_synced_assets(&canonical_brand, &packaged_brand, BRAND_FILES);

    println!("cargo:rerun-if-changed=static/index.html");
    println!("cargo:rerun-if-changed=static/ui");
    println!("cargo:rerun-if-changed=static/lattice");
    println!("cargo:rerun-if-changed=static/brand");
    println!("cargo:rerun-if-changed=../packages/lattice/src");
    println!("cargo:rerun-if-changed=../branding");
    println!("cargo:rerun-if-changed=build.rs");
}
