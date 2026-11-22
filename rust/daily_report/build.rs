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
    println!("cargo:rustc-check-cfg=cfg(delay_network_imports)");
    if std::env::var("TARGET").as_deref() == Ok("x86_64-pc-windows-msvc")
        && std::env::var("PROFILE").as_deref() == Ok("release")
    {
        // History queries and local maintenance can start without the Windows network stack.
        println!("cargo:rustc-cfg=delay_network_imports");
        for dll in ["ws2_32.dll", "crypt32.dll", "secur32.dll"] {
            println!("cargo:rustc-link-arg-bin=ai-daily-report=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg-bin=ai-daily-report=delayimp.lib");
    }
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
    let parser = std::env::var("DEP_DAILY_PARSERS_IDENTITY").expect("paired parser build identity");
    hash.update(parser.as_bytes());
    for file in files {
        println!("cargo:rerun-if-changed={}", file.display());
        let relative = file
            .strip_prefix(root.parent().unwrap().parent().unwrap())
            .expect("local application input");
        hash.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hash.update([0]);
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
