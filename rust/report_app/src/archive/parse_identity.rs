use crate::{config::AppConfig, ReportError};
use ai_daily_scanner_core::parsers::identity::{file_hash, worker_content_identity};
use ai_daily_scanner_core::source_guard::{
    compute_source_guard, source_guard_kind_text, SourceGuardKind,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};

const VERIFICATION_KEY: &str = "parser-verification:v1";

#[derive(Clone, Deserialize, Serialize)]
struct WorkerProof {
    identity: String,
    sha256: Option<String>,
    engine_build: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct Verification {
    key: String,
    office: WorkerProof,
    pdf: WorkerProof,
}

impl Verification {
    pub fn profile(&self, config: &AppConfig) -> Result<String, ReportError> {
        profile_from_identities(config, &self.office.identity, &self.pdf.identity)
    }

    pub fn executable_hash(&self, pdf: bool) -> Option<&str> {
        self.worker(pdf).sha256.as_deref()
    }

    fn worker(&self, pdf: bool) -> &WorkerProof {
        if pdf {
            &self.pdf
        } else {
            &self.office
        }
    }

    pub fn check_worker(
        &self,
        config: &AppConfig,
        pdf: bool,
        build: &str,
    ) -> Result<(), ReportError> {
        if self.key != verification_key(config)?
            || self
                .worker(pdf)
                .engine_build
                .as_deref()
                .is_some_and(|b| b != build)
        {
            return Err(super::error("ARCHIVE_WORKER_IDENTITY_INVALID"));
        }
        Ok(())
    }

    pub fn persist(
        &self,
        config: &AppConfig,
        store: &super::store::ArchiveStore,
    ) -> Result<(), ReportError> {
        if self.key != verification_key(config)? {
            return Err(super::error("ARCHIVE_WORKER_IDENTITY_INVALID"));
        }
        let proof = serde_json::to_value(self)?;
        let value = serde_json::json!({"sha256":crate::run::hash(&proof)?, "proof":proof});
        let value = value.to_string();
        let saved: Option<String> = store
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?",
                [VERIFICATION_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if saved.as_deref() == Some(value.as_str()) {
            return Ok(());
        }
        store.conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value
             WHERE metadata.value<>excluded.value",
            rusqlite::params![VERIFICATION_KEY, value],
        )?;
        Ok(())
    }
}

fn verification_key(config: &AppConfig) -> Result<String, ReportError> {
    let mut files = Vec::new();
    for program in [&config.office_worker_path, &config.pdf_worker_path] {
        for path in [
            program.clone(),
            program.with_file_name("pdfium.dll"),
            program.with_file_name("parser-manifest.json"),
        ] {
            let guard = if path.try_exists()? {
                let guard = compute_source_guard(&path)?;
                if guard.kind == SourceGuardKind::Unavailable {
                    return Err(super::error("ARCHIVE_WORKER_IDENTITY_INVALID"));
                }
                serde_json::json!([source_guard_kind_text(guard.kind), guard.guard_sha256])
            } else {
                serde_json::Value::Null
            };
            files.push(serde_json::json!([path, guard]));
        }
    }
    crate::run::hash(&serde_json::json!({
        "version":1, "build":env!("AI_DAILY_REPORT_BUILD"),
        "parser":ai_daily_scanner_core::PARSER_CONTENT_IDENTITY,
        "parameters":parameters(config), "files":files
    }))
}

fn saved_verification(config: &AppConfig, key: &str) -> Option<Verification> {
    // A profile query must not create an archive or write verification state.
    let store = super::store::ArchiveStore::open(config, false).ok()?;
    let text: String = store
        .conn
        .query_row(
            "SELECT value FROM metadata WHERE key=? AND length(value)<16384",
            [VERIFICATION_KEY],
            |r| r.get(0),
        )
        .ok()?;
    let envelope: serde_json::Value = serde_json::from_str(&text).ok()?;
    let proof = envelope.get("proof")?;
    if envelope.get("sha256")?.as_str()? != crate::run::hash(proof).ok()? {
        return None;
    }
    let verified: Verification = serde_json::from_value(proof.clone()).ok()?;
    (verified.key == key).then_some(verified)
}

pub(super) fn verified(config: &AppConfig) -> Result<Verification, ReportError> {
    static RECENT: OnceLock<Mutex<Option<Verification>>> = OnceLock::new();
    let key = verification_key(config)?;
    let mut recent = RECENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| super::error("ARCHIVE_WORKER_IDENTITY_INVALID"))?;
    if let Some(proof) = recent.as_ref().filter(|proof| proof.key == key) {
        return Ok(proof.clone());
    }
    let proof = if let Some(proof) = saved_verification(config, &key) {
        proof
    } else {
        Verification {
            key,
            office: worker_identity(&config.office_worker_path)?,
            pdf: worker_identity(&config.pdf_worker_path)?,
        }
    };
    // File ID and change-time are checked again after reading/hashing the files.
    if proof.key != verification_key(config)? {
        return Err(super::error("ARCHIVE_WORKER_IDENTITY_INVALID"));
    }
    *recent = Some(proof.clone());
    Ok(proof)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Compatibility {
    version: u32,
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    parser_content_identity: String,
    parameters: serde_json::Value,
    legacy_profile: String,
    legacy_engine_build: String,
    office_sha256: String,
    pdf_sha256: String,
    pdfium_sha256: String,
    evidence_sha256: String,
}

pub fn profile(config: &AppConfig) -> Result<String, ReportError> {
    verified(config)?.profile(config)
}

fn profile_from_identities(
    config: &AppConfig,
    office: &str,
    pdf: &str,
) -> Result<String, ReportError> {
    let parameters = parameters(config);
    let compatibility: Compatibility =
        serde_json::from_str(include_str!("parser-compatibility.json"))
            .map_err(|e| ReportError::new("PARSER_COMPATIBILITY_INVALID", e.to_string()))?;
    if compatibility.version != 1 {
        return Err(super::error("PARSER_COMPATIBILITY_INVALID"));
    }
    for entry in compatibility.entries {
        if let Some(profile) = compatible_profile(&entry, &parameters, office, pdf)? {
            return Ok(profile);
        }
    }
    crate::run::hash(&serde_json::json!({
        "archive_parser":2, "parser":ai_daily_scanner_core::PARSER_CONTENT_IDENTITY,
        "office":office,"pdf":pdf,"parameters":parameters
    }))
}

fn compatible_profile(
    entry: &Entry,
    parameters: &serde_json::Value,
    office: &str,
    pdf: &str,
) -> Result<Option<String>, ReportError> {
    if entry.parser_content_identity != ai_daily_scanner_core::PARSER_CONTENT_IDENTITY
        || entry.parameters != *parameters
        || (office != entry.parser_content_identity
            && office != legacy_worker_identity(&entry.office_sha256, &entry.pdfium_sha256)?)
        || (pdf != entry.parser_content_identity
            && pdf != legacy_worker_identity(&entry.pdf_sha256, &entry.pdfium_sha256)?)
    {
        return Ok(None);
    }
    let sha = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    if !sha(&entry.evidence_sha256)
        || !sha(&entry.office_sha256)
        || !sha(&entry.pdf_sha256)
        || !sha(&entry.pdfium_sha256)
    {
        return Err(super::error("PARSER_COMPATIBILITY_INVALID"));
    }
    let mut old = parameters.clone();
    old["archive_parser"] = serde_json::json!(1);
    old["engine"] = serde_json::json!(entry.legacy_engine_build);
    if crate::run::hash(&old)? != entry.legacy_profile {
        return Err(super::error("PARSER_COMPATIBILITY_INVALID"));
    }
    Ok(Some(entry.legacy_profile.clone()))
}

pub fn parameters(config: &AppConfig) -> serde_json::Value {
    serde_json::json!({"protocol":ai_daily_worker_contract::PROTOCOL_VERSION,
        "max_input_mib":config.archive.max_input_mib,"max_output_mib":config.archive.max_output_mib,
        "parse_seconds":config.archive.parse_seconds,"worker_memory_mib":config.archive.worker_memory_mib,
        "legacy_office_enabled":config.scanner.legacy_office_enabled,"csv_encodings":config.scanner.csv_encodings})
}

fn worker_identity(path: &Path) -> Result<WorkerProof, ReportError> {
    match path.try_exists() {
        Ok(false) => {
            return Ok(WorkerProof {
                identity: format!("unavailable:{}", path.display()),
                sha256: None,
                engine_build: None,
            })
        }
        Err(e) => return Err(e.into()),
        Ok(true) => {}
    }
    let invalid = |e| ReportError::new("ARCHIVE_WORKER_IDENTITY_INVALID", e);
    let content = worker_content_identity(path, None).map_err(invalid)?;
    let executable = file_hash(path).map_err(invalid)?;
    let (identity, engine_build) = if let Some(identity) = content {
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.with_file_name("parser-manifest.json"))?)?;
        let build = manifest["engine_build"]
            .as_str()
            .ok_or_else(|| super::error("ARCHIVE_WORKER_IDENTITY_INVALID"))?;
        (identity, Some(build.to_owned()))
    } else {
        let pdfium = file_hash(&path.with_file_name("pdfium.dll")).map_err(invalid)?;
        (legacy_worker_identity(&executable, &pdfium)?, None)
    };
    Ok(WorkerProof {
        identity,
        sha256: Some(executable),
        engine_build,
    })
}

fn legacy_worker_identity(executable: &str, pdfium: &str) -> Result<String, ReportError> {
    crate::run::hash(&serde_json::json!({"executable":executable,"pdfium":pdfium}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, AppConfig) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config/settings.yaml"),
            "paths:\n  work_dir: work\n",
        )
        .unwrap();
        let mut config =
            AppConfig::load_with_env(dir.path(), None, &std::collections::BTreeMap::new()).unwrap();
        config.paths.data_dir = dir.path().join("data");
        config.office_worker_path = dir.path().join("office.exe");
        config.pdf_worker_path = dir.path().join("pdf.exe");
        for path in [&config.office_worker_path, &config.pdf_worker_path] {
            std::fs::write(path, b"original").unwrap();
        }
        std::fs::write(dir.path().join("pdfium.dll"), b"dll").unwrap();
        (dir, config)
    }

    #[test]
    fn verification_reuses_saved_proof_and_rejects_same_size_timestamp_replacement() {
        let (_dir, config) = setup();
        let proof = verified(&config).unwrap();
        let store = super::super::store::ArchiveStore::open(&config, true).unwrap();
        proof.persist(&config, &store).unwrap();
        let changes = store.conn.total_changes();
        store.conn.pragma_update(None, "query_only", true).unwrap();
        proof.persist(&config, &store).unwrap();
        assert_eq!(store.conn.total_changes(), changes);
        store.conn.pragma_update(None, "query_only", false).unwrap();
        let saved = saved_verification(&config, &verification_key(&config).unwrap()).unwrap();
        assert_eq!(
            saved.profile(&config).unwrap(),
            proof.profile(&config).unwrap()
        );
        let path = &config.office_worker_path;
        let modified = std::fs::metadata(path).unwrap().modified().unwrap();
        let replacement = path.with_extension("new");
        std::fs::write(&replacement, b"replaced").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::rename(&replacement, path).unwrap();
        let key = verification_key(&config).unwrap();
        assert_ne!(proof.key, key);
        assert!(saved_verification(&config, &key).is_none());
        assert!(proof.check_worker(&config, false, "legacy").is_err());
        assert_ne!(profile(&config).unwrap(), proof.profile(&config).unwrap());
    }

    #[test]
    fn corrupted_proof_and_changed_inputs_are_not_reused() {
        let (_dir, mut config) = setup();
        let proof = verified(&config).unwrap();
        let store = super::super::store::ArchiveStore::open(&config, true).unwrap();
        proof.persist(&config, &store).unwrap();
        store.conn.execute(
            "UPDATE metadata SET value=json_set(value,'$.proof.office.identity','corrupt') WHERE key=?",
            [VERIFICATION_KEY],
        ).unwrap();
        assert!(saved_verification(&config, &proof.key).is_none());
        proof.persist(&config, &store).unwrap();
        config.archive.max_output_mib += 1;
        assert!(saved_verification(&config, &verification_key(&config).unwrap()).is_none());
        config.archive.max_output_mib -= 1;
        std::fs::write(
            config
                .office_worker_path
                .with_file_name("parser-manifest.json"),
            "{}",
        )
        .unwrap();
        assert!(saved_verification(&config, &verification_key(&config).unwrap()).is_none());
        assert!(verified(&config).is_err());
    }

    #[test]
    fn profile_without_archive_does_not_create_verification_files() {
        let (_dir, config) = setup();
        assert!(!config.paths.data_dir.exists());
        assert_eq!(profile(&config).unwrap(), profile(&config).unwrap());
        assert!(!config.paths.data_dir.exists());
    }

    #[test]
    fn legacy_reuse_is_bound_to_parameters_workers_and_equivalence_evidence() {
        let parameters = json!({"protocol":4,"budget":1});
        let mut old = parameters.clone();
        old["archive_parser"] = json!(1);
        old["engine"] = json!("old-build");
        let mut entry = Entry {
            parser_content_identity: ai_daily_scanner_core::PARSER_CONTENT_IDENTITY.into(),
            parameters: parameters.clone(),
            legacy_profile: crate::run::hash(&old).unwrap(),
            legacy_engine_build: "old-build".into(),
            office_sha256: "a".repeat(64),
            pdf_sha256: "b".repeat(64),
            pdfium_sha256: "d".repeat(64),
            evidence_sha256: "c".repeat(64),
        };
        let current = ai_daily_scanner_core::PARSER_CONTENT_IDENTITY;
        assert_eq!(
            compatible_profile(&entry, &parameters, current, current).unwrap(),
            Some(entry.legacy_profile.clone())
        );
        assert!(compatible_profile(
            &entry,
            &parameters,
            &legacy_worker_identity(&entry.office_sha256, &entry.pdfium_sha256).unwrap(),
            &legacy_worker_identity(&entry.pdf_sha256, &entry.pdfium_sha256).unwrap()
        )
        .unwrap()
        .is_some());
        assert!(
            compatible_profile(&entry, &json!({"protocol":4,"budget":2}), current, current)
                .unwrap()
                .is_none()
        );
        assert!(
            compatible_profile(&entry, &parameters, "unknown-worker", current)
                .unwrap()
                .is_none()
        );
        entry.evidence_sha256.clear();
        assert!(compatible_profile(&entry, &parameters, current, current).is_err());
    }
}
