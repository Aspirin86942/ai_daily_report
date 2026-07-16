use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=AI_DAILY_BUILD_ID");
    let build = env::var("AI_DAILY_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(local_source_fingerprint);
    println!("cargo:rustc-env=AI_DAILY_ENGINE_BUILD={build}");
    println!("cargo:rustc-env=AI_DAILY_OFFICE_WORKER_BUILD={build}");
}

fn local_source_fingerprint() -> String {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let workspace = manifest_dir.parent().expect("Rust workspace root");
    let mut files = Vec::new();
    for relative in [
        "scanner_contract/src",
        "scanner_core/src",
        "discovery/src",
        "office_parser/src",
    ] {
        collect_files(&workspace.join(relative), &mut files);
    }
    for relative in [
        "Cargo.toml",
        "scanner_contract/Cargo.toml",
        "scanner_core/Cargo.toml",
        "discovery/Cargo.toml",
        "office_parser/Cargo.toml",
    ] {
        files.push(workspace.join(relative));
    }
    files.push(workspace.join("scanner_core/build.rs"));
    files.push(workspace.join("Cargo.lock"));
    files.sort_by(|left, right| {
        relative_text(workspace, left).cmp(&relative_text(workspace, right))
    });

    let mut hasher = Sha256::new();
    hasher.update(b"ai-daily-rust-build-v1\0");
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let relative = relative_text(workspace, &path);
        let bytes = fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "build fingerprint input {} is unavailable: {error}",
                path.display()
            )
        });
        hash_entry(&mut hasher, relative.as_bytes(), &bytes);
    }
    format!("sha256-source-v1:{}", hex_bytes(&hasher.finalize()))
}

fn collect_files(root: &Path, files: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(root)
        .unwrap_or_else(|error| panic!("cannot enumerate {}: {error}", root.display()))
        .map(|entry| entry.expect("source entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_files(&path, files);
        } else if path.is_file() {
            files.push(path);
        }
    }
}

fn relative_text(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .expect("fingerprint input must be inside workspace")
        .to_str()
        .expect("fingerprint paths must be UTF-8")
        .replace('\\', "/")
}

fn hash_entry(hasher: &mut Sha256, path: &[u8], bytes: &[u8]) {
    hasher.update((path.len() as u64).to_le_bytes());
    hasher.update(path);
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
