//! Context artifact relational model, envelope rebuild, and snapshot identity
//! (spec Part 5.1 / 5.4).
//!
//! The artifact is the immutable, de-duplicated storage of `final_context`:
//! every Success/Partial run references one artifact, and only
//! `snapshot_eligible` artifacts carry a snapshot key plus per-source-file
//! semantic rows. `snapshot_key` derives a canonical JSON covering the full
//! provenance and returns a domain-separated SHA-256; the store compares the
//! canonical JSON byte-for-byte on hit (never trusts the hash alone).
//! `rebuild_envelope` reconstructs and re-validates the frozen `ContextEnvelope
//! v1` from `final_envelope_metadata_json` + the current `context_runs`
//! summary + the artifact's `final_context`.

use ai_daily_discovery::{DiscoveredFileOut, DiscoveryIssue};
use ai_daily_scanner_contract::{
    BuildContextRequest, ContextAction, ContextEnvelope, ContextSummary, Diagnostic, EngineStatus,
    NormalizedScannerProfileV2, Nullable, ParseStatus, PdfClassificationStatus, ReportMode,
    Validate,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::scheduler::WorkerIdentities;

/// Snapshot-key canonical payload version. Change it when the component set or
/// ordering changes so stale keys can never collide with new ones.
pub const SNAPSHOT_KEY_VERSION: &str = "snapshot_key_v1";
/// Domain separator for the snapshot-key SHA-256 (spec Part 5.4).
pub const SNAPSHOT_KEY_DOMAIN: &[u8] = b"snapshot-key-v1\0";
/// Frozen classifier contract version entering the snapshot key.
pub const CLASSIFIER_CONTRACT_VERSION: &str = "ai_daily_pdf_classifier_v1";
/// Frozen Python session contract version used as the session marker.
pub const SESSION_CONTRACT_VERSION: &str = "ai_daily_python_session_v1";

/// Immutable semantic summary persisted with the artifact (spec Part 5.1):
/// counts + input/output/reserved/rendered chars. No timings, no request_id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticSummary {
    pub source_file_count: u64,
    pub success_count: u64,
    pub timeout_count: u64,
    pub included_file_count: u64,
    pub omitted_file_count: u64,
    pub error_file_count: u64,
    pub input_chars: u64,
    pub output_chars: u64,
    pub reserved_chars: u64,
    pub rendered_chars: u64,
}

impl SemanticSummary {
    /// The same count equation as the `ContextSummary` contract
    /// (`success + timeout + error <= source_file_count`).
    pub fn validate_counts(&self) -> Result<(), String> {
        let classified = self
            .success_count
            .checked_add(self.timeout_count)
            .and_then(|value| value.checked_add(self.error_file_count))
            .ok_or_else(|| "semantic summary counts overflow".to_string())?;
        if classified <= self.source_file_count {
            Ok(())
        } else {
            Err("semantic summary counts exceed source_file_count".to_string())
        }
    }
}

/// Immutable classifier provenance subset stored on artifact file rows
/// (spec Part 3.2). Never carries cache status / miss reason / run-inspected
/// pages / duration / transport / attempt — those are current-run execution
/// fields and cannot be reused across runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfClassificationProvenanceV1 {
    pub status: PdfClassificationStatus,
    pub page_count: Option<u64>,
    pub result_examined_pages: Option<u64>,
    pub nominal_charged_pages: u64,
    pub classifier_build: String,
    pub classifier_profile_hash: String,
}

/// Immutable per-source-file artifact row (spec Part 5.1 `context_artifact_files`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFileRow {
    pub file_identity: String,
    pub relative_path: String,
    /// Frozen worker-v1 legacy source version (`mtime_ns=...:size=...`).
    pub legacy_source_version: String,
    pub source_guard_kind: Option<String>,
    pub source_guard_sha256: Option<String>,
    pub parse_profile_hash: String,
    pub parse_status: ParseStatus,
    pub parser_backend: String,
    pub worker_lane: String,
    pub truncated: bool,
    pub content_sha256: String,
    /// Nullable classifier provenance (spec Part 3.2 immutable subset).
    pub classifier: Option<PdfClassificationProvenanceV1>,
}

/// Immutable per-source-file artifact decision row
/// (spec Part 5.1 `context_artifact_decisions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDecisionRow {
    pub file_identity: String,
    pub relative_path: String,
    pub action: ContextAction,
    pub reason: String,
    pub priority: u64,
    pub input_chars: u64,
    pub output_chars: u64,
    pub truncated: bool,
    pub error_code: String,
}

/// Ready-to-persist artifact (spec Part 5.1). Construction enforces the
/// eligible/ineligible two-directional row constraint and the context hash:
/// an eligible artifact must carry exactly one file and decision row per
/// source file (1:1 on `file_identity`), while an ineligible payload artifact
/// must carry none. `context_sha256` is always recomputed and must equal
/// `SHA-256(final_context)`; the store re-validates it again at replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactDraft {
    /// `true` when this artifact may serve as a snapshot source (spec Part 5.4):
    /// it carries a snapshot key plus per-source-file semantic rows.
    pub snapshot_eligible: bool,
    pub final_context: String,
    pub context_sha256: String,
    pub semantic_summary: SemanticSummary,
    pub file_rows: Vec<ArtifactFileRow>,
    pub decision_rows: Vec<ArtifactDecisionRow>,
}

impl ArtifactDraft {
    pub fn new(
        snapshot_eligible: bool,
        final_context: String,
        semantic_summary: SemanticSummary,
        file_rows: Vec<ArtifactFileRow>,
        decision_rows: Vec<ArtifactDecisionRow>,
    ) -> Result<ArtifactDraft, String> {
        semantic_summary.validate_counts()?;
        let context_sha256 = sha256_hex(final_context.as_bytes());
        if snapshot_eligible {
            let expected = usize::try_from(semantic_summary.source_file_count)
                .map_err(|_| "source_file_count exceeds platform usize".to_string())?;
            if file_rows.len() != expected || decision_rows.len() != expected {
                return Err(format!(
                    "eligible artifact requires one file and decision row per source file \
                     (source_file_count={expected}, file_rows={}, decision_rows={})",
                    file_rows.len(),
                    decision_rows.len()
                ));
            }
            let mut files: Vec<&str> = file_rows
                .iter()
                .map(|row| row.file_identity.as_str())
                .collect();
            let mut decisions: Vec<&str> = decision_rows
                .iter()
                .map(|row| row.file_identity.as_str())
                .collect();
            files.sort_unstable();
            decisions.sort_unstable();
            if files != decisions {
                return Err(
                    "eligible artifact file rows and decision rows must be 1:1 per file"
                        .to_string(),
                );
            }
        } else if !file_rows.is_empty() || !decision_rows.is_empty() {
            return Err(
                "ineligible artifact must carry no file or decision rows".to_string(),
            );
        }
        Ok(ArtifactDraft {
            snapshot_eligible,
            final_context,
            context_sha256,
            semantic_summary,
            file_rows,
            decision_rows,
        })
    }
}

/// Classifier identity entering the snapshot key (spec Part 5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifierIdentity {
    pub contract: String,
    pub build: String,
    pub profile_hash: String,
}

/// The domain-separated snapshot-key hash plus the canonical JSON. The hash is
/// only an index; a hit requires a byte-exact `canonical_json` comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotKeyParts {
    pub sha256: String,
    pub canonical_json: String,
}

#[derive(Serialize)]
struct SnapshotKeyPayload<'a> {
    snapshot_key_version: &'static str,
    logical_request: serde_json::Value,
    discovery: &'a [DiscoveredFileOut],
    discovery_issues: &'a [DiscoveryIssue],
    profile: &'a NormalizedScannerProfileV2,
    report_mode: ReportMode,
    engine_build: &'a str,
    workers: WorkerCanonical<'a>,
    session: SessionCanonical<'a>,
    classifier: ClassifierCanonical<'a>,
}

#[derive(Serialize)]
struct WorkerCanonical<'a> {
    office_contract: Option<&'a str>,
    office_version: Option<&'a str>,
    office_build: Option<&'a str>,
    python_contract: Option<&'a str>,
    python_version: Option<&'a str>,
    python_build: Option<&'a str>,
}

#[derive(Serialize)]
struct SessionCanonical<'a> {
    capability: &'static str,
    contract: Option<&'a str>,
    version: Option<&'a str>,
    build: Option<&'a str>,
}

#[derive(Serialize)]
struct ClassifierCanonical<'a> {
    contract: &'a str,
    build: &'a str,
    profile_hash: &'a str,
}

/// Canonical snapshot-key JSON (spec Part 5.4). The payload is serialized from
/// a fixed field-order struct so the bytes are stable across runs; the
/// logical request is serialized first and its `request_id` removed.
pub fn snapshot_key_parts(
    logical_request: &BuildContextRequest,
    discovery: &[DiscoveredFileOut],
    issues: &[DiscoveryIssue],
    profile: &NormalizedScannerProfileV2,
    engine_build: &str,
    worker_ids: &WorkerIdentities,
    classifier_ids: &ClassifierIdentity,
) -> Result<SnapshotKeyParts, String> {
    let mut request_value = serde_json::to_value(logical_request)
        .map_err(|error| format!("logical request cannot be canonicalized: {error}"))?;
    request_value
        .as_object_mut()
        .ok_or_else(|| "logical request is not a JSON object".to_string())?
        .remove("request_id");

    let session = if worker_ids.python_contract.is_some() {
        SessionCanonical {
            capability: "session",
            contract: worker_ids.python_contract.as_deref(),
            version: worker_ids.python_version.as_deref(),
            build: worker_ids.python_build.as_deref(),
        }
    } else {
        SessionCanonical {
            capability: "one_shot",
            contract: None,
            version: None,
            build: None,
        }
    };

    let payload = SnapshotKeyPayload {
        snapshot_key_version: SNAPSHOT_KEY_VERSION,
        logical_request: request_value,
        discovery,
        discovery_issues: issues,
        profile,
        report_mode: profile.report_mode,
        engine_build,
        workers: WorkerCanonical {
            office_contract: worker_ids.office_contract.as_deref(),
            office_version: worker_ids.office_version.as_deref(),
            office_build: worker_ids.office_build.as_deref(),
            python_contract: worker_ids.python_contract.as_deref(),
            python_version: worker_ids.python_version.as_deref(),
            python_build: worker_ids.python_build.as_deref(),
        },
        session,
        classifier: ClassifierCanonical {
            contract: &classifier_ids.contract,
            build: &classifier_ids.build,
            profile_hash: &classifier_ids.profile_hash,
        },
    };
    let canonical_bytes = serde_json::to_vec(&payload)
        .map_err(|error| format!("snapshot key cannot be canonicalized: {error}"))?;
    let canonical_json = String::from_utf8(canonical_bytes.clone())
        .map_err(|error| format!("snapshot key canonical JSON is not UTF-8: {error}"))?;
    Ok(SnapshotKeyParts {
        sha256: snapshot_domain_hash(&canonical_bytes),
        canonical_json,
    })
}

/// Domain-separated SHA-256 snapshot-key index (spec Part 5.4). Hit detection
/// must also compare `snapshot_key_parts(...).canonical_json` byte-for-byte.
pub fn snapshot_key(
    logical_request: &BuildContextRequest,
    discovery: &[DiscoveredFileOut],
    issues: &[DiscoveryIssue],
    profile: &NormalizedScannerProfileV2,
    engine_build: &str,
    worker_ids: &WorkerIdentities,
    classifier_ids: &ClassifierIdentity,
) -> Result<String, String> {
    Ok(snapshot_key_parts(
        logical_request,
        discovery,
        issues,
        profile,
        engine_build,
        worker_ids,
        classifier_ids,
    )?
    .sha256)
}

fn snapshot_domain_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SNAPSHOT_KEY_DOMAIN);
    hasher.update(bytes);
    hex_bytes(&hasher.finalize())
}

/// Rebuilds and re-validates the frozen `ContextEnvelope v1` from the stored
/// `final_envelope_metadata_json` (small request/engine/status/warnings/error
/// fields), the current `context_runs` summary, and the artifact's
/// `final_context` (spec Part 5.1). Success/Partial runs pass
/// `Some(artifact)`; Error runs have no artifact and rebuild an empty context.
pub fn rebuild_envelope(
    metadata: &serde_json::Value,
    current_summary: &ContextSummary,
    artifact: Option<&ArtifactDraft>,
) -> Result<ContextEnvelope, String> {
    let contract = metadata
        .get("contract")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "envelope metadata missing 'contract'".to_string())?
        .to_string();
    let protocol_version = metadata
        .get("protocol_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "envelope metadata missing 'protocol_version'".to_string())?;
    let request_id = metadata
        .get("request_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "envelope metadata missing 'request_id'".to_string())?
        .to_string();
    let engine_version = metadata
        .get("engine_version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "envelope metadata missing 'engine_version'".to_string())?
        .to_string();
    let engine_build = metadata
        .get("engine_build")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "envelope metadata missing 'engine_build'".to_string())?
        .to_string();
    let status: EngineStatus = serde_json::from_value(
        metadata
            .get("status")
            .cloned()
            .ok_or_else(|| "envelope metadata missing 'status'".to_string())?,
    )
    .map_err(|error| format!("envelope metadata 'status' is invalid: {error}"))?;
    let scan_run_id = nullable_field(metadata, "scan_run_id")?;
    let context_run_id = nullable_field(metadata, "context_run_id")?;
    let warnings: Vec<Diagnostic> = match metadata.get("warnings") {
        None => Vec::new(),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|error| format!("envelope metadata 'warnings' is invalid: {error}"))?,
    };
    let error: Nullable<Diagnostic> = match metadata.get("error") {
        None | Some(serde_json::Value::Null) => Nullable(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Nullable)
            .map_err(|error| format!("envelope metadata 'error' is invalid: {error}"))?,
    };

    let file_context = artifact
        .map(|draft| draft.final_context.clone())
        .unwrap_or_default();
    let envelope = ContextEnvelope {
        contract,
        protocol_version,
        request_id,
        engine_version,
        engine_build,
        status,
        file_context,
        summary: current_summary.clone(),
        scan_run_id,
        context_run_id,
        warnings,
        error,
    };
    envelope.validate()?;
    Ok(envelope)
}

fn nullable_field<T>(metadata: &serde_json::Value, field: &str) -> Result<Nullable<T>, String>
where
    T: serde::de::DeserializeOwned,
{
    match metadata.get(field) {
        None | Some(serde_json::Value::Null) => Ok(Nullable(None)),
        Some(value) => serde_json::from_value(value.clone())
            .map(Nullable)
            .map_err(|error| format!("envelope metadata '{field}' is invalid: {error}")),
    }
}

/// SHA-256 of `bytes` as 64 lowercase hex characters.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_bytes(&digest)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::normalize_scanner_profile_v2;
    use ai_daily_scanner_contract::{RawScannerProfileV2, ScannerProfile};

    #[test]
    fn rebuild_envelope_error_run_requires_no_artifact() {
        let metadata = serde_json::json!({
            "contract": "ai_daily_context",
            "protocol_version": 1,
            "request_id": "11111111-1111-4111-8111-111111111111",
            "engine_version": "test",
            "engine_build": "build",
            "status": "error",
            "scan_run_id": 1,
            "context_run_id": null,
            "warnings": [],
            "error": {
                "error_code": "PARSER_FAILED",
                "message": "cannot start",
                "retryable": false,
                "stage": "parse",
                "file_path": null,
                "backend": null
            }
        });
        let summary = ContextSummary {
            source_file_count: 0,
            success_count: 0,
            timeout_count: 0,
            included_file_count: 0,
            omitted_file_count: 0,
            error_file_count: 0,
            input_chars: 0,
            output_chars: 0,
            total_duration_ms: 1,
            discovery_duration_ms: 0,
            parse_duration_ms: 0,
            compression_duration_ms: 0,
        };
        let envelope = rebuild_envelope(&metadata, &summary, None).expect("error rebuild");
        assert_eq!(envelope.file_context, "");
        envelope.validate().expect("validated");
    }

    #[test]
    fn snapshot_key_version_is_part_of_the_payload() {
        let profile = normalize_scanner_profile_v2(
            &ScannerProfile::V2(
                serde_json::from_value::<RawScannerProfileV2>(serde_json::json!({
                    "schema_version": "scanner_profile_v2"
                }))
                .unwrap(),
            ),
            ReportMode::Daily,
        )
        .unwrap();
        let request: BuildContextRequest =
            serde_json::from_value(serde_json::json!({
                "contract": "ai_daily_context",
                "protocol_version": 1,
                "request_id": "11111111-1111-4111-8111-111111111111",
                "work_dir": "C:\\work",
                "start_date": "2026-07-14",
                "end_date": "2026-07-15",
                "report_mode": "daily",
                "compression_profile": null,
                "scan_db_path": "C:\\state\\index.sqlite3",
                "scanner_profile": {"schema_version": "scanner_profile_v2"},
                "adapters": {
                    "office_worker_path": "C:\\bin\\office.exe",
                    "python_executable": "C:\\venv\\python.exe",
                    "python_module_root": "C:\\repo",
                    "python_document_worker_module": "src.workers.document_parser_worker"
                }
            }))
            .unwrap();
        let workers = WorkerIdentities::default();
        let classifier = ClassifierIdentity {
            contract: CLASSIFIER_CONTRACT_VERSION.to_string(),
            build: "a".repeat(64),
            profile_hash: "b".repeat(64),
        };
        let parts = snapshot_key_parts(&request, &[], &[], &profile, "build", &workers, &classifier)
            .expect("key parts");
        let value: serde_json::Value =
            serde_json::from_str(&parts.canonical_json).expect("canonical json parses");
        assert_eq!(value["snapshot_key_version"], SNAPSHOT_KEY_VERSION);
        assert!(value.get("logical_request").and_then(|v| v.get("request_id")).is_none());
        assert_eq!(value["report_mode"], "daily");
    }
}
