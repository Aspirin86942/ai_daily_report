//! Scanner evidence normalization, persistence DTO assembly, and stored snapshots.

use ai_daily_scanner_contract::{
    AuditWorkerLane, ContextAction, ContextDecision, ContextProfile, ContextSummary, Diagnostic,
    EngineStatus, ExecutionMetricsV2, ExtensionMetric, FileAudit, NormalizedScannerSettings,
    Nullable, ParseStatus, ParseTransport, PdfClassificationAuditV1, PdfClassificationStatus,
    RunStatus, StageMetric, StageName, Validate,
};
use rusqlite::{OptionalExtension, Transaction};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use thiserror::Error;

use crate::artifact::PdfClassificationProvenanceV1;
use crate::store::{sha256_hex, FileResultRecord, InventoryRecord};

pub fn extension_metrics(
    inventory: &[InventoryRecord],
    file_results: &[FileResultRecord],
) -> Result<Vec<ExtensionMetric>, String> {
    let result_by_identity: HashMap<&str, &FileResultRecord> = file_results
        .iter()
        .map(|result| (result.file_identity.as_str(), result))
        .collect();
    if result_by_identity.len() != file_results.len() {
        return Err("file results contain duplicate identities".to_string());
    }
    let mut grouped: BTreeMap<&str, ExtensionMetric> = BTreeMap::new();
    for item in inventory {
        let result = result_by_identity
            .get(item.file_identity.as_str())
            .ok_or_else(|| "inventory has no matching file result".to_string())?;
        let metric = grouped
            .entry(item.file_type.as_str())
            .or_insert_with(|| ExtensionMetric {
                extension: item.file_type.clone(),
                file_count: 0,
                parse_duration_ms: 0,
                success_count: 0,
                error_count: 0,
                timeout_count: 0,
            });
        metric.file_count = checked_add(metric.file_count, 1, "extension file count")?;
        metric.parse_duration_ms = checked_add(
            metric.parse_duration_ms,
            result.parse_duration_ms,
            "extension duration",
        )?;
        let (count, field) = match result.parse_status {
            ParseStatus::Success => (&mut metric.success_count, "extension success count"),
            ParseStatus::Timeout => (&mut metric.timeout_count, "extension timeout count"),
            ParseStatus::Error => (&mut metric.error_count, "extension error count"),
            ParseStatus::NotParsed => continue,
        };
        *count = checked_add(*count, 1, field)?;
    }
    let metrics: Vec<_> = grouped.into_values().collect();
    for metric in &metrics {
        metric.validate()?;
    }
    Ok(metrics)
}

#[derive(Serialize)]
struct ContextProfileFingerprint<'a> {
    protocol_version: u64,
    engine_build: &'a str,
    context: &'a ContextProfile,
}

#[derive(Serialize)]
struct RejectedProfileFingerprint<'a> {
    protocol_version: u64,
    engine_build: &'a str,
    profile: &'a NormalizedScannerSettings,
}

pub fn context_profile_hash(
    protocol_version: u64,
    engine_build: &str,
    profile: &NormalizedScannerSettings,
) -> Result<String, String> {
    if protocol_version != 1 || engine_build.is_empty() {
        return Err("context profile fingerprint identity is invalid".to_string());
    }
    profile.validate()?;
    let canonical = serde_json::to_vec(&ContextProfileFingerprint {
        protocol_version,
        engine_build,
        context: &profile.context,
    })
    .map_err(|error| error.to_string())?;
    Ok(crate::store::cache::domain_hash(
        b"context-profile-v1\0",
        &canonical,
    ))
}

pub fn rejected_profile_hash(
    protocol_version: u64,
    engine_build: &str,
    profile: &NormalizedScannerSettings,
) -> Result<String, String> {
    if protocol_version != 1 || engine_build.is_empty() {
        return Err("rejected profile fingerprint identity is invalid".to_string());
    }
    profile.validate()?;
    let canonical = serde_json::to_vec(&RejectedProfileFingerprint {
        protocol_version,
        engine_build,
        profile,
    })
    .map_err(|error| error.to_string())?;
    Ok(crate::store::cache::domain_hash(
        b"rejected-profile-v1\0",
        &canonical,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageMetricInputs {
    pub source_file_count: u64,
    pub cache_item_count: u64,
    pub parsed_item_count: u64,
    pub context_item_count: u64,
    pub discovery_duration_ms: u64,
    pub cache_duration_ms: u64,
    pub parse_duration_ms: u64,
    pub context_duration_ms: u64,
}

pub fn stage_metrics(input: StageMetricInputs) -> Vec<StageMetric> {
    vec![
        StageMetric {
            stage: StageName::Discovery,
            item_count: input.source_file_count,
            duration_ms: input.discovery_duration_ms,
        },
        StageMetric {
            stage: StageName::Cache,
            item_count: input.cache_item_count,
            duration_ms: input.cache_duration_ms,
        },
        StageMetric {
            stage: StageName::Parse,
            item_count: input.parsed_item_count,
            duration_ms: input.parse_duration_ms,
        },
        StageMetric {
            stage: StageName::Context,
            item_count: input.context_item_count,
            duration_ms: input.context_duration_ms,
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceSnapshot {
    pub context_run_id: Option<u64>,
    pub run_status: RunStatus,
    pub summary: ContextSummary,
    pub stage_metrics: Vec<StageMetric>,
    pub extension_metrics: Vec<ExtensionMetric>,
    pub decisions: Vec<ContextDecision>,
    pub warnings: Vec<Diagnostic>,
    pub artifact_id: Option<u64>,
    pub reused_from_context_run_id: Option<u64>,
    pub snapshot_hit: bool,
    /// `reserved_chars`/`rendered_chars` reconstructed from the artifact's
    /// immutable semantic summary (spec Part 5.3 `execution_metrics`).
    pub reserved_chars: u64,
    pub rendered_chars: u64,
    /// WorkDeadline run-level trigger fired during this run (0 or 1).
    pub stage_deadline_exhausted: bool,
    /// Per-file rows with source-guard and classifier provenance.
    /// Empty for nonterminal rows.
    pub files_v2: Vec<FileAuditV2Source>,
    /// Authoritative `execution_metrics` persisted at finalize (spec Part 5.3).
    /// `None` for nonterminal rows and engine-error runs that had no scheduler
    /// outcome.
    pub execution_metrics: Option<ExecutionMetricsV2>,
}

/// Per-file SourceGuardV2 + classifier provenance used to assemble
/// `FileAuditV2` (spec Part 5.3). Migrated v1 rows carry null guards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAuditV2Source {
    pub relative_path: String,
    pub file_identity: String,
    pub source_version: String,
    pub source_guard_kind: Option<String>,
    pub source_guard_sha256: Option<String>,
    pub file_type: String,
    pub size_bytes: u64,
    pub parse_status: ParseStatus,
    pub parser_backend: String,
    pub worker_lane: AuditWorkerLane,
    pub parse_cache_status: Option<String>,
    pub cache_miss_reason: String,
    pub truncated: bool,
    pub content_sha256: String,
    pub parse_duration_ms: u64,
    pub failure_class: String,
    pub fallback_backend: String,
    pub fallback_reason_code: String,
    /// Missing provenance fails closed when current evidence is assembled.
    pub parse_transport: Option<ParseTransport>,
    pub parse_attempt_count: Option<u64>,
    pub final_diagnostic: Option<Diagnostic>,
    pub classifier: Option<PdfClassificationProvenanceV1>,
    pub classification_execution: Option<PdfClassificationAuditV1>,
}

/// Assembles the strict `execution_metrics` object (spec Part 5.3) from the
/// persisted full_v2 run. Counts that the scheduler owns and that cannot be
/// reconstructed from persisted rows are only populated for non-snapshot runs
/// via best-effort derivation from the per-file audit rows; a snapshot-hit
/// current run legitimately reports 0 for every plan/execution count because
/// the scheduler did not run. The two `*_all_hit` nullables follow the
/// `lookup_count=0 -> null` rule.
pub fn assemble_execution_metrics_v2(snapshot: &EvidenceSnapshot) -> ExecutionMetricsV2 {
    let discovery = snapshot
        .stage_metrics
        .iter()
        .find(|row| row.stage == StageName::Discovery);
    let mut metrics = ExecutionMetricsV2 {
        discovery_observed_file_count: discovery
            .map_or(snapshot.summary.source_file_count, |row| row.item_count),
        discovery_ms: discovery.map_or(snapshot.summary.discovery_duration_ms, |row| {
            row.duration_ms
        }),
        snapshot_hit: snapshot.snapshot_hit,
        stage_deadline_exhausted_count: u64::from(snapshot.stage_deadline_exhausted),
        reserved_chars: snapshot.reserved_chars,
        rendered_chars: snapshot.rendered_chars,
        ..Default::default()
    };
    let mut fresh = 0;
    for file in &snapshot.files_v2 {
        match file.source_guard_kind.as_deref() {
            Some("content_sha256_v1") => {
                metrics.source_guard_content_hash_file_count += 1;
                metrics.source_guard_bytes_read = metrics
                    .source_guard_bytes_read
                    .saturating_add(file.size_bytes);
            }
            Some("unavailable") => metrics.source_guard_unavailable_count += 1,
            _ => {}
        }
        let cache = file.parse_cache_status.as_deref();
        if matches!(cache, Some("fresh" | "miss")) {
            metrics.parse_cache_lookup_count += 1;
            fresh += u64::from(cache == Some("fresh"));
        }
        if cache == Some("miss") {
            metrics.pdfplumber_invocations += u64::from(file.parser_backend == "rust_pdfium_v3");
            metrics.parse_attempt_count += u64::from(matches!(
                file.parse_status,
                ParseStatus::Success | ParseStatus::Error | ParseStatus::Timeout
            ));
        }
        // A whole-context snapshot does no scheduling in the replaying run.
        if snapshot.snapshot_hit {
            continue;
        }
        metrics.candidate_file_count += 1;
        metrics.admitted_file_count +=
            u64::from(file.parser_backend != "not_parsed" || cache == Some("fresh"));
        if let Some(classifier) = &file.classifier {
            metrics.classification_slot_count += 1;
            metrics.nominal_charged_pages_total = metrics
                .nominal_charged_pages_total
                .saturating_add(classifier.nominal_charged_pages);
            if cache != Some("snapshot") {
                metrics.classification_cache_lookup_count += 1;
                metrics.classify_attempt_count += 1;
            }
            metrics.extraction_slot_count += u64::from(matches!(
                classifier.status,
                PdfClassificationStatus::TextInParseWindow
                    | PdfClassificationStatus::NoTextInParseWindow
            ));
        }
    }
    metrics.parse_cache_all_hit = Nullable(
        (metrics.parse_cache_lookup_count > 0).then_some(fresh == metrics.parse_cache_lookup_count),
    );
    // Old rows cannot establish that all classification lookups were hits.
    metrics.classification_cache_all_hit =
        Nullable((metrics.classification_cache_lookup_count > 0).then_some(false));
    metrics
}

#[derive(Debug, Error)]
pub enum EvidenceAuditError {
    #[error("scanner run was not found")]
    RunNotFound,
    #[error("persisted scanner run is corrupt: {0}")]
    RunCorrupt(String),
    #[error("scanner database could not be read")]
    Sql(#[from] rusqlite::Error),
}

#[derive(Debug)]
pub struct EvidenceLoadError {
    pub error: EvidenceAuditError,
    pub run_status: Option<RunStatus>,
}

impl EvidenceLoadError {
    fn before_status(error: EvidenceAuditError) -> Self {
        Self {
            error,
            run_status: None,
        }
    }

    fn after_status(error: EvidenceAuditError, run_status: RunStatus) -> Self {
        Self {
            error,
            run_status: Some(run_status),
        }
    }
}

#[derive(Debug)]
struct PersistedContext {
    context_run_id: u64,
    status: RunStatus,
    final_context: String,
    summary: ContextSummary,
}

#[derive(Debug)]
struct PersistedDecision {
    file_identity: String,
    decision: ContextDecision,
}

fn to_positive_u64(value: Option<i64>, field: &str) -> Result<Option<u64>, EvidenceAuditError> {
    value.map(|n| to_u64(n, field)).transpose()
}

fn to_u64_opt(value: Option<i64>, field: &str) -> Result<u64, EvidenceAuditError> {
    let value = value
        .ok_or_else(|| EvidenceAuditError::RunCorrupt(format!("persisted {field} is missing")))?;
    to_u64(value, field)
}

fn parse_run_status(value: &str) -> Result<RunStatus, String> {
    parse_enum(value, "run status").map_err(|_| "run status is invalid".into())
}

fn parse_enum<T: serde::de::DeserializeOwned>(
    value: &str,
    field: &str,
) -> Result<T, EvidenceAuditError> {
    T::deserialize(serde::de::value::BorrowedStrDeserializer::<
        serde::de::value::Error,
    >::new(value))
    .map_err(|_| EvidenceAuditError::RunCorrupt(format!("persisted {field} is invalid")))
}

fn parse_bool(value: i64, field: &str) -> Result<bool, EvidenceAuditError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(EvidenceAuditError::RunCorrupt(format!(
            "persisted {field} is invalid"
        ))),
    }
}

fn to_u64(value: i64, field: &str) -> Result<u64, EvidenceAuditError> {
    u64::try_from(value)
        .map_err(|_| EvidenceAuditError::RunCorrupt(format!("persisted {field} is negative")))
}

pub(crate) fn relative_contract_path(
    work_dir: &Path,
    absolute_path: &str,
) -> Result<String, String> {
    let root_text = ai_daily_discovery::normalize_contract_path_text(&work_dir.to_string_lossy());
    let path_text = ai_daily_discovery::normalize_contract_path_text(absolute_path);
    let absolute = Path::new(&path_text);
    let relative = absolute
        .strip_prefix(Path::new(&root_text))
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(|| {
            let root = root_text.replace('/', "\\");
            let path = path_text.replace('/', "\\");
            let root = root.trim_end_matches('\\');
            if path.len() > root.len()
                && path
                    .get(..root.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(root))
                && path.as_bytes().get(root.len()) == Some(&b'\\')
            {
                path.get(root.len() + 1..).map(ToOwned::to_owned)
            } else {
                None
            }
        })
        .ok_or_else(|| "discovered file is outside work_dir".to_string())?;
    let relative = relative.replace('/', "\\");
    if relative.is_empty()
        || relative.starts_with('\\')
        || relative.split('\\').any(|component| component == "..")
    {
        return Err("discovered relative path is unsafe".to_string());
    }
    Ok(relative)
}

fn checked_add(left: u64, right: u64, field: &str) -> Result<u64, String> {
    left.checked_add(right)
        .ok_or_else(|| format!("{field} overflows"))
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[cfg(test)]
mod path_tests;

mod snapshot;
pub(crate) use snapshot::load_evidence_snapshot;
mod files;
use files::load_file_audits_v2;
mod relations;
use relations::{
    load_context_decisions, load_context_row, load_extension_metrics, load_file_audits,
    load_run_diagnostics, load_stage_metrics, validate_relational_summary,
};
