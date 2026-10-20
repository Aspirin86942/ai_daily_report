//! Build-time content identity. Full engine identity remains a separate audit field.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, collections::BTreeSet, env, fs, path::Path, process::Command};

pub fn emit(workspace: &Path) {
    assert!(
        fs::read(workspace.join("build_identity/src/parser_identity.rs"))
            .is_ok_and(|bytes| bytes.as_slice() == include_bytes!("parser_identity.rs").as_slice()),
        "parser identity build script does not match source; rebuild the build script"
    );
    let spec: Value = serde_json::from_slice(
        &fs::read(workspace.join("parser-inputs.json")).expect("parser input specification"),
    )
    .expect("parser input JSON");
    let mut inputs = BTreeMap::new();
    for source in spec["sources"].as_array().expect("parser source list") {
        collect(
            workspace,
            &workspace.join(source.as_str().expect("source path")),
            &mut inputs,
        );
    }
    let target = env::var("TARGET").expect("target");
    let roots = spec["dependency_roots"].as_array().unwrap();
    let metadata = dependency_metadata(workspace, &target, roots);
    let packages: BTreeMap<_, _> = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p))
        .collect();
    let nodes: BTreeMap<_, _> = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p))
        .collect();
    let mut queue: Vec<_> = packages
        .iter()
        .filter(|(_, p)| roots.contains(&p["name"]))
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(
        queue.len(),
        roots.len(),
        "all parser dependency roots must exist"
    );
    let mut visited = BTreeSet::new();
    let mut dependencies = BTreeMap::new();
    while let Some(id) = queue.pop() {
        if !visited.insert(id) {
            continue;
        }
        let package = packages[id];
        let node = nodes[id];
        println!(
            "cargo:rerun-if-changed={}",
            package["manifest_path"].as_str().unwrap()
        );
        let dependencies_used: Vec<_> = node["deps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| {
                d["dep_kinds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|k| k["kind"].is_null() || k["kind"] == "build")
            })
            .map(|d| d["pkg"].as_str().unwrap())
            .collect();
        queue.extend(dependencies_used.iter().copied());
        // Local package IDs contain absolute paths; identities must be relocatable.
        let labels: Vec<_> = dependencies_used
            .iter()
            .map(|id| {
                let p = packages[id];
                format!(
                    "{}@{}:{}",
                    p["name"].as_str().unwrap(),
                    p["version"].as_str().unwrap(),
                    p["source"]
                )
            })
            .collect();
        dependencies.insert(
            format!(
                "{}@{}",
                package["name"].as_str().unwrap(),
                package["version"].as_str().unwrap()
            ),
            json!({"source":package["source"],"features":node["features"],"dependencies":labels}),
        );
    }
    let lock = fs::read_to_string(workspace.join("Cargo.lock")).expect("locked dependencies");
    // Include complete lock records for resolved parser packages, including checksums.
    // Cargo validates this file; records with duplicate names/versions are all retained.
    let records: Vec<_> = lock
        .split("[[package]]")
        .skip(1)
        .filter(|record| {
            let name = field(record, "name");
            let version = field(record, "version");
            dependencies.contains_key(&format!("{name}@{version}"))
        })
        .collect();
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join("Cargo.lock").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        workspace.join("Cargo.toml").display()
    );
    let mut environment = BTreeMap::new();
    for (name, value) in env::vars() {
        if name.starts_with("CARGO_CFG_") || name == "OPT_LEVEL" {
            environment.insert(name, value);
        }
    }
    let flags = env::var("CARGO_ENCODED_RUSTFLAGS").unwrap_or_default();
    println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
    environment.insert("rustflags".into(), content_flags(&flags));
    let compiler = Command::new(env::var_os("RUSTC").expect("rustc"))
        .arg("-vV")
        .output()
        .expect("rustc version");
    assert!(compiler.status.success(), "rustc version failed");
    let document = json!({"schema":"ai_daily_parser_build_v1","sources":inputs,
        "dependencies":dependencies,"lock_records":records,"environment":environment,
        "compiler":String::from_utf8(compiler.stdout).expect("rustc version UTF-8")});
    let bytes = serde_json::to_vec(&document).expect("parser build JSON");
    let identity = format!(
        "sha256-parser-v1:{}",
        super::hex_bytes(&Sha256::digest(&bytes))
    );
    println!("cargo:rustc-env=AI_DAILY_PARSER_BUILD={identity}");
    fs::write(
        Path::new(&env::var_os("OUT_DIR").expect("output directory")).join("parser-build.json"),
        bytes,
    )
    .expect("parser build inputs");
}

fn dependency_metadata(workspace: &Path, target: &str, roots: &[Value]) -> Value {
    let cargo = env::var_os("CARGO").expect("cargo");
    let version = Command::new(&cargo)
        .arg("--version")
        .output()
        .expect("cargo version");
    assert!(version.status.success(), "cargo version failed");
    let request = json!([
        3,
        workspace,
        target,
        cargo,
        String::from_utf8_lossy(&version.stdout),
        roots,
    ]);
    let cache = Path::new(&env::var_os("OUT_DIR").expect("output directory"))
        .join("parser-dependencies.json");
    let reusable = metadata_cache_allowed(workspace);
    if reusable {
        if let Ok(bytes) = fs::read(&cache) {
            if let Ok(saved) = serde_json::from_slice::<Value>(&bytes) {
                let value = &saved["value"];
                let checksum =
                    super::hex_bytes(&Sha256::digest(serde_json::to_vec(value).unwrap()));
                if saved["sha256"] == checksum && value["request"] == request {
                    if let Some(manifests) = value["manifests"].as_object() {
                        if !manifests.is_empty()
                            && manifests.iter().all(|(path, digest)| {
                                fs::read(path).is_ok_and(|bytes| {
                                    *digest == super::hex_bytes(&Sha256::digest(bytes))
                                })
                            })
                        {
                            return value["metadata"].clone();
                        }
                    }
                }
            }
        }
    }
    let output = Command::new(&cargo)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--offline",
            "--locked",
            "--filter-platform",
        ])
        .arg(target)
        .arg("--manifest-path")
        .arg(workspace.join("Cargo.toml"))
        .output()
        .expect("locked parser dependency metadata");
    assert!(
        output.status.success(),
        "parser dependency metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let complete: Value = serde_json::from_slice(&output.stdout).expect("dependency metadata JSON");
    let packages_by_id: BTreeMap<_, _> = complete["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p))
        .collect();
    let nodes_by_id: BTreeMap<_, _> = complete["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p))
        .collect();
    let mut queue: Vec<_> = packages_by_id
        .iter()
        .filter(|(_, p)| roots.contains(&p["name"]))
        .map(|(id, _)| *id)
        .collect();
    let mut retained = BTreeSet::new();
    while let Some(id) = queue.pop() {
        if retained.insert(id) {
            queue.extend(
                nodes_by_id[id]["deps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|d| {
                        d["dep_kinds"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|k| k["kind"].is_null() || k["kind"] == "build")
                    })
                    .map(|d| d["pkg"].as_str().unwrap()),
            );
        }
    }
    let fields = |value: &Value, names: &[&str]| -> Value {
        Value::Object(
            names
                .iter()
                .map(|name| ((*name).to_owned(), value[*name].clone()))
                .collect(),
        )
    };
    let packages: Vec<_> = complete["packages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|package| retained.contains(package["id"].as_str().unwrap()))
        .map(|package| {
            fields(
                package,
                &["id", "name", "version", "source", "manifest_path"],
            )
        })
        .collect();
    let nodes: Vec<_> = complete["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| retained.contains(node["id"].as_str().unwrap()))
        .map(|node| fields(node, &["id", "deps", "features"]))
        .collect();
    let metadata = json!({"packages":packages, "resolve":{"nodes":nodes}});
    if reusable {
        let mut paths = vec![workspace.join("Cargo.toml"), workspace.join("Cargo.lock")];
        paths.extend(
            complete["packages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|package| {
                    package["source"].is_null()
                        || retained.contains(package["id"].as_str().unwrap())
                })
                .map(|package| Path::new(package["manifest_path"].as_str().unwrap()).to_owned()),
        );
        let manifests: BTreeMap<_, _> = paths
            .into_iter()
            .map(|path| {
                let digest = super::hex_bytes(&Sha256::digest(
                    fs::read(&path).expect("dependency manifest"),
                ));
                (path.to_string_lossy().into_owned(), digest)
            })
            .collect();
        let value = json!({"request":request, "manifests":manifests, "metadata":metadata});
        let checksum = super::hex_bytes(&Sha256::digest(serde_json::to_vec(&value).unwrap()));
        let _ = fs::write(
            cache,
            serde_json::to_vec(&json!({"sha256":checksum, "value":value})).unwrap(),
        );
    }
    metadata
}

fn metadata_cache_allowed(workspace: &Path) -> bool {
    // Config overrides take the uncached path; never read credentials/configuration.
    let mut roots: Vec<_> = workspace.ancestors().map(|p| p.join(".cargo")).collect();
    if let Some(home) = env::var_os("CARGO_HOME") {
        roots.push(home.into());
    } else if let Some(home) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) {
        roots.push(Path::new(&home).join(".cargo"));
    }
    let manifest = env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory");
    roots.push(Path::new(&manifest).join(".cargo"));
    if fs::read(workspace.join("Cargo.toml"))
        .map_or(true, |bytes| bytes.contains(&b'*') || bytes.contains(&b'?'))
    {
        return false;
    }
    if roots.iter().any(|root| {
        ["config", "config.toml"]
            .iter()
            .any(|name| root.join(name).exists())
    }) {
        return false;
    }
    !env::vars_os().any(|(name, _)| {
        let name = name.to_string_lossy();
        name.starts_with("CARGO_")
            && ![
                "CARGO_CFG_",
                "CARGO_FEATURE_",
                "CARGO_PKG_",
                "CARGO_MANIFEST_",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix))
            && !matches!(
                name.as_ref(),
                "CARGO_HOME"
                    | "CARGO_ENCODED_RUSTFLAGS"
                    | "CARGO_MAKEFLAGS"
                    | "CARGO_CRATE_NAME"
                    | "CARGO_PRIMARY_PACKAGE"
                    | "CARGO_BIN_NAME"
                    | "CARGO_TARGET_TMPDIR"
            )
    })
}

fn field<'a>(record: &'a str, name: &str) -> &'a str {
    let prefix = format!("{name} = \"");
    record
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).and_then(|v| v.strip_suffix('"')))
        .expect("Cargo lock package identity")
}

fn collect(workspace: &Path, path: &Path, output: &mut BTreeMap<String, String>) {
    println!("cargo:rerun-if-changed={}", path.display());
    if path.is_dir() {
        for entry in fs::read_dir(path).expect("parser source directory") {
            collect(
                workspace,
                &entry.expect("parser source entry").path(),
                output,
            );
        }
    } else {
        let name = path
            .strip_prefix(workspace)
            .expect("parser input inside workspace")
            .to_str()
            .expect("UTF-8 path")
            .replace('\\', "/");
        output.insert(
            name,
            super::hex_bytes(&Sha256::digest(
                fs::read(path).expect("parser source bytes"),
            )),
        );
    }
}

fn content_flags(flags: &str) -> String {
    let mut result = Vec::new();
    let mut arguments = flags.split('\u{1f}').peekable();
    while let Some(argument) = arguments.next() {
        let value = if argument == "-C" {
            arguments.peek().copied()
        } else {
            argument.strip_prefix("-C")
        };
        if value.is_some_and(|v| {
            ["debuginfo=", "strip=", "incremental="]
                .iter()
                .any(|key| v.starts_with(key))
        }) {
            if argument == "-C" {
                arguments.next();
            }
        } else {
            result.push(argument);
        }
    }
    result.join("\u{1f}")
}
