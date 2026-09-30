use sha2::{Digest, Sha256};
use std::{env, fs, path::Path};

fn source_files(root: &Path, directory: &Path, entries: &mut Vec<(String, Vec<u8>)>) {
    for entry in fs::read_dir(directory).expect("read supervisor source directory") {
        let path = entry.expect("read supervisor source entry").path();
        let metadata = fs::symlink_metadata(&path).expect("inspect supervisor source entry");
        assert!(
            !metadata.file_type().is_symlink(),
            "supervisor source cannot be symlinked"
        );
        if metadata.is_dir() {
            source_files(root, &path, entries);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            assert!(
                metadata.is_file(),
                "supervisor source must be a regular file"
            );
            entries.push((
                path.strip_prefix(root)
                    .expect("source stays in package")
                    .to_str()
                    .expect("UTF-8 source path")
                    .replace('\\', "/"),
                fs::read(&path).expect("read supervisor source"),
            ));
        }
    }
}

fn main() {
    let root = env::var_os("CARGO_MANIFEST_DIR").expect("Cargo package directory");
    let root = Path::new(&root);
    let mut entries = Vec::new();
    source_files(root, &root.join("src"), &mut entries);
    let build = root.join("build.rs");
    assert!(fs::symlink_metadata(&build)
        .expect("inspect build script")
        .is_file());
    entries.push((
        "build.rs".into(),
        fs::read(build).expect("read build script"),
    ));
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let mut fingerprint = Sha256::new();
    fingerprint.update(format!("axocoatl-exec-source-v1\nversion={version}\n"));
    for (path, bytes) in entries {
        assert!(!path.contains(['\0', '\n', '\r']), "invalid source path");
        println!("cargo:rerun-if-changed={path}");
        fingerprint.update(format!(
            "{path}\0{}\0{:x}\n",
            bytes.len(),
            Sha256::digest(&bytes)
        ));
    }
    // Directory tracking detects newly added source files, not only changes to
    // files present in the previous compilation. Cargo.toml itself is omitted:
    // Cargo rewrites it when packaging. The resolved package version is above.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!(
        "cargo:rustc-env=AXOCOATL_EXEC_SOURCE_SHA256={:x}",
        fingerprint.finalize()
    );
}
