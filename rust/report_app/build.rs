use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
};
fn collect(path: &Path, files: &mut Vec<PathBuf>) {
    for item in fs::read_dir(path).expect("source directory") {
        let path = item.expect("source entry").path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}
fn main() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut files = vec![
        root.join("Cargo.toml"),
        root.join("build.rs"),
        root.join("../Cargo.lock"),
    ];
    collect(&root.join("src"), &mut files);
    collect(&root.join("../../templates"), &mut files);
    files.sort();
    let mut hash = Sha256::new();
    for file in files {
        println!("cargo:rerun-if-changed={}", file.display());
        hash.update(fs::read(file).expect("build input"));
        hash.update([0]);
    }
    println!(
        "cargo:rustc-env=AI_DAILY_REPORT_BUILD={}",
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
}
