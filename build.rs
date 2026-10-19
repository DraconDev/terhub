//! Build identity.
//!
//! The daemon and the client have to be able to tell whether they are
//! the same build. That question has a sharp edge: a daemon that
//! speaks a protocol the client cannot decode does not look like a
//! broken daemon, it looks like *no* daemon, and every pane in the TUI
//! goes blank while a healthy daemon keeps serving.
//!
//! The identity is a fingerprint of the sources the package was built
//! from, so every binary in the package (`terhub`, `terhubd`) carries
//! the same id, and a client built from a different tree is detectable
//! without comparing binaries byte by byte. Git is deliberately not
//! consulted: a release build from a tarball has no git metadata, and
//! an id that vanishes on some builds is worse than no id at all.

use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=src");
    let id = fingerprint();
    println!("cargo:rustc-env=TERHUB_BUILD_ID={id}");
}

/// FNV-1a over the package's sources, in a stable order.
fn fingerprint() -> String {
    let mut files: Vec<PathBuf> = Vec::new();
    collect(Path::new("src"), &mut files);
    for extra in ["Cargo.toml", "Cargo.lock"] {
        let p = Path::new(extra);
        if p.exists() {
            files.push(p.to_path_buf());
        }
    }
    files.sort();

    let mut h: u64 = 0xcbf29ce484222325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    for file in &files {
        eat(file.to_string_lossy().as_bytes());
        eat(b"\0");
        match std::fs::read(file) {
            Ok(bytes) => {
                eat(&(bytes.len() as u64).to_le_bytes());
                eat(&bytes);
            }
            Err(_) => {
                // A file that cannot be read still has to change the
                // id, or two different trees could share one.
                eat(b"<unreadable>");
            }
        }
        eat(b"\0");
    }
    format!("{h:016x}")
}

/// Every `.rs` file under `dir`, recursively.
fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}
