use sha2::{Digest, Sha256};
use std::{env, fs, path::Path, process::Command};

fn add_input(root: &Path, path: &Path, hash: &mut Sha256) {
    println!("cargo:rerun-if-changed={}", path.display());
    let metadata = fs::symlink_metadata(path).expect("read input metadata");
    assert!(
        !metadata.file_type().is_symlink(),
        "redirected parser input"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        assert_eq!(
            metadata.file_attributes() & 0x400,
            0,
            "redirected parser input"
        );
    }
    if metadata.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(path)
            .expect("list parser inputs")
            .map(|entry| entry.expect("read parser input").path())
            .collect();
        entries.sort();
        for entry in entries {
            add_input(root, &entry, hash);
        }
    } else {
        let name = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        hash.update(name.as_bytes());
        hash.update([0]);
        assert!(metadata.is_file(), "parser inputs must be ordinary files");
        let data = fs::read(path).expect("read parser input");
        hash.update((data.len() as u64).to_le_bytes());
        hash.update(data);
    }
}

fn main() {
    let pkg = env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let root = Path::new(&pkg).parent().unwrap();
    let mut hash = Sha256::new();
    #[derive(serde::Deserialize)]
    struct Inputs {
        sources: Vec<String>,
    }
    let inputs: Inputs = serde_json::from_slice(
        &fs::read(root.join("parser-inputs.json")).expect("read parser input manifest"),
    )
    .expect("decode parser input manifest");
    for input in inputs.sources {
        assert!(
            Path::new(&input)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_))),
            "nonlocal parser input"
        );
        add_input(root, &root.join(input), &mut hash);
    }
    for key in [
        "TARGET",
        "PROFILE",
        "CARGO_CFG_TARGET_FEATURE",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_FEATURE_OCR",
    ] {
        println!("cargo:rerun-if-env-changed={key}");
        hash.update(env::var_os(key).unwrap_or_default().as_encoded_bytes());
        hash.update([0]);
    }
    let version = Command::new(env::var_os("RUSTC").unwrap())
        .arg("-vV")
        .output()
        .expect("query compiler identity");
    assert!(version.status.success(), "could not identify rustc");
    hash.update(version.stdout);
    let digest: String = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("cargo:rustc-env=DAILY_PARSER_IDENTITY={digest}");
    println!("cargo::metadata=identity={digest}");
}
