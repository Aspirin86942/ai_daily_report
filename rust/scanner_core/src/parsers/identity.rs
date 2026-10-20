//! A content identity is trusted only with the matching executable and PDFium bytes.
use crate::source_guard::{compute_source_guard, SourceGuardV2};
use ai_daily_scanner_contract::SourceGuardKind;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs::File, io::Read, path::Path};
use std::{
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    parser_content_identity: String,
    engine_build: String,
    files: BTreeMap<String, String>,
}

pub fn file_hash(path: &Path) -> Result<String, String> {
    type VerifiedFiles = BTreeMap<PathBuf, (SourceGuardV2, String)>;
    static HASHES: OnceLock<Mutex<VerifiedFiles>> = OnceLock::new();
    let guard = compute_source_guard(path).map_err(|e| e.to_string())?;
    if guard.kind == SourceGuardKind::ContentSha256V1 {
        return guard
            .guard_sha256
            .ok_or_else(|| "file content hash unavailable".into());
    }
    if guard.kind == SourceGuardKind::Unavailable {
        return Err("worker file identity unavailable".into());
    }
    let cache = HASHES.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut files = cache.lock().map_err(|_| "worker file hash lock poisoned")?;
    if let Some((previous, digest)) = files.get(path) {
        if previous == &guard {
            return Ok(digest.clone());
        }
    }
    let mut stream = File::open(path).map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = stream.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let digest = ai_daily_worker_contract::archive::hex(&digest.finalize());
    if compute_source_guard(path).map_err(|e| e.to_string())? != guard {
        return Err("worker file changed while hashing".into());
    }
    // Recheck file ID/change-time on every call; retain hashes only within this process.
    if files.len() >= 64 {
        files.clear();
    }
    files.insert(path.to_owned(), (guard, digest.clone()));
    Ok(digest)
}

pub fn worker_content_identity(
    program: &Path,
    expected_build: Option<&str>,
) -> Result<Option<String>, String> {
    let inputs: serde_json::Value =
        serde_json::from_str(include_str!("../../../build-inputs.json"))
            .map_err(|e| e.to_string())?;
    verified_content_identity(
        program,
        expected_build,
        inputs["pdfium"]["dll_sha256"]
            .as_str()
            .ok_or("compiled PDFium pin missing")?,
    )
}

fn verified_content_identity(
    program: &Path,
    expected_build: Option<&str>,
    pinned_pdfium: &str,
) -> Result<Option<String>, String> {
    let directory = program.parent().ok_or("worker has no parent")?;
    let path = directory.join("parser-manifest.json");
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if manifest.schema != "ai_daily_parser_manifest_v1"
        || manifest.parser_content_identity != crate::PARSER_CONTENT_IDENTITY
        || expected_build.is_some_and(|build| build != manifest.engine_build)
        || manifest.files.get("pdfium.dll").map(String::as_str) != Some(pinned_pdfium)
    {
        return Err("worker parser identity or complete build mismatch".into());
    }
    let name = program
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid worker filename")?;
    if !matches!(
        name,
        "ai-daily-office-parser.exe" | "ai-daily-pdf-parser.exe"
    ) {
        return Err("unsupported parser manifest executable".into());
    }
    for name in [name, "pdfium.dll"] {
        let expected = manifest
            .files
            .get(name)
            .ok_or("parser manifest file missing")?;
        if expected != &file_hash(&directory.join(name))? {
            return Err("parser manifest file hash mismatch".into());
        }
    }
    Ok(Some(manifest.parser_content_identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_reuse_rejects_replacement_with_same_length_and_modified_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker");
        std::fs::write(&path, b"first").unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let original = file_hash(&path).unwrap();
        assert_eq!(file_hash(&path).unwrap(), original);
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, b"other").unwrap();
        File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_ne!(file_hash(&path).unwrap(), original);
    }

    #[test]
    fn content_identity_requires_exact_build_and_files() {
        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("ai-daily-office-parser.exe");
        let dll = dir.path().join("pdfium.dll");
        std::fs::write(&worker, b"worker-a").unwrap();
        std::fs::write(&dll, b"pinned-pdfium").unwrap();
        let pinned = file_hash(&dll).unwrap();
        let verify = |build| verified_content_identity(&worker, Some(build), &pinned);
        assert_eq!(
            worker_content_identity(&worker, Some("build-a")).unwrap(),
            None
        );
        let manifest = json!({"schema":"ai_daily_parser_manifest_v1",
            "parser_content_identity":crate::PARSER_CONTENT_IDENTITY,"engine_build":"build-a",
            "files":{"ai-daily-office-parser.exe":file_hash(&worker).unwrap(),"pdfium.dll":file_hash(&dll).unwrap()}});
        let path = dir.path().join("parser-manifest.json");
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_eq!(
            verify("build-a").unwrap().as_deref(),
            Some(crate::PARSER_CONTENT_IDENTITY)
        );
        assert!(verified_content_identity(&worker, Some("build-a"), &"f".repeat(64)).is_err());
        assert!(verify("different-build").is_err());
        std::fs::write(&worker, b"changed-worker").unwrap();
        assert!(verify("build-a").is_err());
        std::fs::write(&worker, b"worker-a").unwrap();
        std::fs::write(&dll, b"changed-pdfium").unwrap();
        assert!(verify("build-a").is_err());
        std::fs::write(&dll, b"pinned-pdfium").unwrap();
        let mut invalid = manifest;
        invalid["parser_content_identity"] = json!("unverified-parser");
        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(verify("build-a").is_err());
    }
}
