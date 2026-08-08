//! Scanner evidence normalization, persistence DTO assembly, and inspect snapshots.

use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheMissReason, CacheStatus, ContextAction, ContextDecision, ContextEnvelope,
    ContextProfile, ContextSummary, Diagnostic, DiagnosticStage, EngineStatus, ErrorCode,
    ExtensionMetric, FileAudit, NormalizedScannerProfileV1, Nullable, ParseStatus, RunStatus,
    StageMetric, StageName, Validate,
};
use rusqlite::{OptionalExtension, Transaction};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use thiserror::Error;

use crate::classifier::ClassificationError;
use crate::decision::ContextFileEvidence;
use crate::parsers::{worker_diagnostic_to_scanner, ParsedPayload, ScheduledFileParse};
use crate::planner::PlanAction;
use crate::store::{
    sha256_hex, CacheAwarePlanEntry, CacheLookup, CacheWriteRecord, FileResultRecord,
    InventoryRecord,
};

pub const SANITIZED_FIXTURE_APPLICATION_ID: i64 = 0x4149_4446;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalParserFingerprint {
    pub contract: String,
    pub version: String,
    pub build: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanAuditBundle {
    pub inventory: Vec<InventoryRecord>,
    pub cache_writes: Vec<CacheWriteRecord>,
    pub file_results: Vec<FileResultRecord>,
    pub context_evidence: Vec<ContextFileEvidence>,
    pub degradation_diagnostics: Vec<Diagnostic>,
}

pub fn assemble_scan_audit(
    cache_plan: Vec<CacheAwarePlanEntry>,
    parsed: Vec<ScheduledFileParse>,
    work_dir: &Path,
    rejected_profile_hash: &str,
    local_parser: &LocalParserFingerprint,
) -> Result<ScanAuditBundle, String> {
    if !is_sha256(rejected_profile_hash)
        || local_parser.contract.is_empty()
        || local_parser.version.is_empty()
        || local_parser.build.is_empty()
    {
        return Err("scanner audit fingerprints are invalid".to_string());
    }
    let mut parsed_by_identity = HashMap::new();
    for result in parsed {
        let identity = result.file.file_identity.clone();
        if parsed_by_identity.insert(identity, result).is_some() {
            return Err("parser returned a duplicate file identity".to_string());
        }
    }

    let mut inventory = Vec::with_capacity(cache_plan.len());
    let mut cache_writes = Vec::new();
    let mut file_results = Vec::with_capacity(cache_plan.len());
    let mut context_evidence = Vec::with_capacity(cache_plan.len());
    let mut degradation_diagnostics = Vec::new();

    for entry in cache_plan {
        let relative_path = relative_contract_path(work_dir, &entry.planned.file.path)?;
        let inventory_record =
            InventoryRecord::from_discovered(&entry.planned.file, relative_path.clone())?;
        let file_identity = entry.planned.file.file_identity.clone();
        let source_version = entry.planned.file.source_version.clone();
        let extension = entry.planned.file.extension.clone();
        let absolute_path = entry.planned.file.path.clone();

        match (entry.planned.action, entry.cache_lookup) {
            (PlanAction::Reject(reason), None) => {
                let diagnostic = classification_diagnostic(reason, &absolute_path);
                let content_hash = sha256_hex(b"");
                file_results.push(FileResultRecord {
                    file_identity: file_identity.clone(),
                    relative_path: relative_path.clone(),
                    source_version: source_version.clone(),
                    parse_profile_hash: rejected_profile_hash.to_string(),
                    cache_status: CacheStatus::Miss,
                    cache_miss_reason: CacheMissReason::NewFile,
                    parse_status: ParseStatus::NotParsed,
                    parser_backend: "not_parsed".to_string(),
                    worker_lane: AuditWorkerLane::NotParsed,
                    truncated: false,
                    content_sha256: content_hash,
                    primary_duration_ms: 0,
                    fallback_duration_ms: 0,
                    parse_duration_ms: 0,
                    failure_class: "deterministic".to_string(),
                    fallback_backend: String::new(),
                    fallback_reason_code: String::new(),
                    error: Some(diagnostic.clone()),
                });
                context_evidence.push(ContextFileEvidence {
                    file_identity: file_identity.clone(),
                    absolute_path,
                    relative_path,
                    extension,
                    size_bytes: Some(entry.planned.file.size_bytes),
                    content: String::new(),
                    parser_backend: "not_parsed".to_string(),
                    worker_lane: AuditWorkerLane::NotParsed,
                    cache_status: CacheStatus::Miss,
                    parse_status: ParseStatus::NotParsed,
                    truncated: false,
                    error: Some(diagnostic.clone()),
                    reason: None,
                });
                degradation_diagnostics.push(diagnostic);
            }
            (PlanAction::Parse(_), Some(CacheLookup::Fresh(cached))) => {
                let parse_profile_hash = entry
                    .parse_profile_hash
                    .ok_or_else(|| "fresh cache entry has no profile hash".to_string())?;
                let worker_lane = parse_worker_lane(&cached.worker_lane)?;
                file_results.push(FileResultRecord {
                    file_identity: file_identity.clone(),
                    relative_path: relative_path.clone(),
                    source_version: source_version.clone(),
                    parse_profile_hash,
                    cache_status: CacheStatus::Fresh,
                    cache_miss_reason: CacheMissReason::None,
                    parse_status: ParseStatus::Success,
                    parser_backend: cached.parser_backend.clone(),
                    worker_lane,
                    truncated: cached.truncated,
                    content_sha256: cached.content_sha256.clone(),
                    primary_duration_ms: 0,
                    fallback_duration_ms: 0,
                    parse_duration_ms: 0,
                    failure_class: String::new(),
                    fallback_backend: String::new(),
                    fallback_reason_code: String::new(),
                    error: None,
                });
                context_evidence.push(ContextFileEvidence {
                    file_identity: file_identity.clone(),
                    absolute_path,
                    relative_path,
                    extension,
                    size_bytes: Some(entry.planned.file.size_bytes),
                    content: cached.content,
                    parser_backend: cached.parser_backend,
                    worker_lane,
                    cache_status: CacheStatus::Fresh,
                    parse_status: ParseStatus::Success,
                    truncated: cached.truncated,
                    error: None,
                    reason: None,
                });
            }
            (PlanAction::Parse(_), Some(CacheLookup::Miss(reason))) => {
                let parse_profile_hash = entry
                    .parse_profile_hash
                    .ok_or_else(|| "cache miss has no profile hash".to_string())?;
                let parsed = parsed_by_identity
                    .remove(&file_identity)
                    .ok_or_else(|| "cache miss has no parser result".to_string())?;
                if parsed.file.source_version != source_version
                    || parsed.file.path != absolute_path
                    || parsed.file.extension != extension
                {
                    return Err("parser result identity changed after planning".to_string());
                }
                let normalized = normalize_parser_result(
                    parsed,
                    &parse_profile_hash,
                    reason,
                    &relative_path,
                    local_parser,
                )?;
                degradation_diagnostics.extend(normalized.diagnostics);
                if let Some(cache_write) = normalized.cache_write {
                    cache_writes.push(cache_write);
                }
                file_results.push(normalized.file_result);
                context_evidence.push(normalized.context_evidence);
            }
            _ => return Err("cache-aware plan shape is inconsistent".to_string()),
        }
        inventory.push(inventory_record);
    }
    if !parsed_by_identity.is_empty() {
        return Err("parser returned results that were not cache misses".to_string());
    }
    Ok(ScanAuditBundle {
        inventory,
        cache_writes,
        file_results,
        context_evidence,
        degradation_diagnostics,
    })
}

struct NormalizedParserResult {
    cache_write: Option<CacheWriteRecord>,
    file_result: FileResultRecord,
    context_evidence: ContextFileEvidence,
    diagnostics: Vec<Diagnostic>,
}

fn normalize_parser_result(
    parsed: ScheduledFileParse,
    parse_profile_hash: &str,
    cache_miss_reason: CacheMissReason,
    relative_path: &str,
    local_parser: &LocalParserFingerprint,
) -> Result<NormalizedParserResult, String> {
    let parser_backend = parsed
        .parser_backend
        .clone()
        .unwrap_or_else(|| "not_parsed".to_string());
    let worker_lane_text = parsed
        .worker_lane
        .clone()
        .unwrap_or_else(|| "not_parsed".to_string());
    let worker_lane = parse_worker_lane(&worker_lane_text)?;
    let mut diagnostics = Vec::new();
    if let Some(primary) = &parsed.primary_failure {
        diagnostics.push(primary.diagnostic.clone());
    }
    let fallback_backend = parsed
        .fallback_backend
        .map(|backend| backend.as_str().to_string())
        .unwrap_or_default();
    let fallback_reason_code = parsed
        .primary_failure
        .as_ref()
        .map(|failure| enum_text(&failure.diagnostic.error_code))
        .unwrap_or_default();
    let failure_class = parsed
        .error
        .as_ref()
        .or(parsed.primary_failure.as_ref())
        .map(|failure| failure.class.as_str().to_string())
        .unwrap_or_default();

    let (
        content,
        content_sha256,
        truncated,
        parse_status,
        final_error,
        worker_contract,
        worker_version,
        worker_build,
    ) = match parsed.payload {
        Some(ParsedPayload::LightText(payload)) => {
            let content_hash = sha256_hex(payload.content.as_bytes());
            (
                payload.content,
                content_hash,
                payload.truncated,
                ParseStatus::Success,
                None,
                local_parser.contract.clone(),
                local_parser.version.clone(),
                local_parser.build.clone(),
            )
        }
        Some(ParsedPayload::Worker(payload)) => {
            diagnostics.extend(
                payload
                    .warnings
                    .iter()
                    .map(worker_diagnostic_to_scanner),
            );
            let content_hash = sha256_hex(payload.content.as_bytes());
            (
                payload.content,
                content_hash,
                payload.truncated,
                ParseStatus::Success,
                None,
                payload.worker_contract_version,
                payload.worker_version,
                payload.worker_build,
            )
        }
        None => {
            let error = parsed
                .error
                .clone()
                .ok_or_else(|| "empty parser result has no diagnostic".to_string())?;
            let parse_status = if error.is_timeout() {
                ParseStatus::Timeout
            } else {
                ParseStatus::Error
            };
            diagnostics.push(error.diagnostic.clone());
            (
                String::new(),
                sha256_hex(b""),
                false,
                parse_status,
                Some(error.diagnostic),
                String::new(),
                String::new(),
                String::new(),
            )
        }
    };
    let cache_write = (parse_status == ParseStatus::Success).then(|| CacheWriteRecord {
        file_identity: parsed.file.file_identity.clone(),
        source_version: parsed.file.source_version.clone(),
        parse_profile_hash: parse_profile_hash.to_string(),
        content: content.clone(),
        content_sha256: content_sha256.clone(),
        parser_backend: parser_backend.clone(),
        worker_lane: worker_lane_text.clone(),
        truncated,
        worker_contract_version: worker_contract,
        worker_version,
        worker_build,
    });
    let file_result = FileResultRecord {
        file_identity: parsed.file.file_identity.clone(),
        relative_path: relative_path.to_string(),
        source_version: parsed.file.source_version.clone(),
        parse_profile_hash: parse_profile_hash.to_string(),
        cache_status: CacheStatus::Miss,
        cache_miss_reason,
        parse_status,
        parser_backend: parser_backend.clone(),
        worker_lane,
        truncated,
        content_sha256,
        primary_duration_ms: parsed.primary_duration_ms,
        fallback_duration_ms: parsed.fallback_duration_ms,
        parse_duration_ms: parsed.total_duration_ms,
        failure_class,
        fallback_backend,
        fallback_reason_code,
        error: final_error.clone(),
    };
    let context_evidence = ContextFileEvidence {
        file_identity: parsed.file.file_identity,
        absolute_path: parsed.file.path,
        relative_path: relative_path.to_string(),
        extension: parsed.file.extension,
        size_bytes: Some(parsed.file.size_bytes),
        content,
        parser_backend,
        worker_lane,
        cache_status: CacheStatus::Miss,
        parse_status,
        truncated,
        error: final_error,
        reason: None,
    };
    Ok(NormalizedParserResult {
        cache_write,
        file_result,
        context_evidence,
        diagnostics,
    })
}

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
        match result.parse_status {
            ParseStatus::Success => {
                metric.success_count =
                    checked_add(metric.success_count, 1, "extension success count")?;
            }
            ParseStatus::Timeout => {
                metric.timeout_count =
                    checked_add(metric.timeout_count, 1, "extension timeout count")?;
            }
            // spec Part 2.2: extension `error_count` counts ONLY ParseStatus::Error;
            // NotParsed never enters the error metric. The derived not_parsed count
            // is `file_count - success - error - timeout`.
            ParseStatus::Error => {
                metric.error_count = checked_add(metric.error_count, 1, "extension error count")?;
            }
            ParseStatus::NotParsed => {}
        }
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
    profile: &'a NormalizedScannerProfileV1,
}

pub fn context_profile_hash(
    protocol_version: u64,
    engine_build: &str,
    profile: &NormalizedScannerProfileV1,
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
    profile: &NormalizedScannerProfileV1,
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
pub struct InspectSnapshot {
    pub context_run_id: Option<u64>,
    pub run_status: RunStatus,
    pub summary: ContextSummary,
    pub stage_metrics: Vec<StageMetric>,
    pub extension_metrics: Vec<ExtensionMetric>,
    pub files: Vec<FileAudit>,
    pub decisions: Vec<ContextDecision>,
    pub warnings: Vec<Diagnostic>,
}

#[derive(Debug, Error)]
pub enum InspectAuditError {
    #[error("scanner run was not found")]
    RunNotFound,
    #[error("persisted scanner run is corrupt: {0}")]
    RunCorrupt(String),
    #[error("inspect content is restricted to sanitized fixture databases")]
    ContentForbidden,
    #[error("scanner database could not be read")]
    Sql(#[source] rusqlite::Error),
}

#[derive(Debug)]
pub struct InspectLoadError {
    pub error: InspectAuditError,
    pub run_status: Option<RunStatus>,
}

impl InspectLoadError {
    fn before_status(error: InspectAuditError) -> Self {
        Self {
            error,
            run_status: None,
        }
    }

    fn after_status(error: InspectAuditError, run_status: RunStatus) -> Self {
        Self {
            error,
            run_status: Some(run_status),
        }
    }
}

pub(crate) fn load_inspect_snapshot(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    include_content: bool,
) -> Result<InspectSnapshot, InspectLoadError> {
    let run_row: Option<(String, String, String, String, Option<String>)> = transaction
        .query_row(
            "SELECT request_id, canonical_request_json, request_hash, status, final_envelope_json
             FROM scan_runs WHERE scan_run_id=?1",
            [scan_run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| InspectLoadError::before_status(InspectAuditError::Sql(error)))?;
    let (stored_request_id, canonical_request_json, request_hash, run_status_text, envelope_json) =
        run_row.ok_or_else(|| InspectLoadError::before_status(InspectAuditError::RunNotFound))?;
    if crate::store::cache::domain_hash(b"request-v1\0", canonical_request_json.as_bytes())
        != request_hash
    {
        return Err(InspectLoadError::before_status(
            InspectAuditError::RunCorrupt("stored logical request hash is invalid".to_string()),
        ));
    }
    let run_status = parse_run_status(&run_status_text).map_err(|message| {
        InspectLoadError::before_status(InspectAuditError::RunCorrupt(message))
    })?;

    if include_content {
        let application_id: i64 = transaction
            .query_row("PRAGMA application_id", [], |row| row.get(0))
            .map_err(|error| {
                InspectLoadError::after_status(InspectAuditError::Sql(error), run_status)
            })?;
        if application_id != SANITIZED_FIXTURE_APPLICATION_ID {
            return Err(InspectLoadError::after_status(
                InspectAuditError::ContentForbidden,
                run_status,
            ));
        }
    }

    let envelope = load_and_validate_envelope(
        scan_run_id,
        &stored_request_id,
        run_status,
        envelope_json.as_deref(),
    )
    .map_err(|error| InspectLoadError::after_status(error, run_status))?;
    let context = load_context_row(transaction, scan_run_id)
        .map_err(|error| InspectLoadError::after_status(error, run_status))?;
    let stage_metrics = load_stage_metrics(transaction, scan_run_id)
        .map_err(|error| InspectLoadError::after_status(error, run_status))?;
    let extension_metrics = load_extension_metrics(transaction, scan_run_id)
        .map_err(|error| InspectLoadError::after_status(error, run_status))?;
    let files = load_file_audits(transaction, scan_run_id)
        .map_err(|error| InspectLoadError::after_status(error, run_status))?;
    let persisted_decisions = load_context_decisions(transaction, scan_run_id)
        .map_err(|error| InspectLoadError::after_status(error, run_status))?;
    let (warnings, persisted_error) = load_run_diagnostics(transaction, scan_run_id)
        .map_err(|error| InspectLoadError::after_status(error, run_status))?;

    let (context_run_id, summary) = match (&context, &envelope) {
        (Some(context), Some(envelope)) => {
            if context.status != run_status
                || envelope.context_run_id.0 != Some(context.context_run_id)
                || envelope.file_context != context.final_context
                || envelope.summary != context.summary
                || warnings != envelope.warnings
                || persisted_error.is_some()
            {
                return Err(InspectLoadError::after_status(
                    InspectAuditError::RunCorrupt(
                        "context row and final envelope disagree".to_string(),
                    ),
                    run_status,
                ));
            }
            validate_relational_summary(
                context,
                &stage_metrics,
                &extension_metrics,
                &files,
                &persisted_decisions,
            )
            .map_err(|error| InspectLoadError::after_status(error, run_status))?;
            (Some(context.context_run_id), context.summary.clone())
        }
        (None, Some(envelope)) if run_status == RunStatus::Error => {
            if envelope.context_run_id.0.is_some()
                || !files.is_empty()
                || !persisted_decisions.is_empty()
            {
                return Err(InspectLoadError::after_status(
                    InspectAuditError::RunCorrupt(
                        "error envelope has unexpected context rows".to_string(),
                    ),
                    run_status,
                ));
            }
            if warnings != envelope.warnings || persisted_error != envelope.error.0 {
                return Err(InspectLoadError::after_status(
                    InspectAuditError::RunCorrupt(
                        "run warnings and error envelope disagree".to_string(),
                    ),
                    run_status,
                ));
            }
            (None, envelope.summary.clone())
        }
        (None, None) if matches!(run_status, RunStatus::Running | RunStatus::Abandoned) => {
            if !stage_metrics.is_empty()
                || !extension_metrics.is_empty()
                || !files.is_empty()
                || !persisted_decisions.is_empty()
                || !warnings.is_empty()
                || persisted_error.is_some()
            {
                return Err(InspectLoadError::after_status(
                    InspectAuditError::RunCorrupt(
                        "nonterminal run exposes committed final rows".to_string(),
                    ),
                    run_status,
                ));
            }
            (None, empty_context_summary())
        }
        _ => {
            return Err(InspectLoadError::after_status(
                InspectAuditError::RunCorrupt(
                    "run status, envelope, and context rows disagree".to_string(),
                ),
                run_status,
            ));
        }
    };
    let decisions = persisted_decisions
        .into_iter()
        .map(|record| record.decision)
        .collect();

    Ok(InspectSnapshot {
        context_run_id,
        run_status,
        summary,
        stage_metrics,
        extension_metrics,
        files,
        decisions,
        warnings,
    })
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

fn load_and_validate_envelope(
    scan_run_id: i64,
    stored_request_id: &str,
    run_status: RunStatus,
    envelope_json: Option<&str>,
) -> Result<Option<ContextEnvelope>, InspectAuditError> {
    if matches!(run_status, RunStatus::Running | RunStatus::Abandoned) {
        return if envelope_json.is_none() {
            Ok(None)
        } else {
            Err(InspectAuditError::RunCorrupt(
                "nonterminal run has a final envelope".to_string(),
            ))
        };
    }
    let json = envelope_json.ok_or_else(|| {
        InspectAuditError::RunCorrupt("terminal run has no final envelope".to_string())
    })?;
    let envelope: ContextEnvelope = serde_json::from_str(json)
        .map_err(|_| InspectAuditError::RunCorrupt("final envelope JSON is invalid".to_string()))?;
    envelope.validate().map_err(|_| {
        InspectAuditError::RunCorrupt("final envelope violates the contract".to_string())
    })?;
    let canonical = serde_json::to_string(&envelope).map_err(|_| {
        InspectAuditError::RunCorrupt("final envelope could not be canonicalized".to_string())
    })?;
    let status_matches = matches!(
        (run_status, envelope.status),
        (RunStatus::Success, EngineStatus::Ok)
            | (RunStatus::Partial, EngineStatus::Partial)
            | (RunStatus::Error, EngineStatus::Error)
    );
    if canonical != json
        || envelope.request_id != stored_request_id
        || envelope.scan_run_id.0 != Some(scan_run_id as u64)
        || !status_matches
    {
        return Err(InspectAuditError::RunCorrupt(
            "final envelope does not match its run".to_string(),
        ));
    }
    Ok(Some(envelope))
}

fn load_context_row(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Option<PersistedContext>, InspectAuditError> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
        i64,
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    )> = transaction
        .query_row(
            "SELECT context_run_id, context_profile_hash, status, final_context,
                    context_sha256, source_file_count, success_count, timeout_count,
                    included_file_count, omitted_file_count, error_file_count,
                    input_chars, output_chars, total_duration_ms,
                    discovery_duration_ms, parse_duration_ms, compression_duration_ms
             FROM context_runs WHERE scan_run_id=?1",
            [scan_run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                    row.get(15)?,
                    row.get(16)?,
                ))
            },
        )
        .optional()
        .map_err(InspectAuditError::Sql)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let status = parse_run_status(&row.2).map_err(InspectAuditError::RunCorrupt)?;
    if row.0 != scan_run_id
        || !is_sha256(&row.1)
        || !matches!(
            status,
            RunStatus::Success | RunStatus::Partial | RunStatus::Error
        )
        || sha256_hex(row.3.as_bytes()) != row.4
    {
        return Err(InspectAuditError::RunCorrupt(
            "context row identity or hash is invalid".to_string(),
        ));
    }
    let summary = ContextSummary {
        source_file_count: to_u64(row.5, "source_file_count")?,
        success_count: to_u64(row.6, "success_count")?,
        timeout_count: to_u64(row.7, "timeout_count")?,
        included_file_count: to_u64(row.8, "included_file_count")?,
        omitted_file_count: to_u64(row.9, "omitted_file_count")?,
        error_file_count: to_u64(row.10, "error_file_count")?,
        input_chars: to_u64(row.11, "input_chars")?,
        output_chars: to_u64(row.12, "output_chars")?,
        total_duration_ms: to_u64(row.13, "total_duration_ms")?,
        discovery_duration_ms: to_u64(row.14, "discovery_duration_ms")?,
        parse_duration_ms: to_u64(row.15, "parse_duration_ms")?,
        compression_duration_ms: to_u64(row.16, "compression_duration_ms")?,
    };
    summary
        .validate()
        .map_err(|_| InspectAuditError::RunCorrupt("context summary is invalid".to_string()))?;
    Ok(Some(PersistedContext {
        context_run_id: row.0 as u64,
        status,
        final_context: row.3,
        summary,
    }))
}

fn load_stage_metrics(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Vec<StageMetric>, InspectAuditError> {
    let mut statement = transaction
        .prepare(
            "SELECT stage, item_count, duration_ms FROM scan_stage_metrics
             WHERE scan_run_id=?1
             ORDER BY CASE stage WHEN 'discovery' THEN 1 WHEN 'cache' THEN 2
                                  WHEN 'parse' THEN 3 WHEN 'context' THEN 4 END",
        )
        .map_err(InspectAuditError::Sql)?;
    let raw = statement
        .query_map([scan_run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(InspectAuditError::Sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(InspectAuditError::Sql)?;
    raw.into_iter()
        .map(|row| {
            let metric = StageMetric {
                stage: parse_enum(&row.0, "stage metric")?,
                item_count: to_u64(row.1, "stage item_count")?,
                duration_ms: to_u64(row.2, "stage duration_ms")?,
            };
            metric.validate().map_err(|_| {
                InspectAuditError::RunCorrupt("stage metric is invalid".to_string())
            })?;
            Ok(metric)
        })
        .collect()
}

fn load_extension_metrics(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Vec<ExtensionMetric>, InspectAuditError> {
    let mut statement = transaction
        .prepare(
            "SELECT extension, file_count, parse_duration_ms, success_count,
                    error_count, timeout_count
             FROM scan_extension_metrics WHERE scan_run_id=?1 ORDER BY extension",
        )
        .map_err(InspectAuditError::Sql)?;
    let raw = statement
        .query_map([scan_run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(InspectAuditError::Sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(InspectAuditError::Sql)?;
    raw.into_iter()
        .map(|row| {
            let metric = ExtensionMetric {
                extension: row.0,
                file_count: to_u64(row.1, "extension file_count")?,
                parse_duration_ms: to_u64(row.2, "extension parse_duration_ms")?,
                success_count: to_u64(row.3, "extension success_count")?,
                error_count: to_u64(row.4, "extension error_count")?,
                timeout_count: to_u64(row.5, "extension timeout_count")?,
            };
            metric.validate().map_err(|_| {
                InspectAuditError::RunCorrupt("extension metric is invalid".to_string())
            })?;
            Ok(metric)
        })
        .collect()
}

fn load_file_audits(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Vec<FileAudit>, InspectAuditError> {
    #[allow(clippy::type_complexity)]
    let mut statement = transaction
        .prepare(
            "SELECT relative_path, file_identity, source_version, parse_status,
                    parser_backend, worker_lane, cache_status, cache_miss_reason,
                    truncated, content_sha256, parse_duration_ms, failure_class,
                    fallback_backend, fallback_reason_code
             FROM scan_file_results WHERE scan_run_id=?1
             ORDER BY lower(relative_path), relative_path, file_identity",
        )
        .map_err(InspectAuditError::Sql)?;
    let raw = statement
        .query_map([scan_run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
            ))
        })
        .map_err(InspectAuditError::Sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(InspectAuditError::Sql)?;
    raw.into_iter()
        .map(|row| {
            let file = FileAudit {
                relative_path: row.0,
                file_identity: row.1,
                source_version: row.2,
                parse_status: parse_enum(&row.3, "file parse_status")?,
                parser_backend: row.4,
                worker_lane: parse_enum(&row.5, "file worker_lane")?,
                cache_status: parse_enum(&row.6, "file cache_status")?,
                cache_miss_reason: parse_enum(&row.7, "file cache_miss_reason")?,
                truncated: parse_bool(row.8, "file truncated")?,
                content_sha256: row.9,
                parse_duration_ms: to_u64(row.10, "file parse_duration_ms")?,
                failure_class: row.11,
                fallback_backend: row.12,
                fallback_reason_code: row.13,
            };
            file.validate()
                .map_err(|_| InspectAuditError::RunCorrupt("file audit is invalid".to_string()))?;
            Ok(file)
        })
        .collect()
}

fn load_context_decisions(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Vec<PersistedDecision>, InspectAuditError> {
    let mut statement = transaction
        .prepare(
            "SELECT d.file_identity, d.relative_path, d.action, d.reason, d.priority,
                    d.input_chars, d.output_chars, d.truncated, d.error_code
             FROM context_decisions d
             JOIN context_runs c ON c.context_run_id=d.context_run_id
             WHERE c.scan_run_id=?1
             ORDER BY d.priority, lower(d.relative_path), d.relative_path, d.file_identity",
        )
        .map_err(InspectAuditError::Sql)?;
    let raw = statement
        .query_map([scan_run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
            ))
        })
        .map_err(InspectAuditError::Sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(InspectAuditError::Sql)?;
    raw.into_iter()
        .map(|row| {
            let decision = ContextDecision {
                relative_path: row.1,
                action: parse_enum(&row.2, "context action")?,
                reason: row.3,
                priority: to_u64(row.4, "context priority")?,
                input_chars: to_u64(row.5, "context input_chars")?,
                output_chars: to_u64(row.6, "context output_chars")?,
                truncated: parse_bool(row.7, "context truncated")?,
                error_code: row.8,
            };
            decision.validate().map_err(|_| {
                InspectAuditError::RunCorrupt("context decision is invalid".to_string())
            })?;
            Ok(PersistedDecision {
                file_identity: row.0,
                decision,
            })
        })
        .collect()
}

fn load_run_diagnostics(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<(Vec<Diagnostic>, Option<Diagnostic>), InspectAuditError> {
    let mut statement = transaction
        .prepare(
            "SELECT severity, error_code, message, retryable, stage, file_path, backend
             FROM run_diagnostics WHERE scan_run_id=?1 ORDER BY sequence",
        )
        .map_err(InspectAuditError::Sql)?;
    let raw = statement
        .query_map([scan_run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })
        .map_err(InspectAuditError::Sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(InspectAuditError::Sql)?;
    let mut warnings = Vec::new();
    let mut persisted_error = None;
    for row in raw {
        if !matches!(row.0.as_str(), "warning" | "error") {
            return Err(InspectAuditError::RunCorrupt(
                "run diagnostic severity is invalid".to_string(),
            ));
        }
        let diagnostic = Diagnostic {
            error_code: parse_enum(&row.1, "diagnostic error_code")?,
            message: row.2,
            retryable: parse_bool(row.3, "diagnostic retryable")?,
            stage: parse_enum(&row.4, "diagnostic stage")?,
            file_path: Nullable(row.5),
            backend: Nullable(row.6),
        };
        diagnostic
            .validate()
            .map_err(|_| InspectAuditError::RunCorrupt("run diagnostic is invalid".to_string()))?;
        if row.0 == "warning" {
            warnings.push(diagnostic);
        } else if persisted_error.replace(diagnostic).is_some() {
            return Err(InspectAuditError::RunCorrupt(
                "run contains multiple terminal errors".to_string(),
            ));
        }
    }
    Ok((warnings, persisted_error))
}

fn validate_relational_summary(
    context: &PersistedContext,
    stages: &[StageMetric],
    extensions: &[ExtensionMetric],
    files: &[FileAudit],
    decisions: &[PersistedDecision],
) -> Result<(), InspectAuditError> {
    let summary = &context.summary;
    let success_count = files
        .iter()
        .filter(|file| file.parse_status == ParseStatus::Success)
        .count() as u64;
    let timeout_count = files
        .iter()
        .filter(|file| file.parse_status == ParseStatus::Timeout)
        .count() as u64;
    let error_count = files
        .iter()
        .filter(|file| file.parse_status == ParseStatus::Error)
        .count() as u64;
    let not_parsed_count = files
        .len()
        .checked_sub(success_count as usize)
        .and_then(|value| value.checked_sub(timeout_count as usize))
        .and_then(|value| value.checked_sub(error_count as usize))
        .map(|value| value as u64)
        .ok_or_else(|| {
            InspectAuditError::RunCorrupt("file status counts overflow".to_string())
        })?;
    let included_count = decisions
        .iter()
        .filter(|record| {
            matches!(
                record.decision.action,
                ContextAction::Keep | ContextAction::Compress | ContextAction::MetadataOnly
            )
        })
        .count() as u64;
    let omitted_count = decisions
        .iter()
        .filter(|record| record.decision.action == ContextAction::Omit)
        .count() as u64;
    let decision_error_count = decisions
        .iter()
        .filter(|record| record.decision.action == ContextAction::Error)
        .count() as u64;
    let input_chars = decisions.iter().try_fold(0_u64, |total, record| {
        total
            .checked_add(record.decision.input_chars)
            .ok_or_else(|| {
                InspectAuditError::RunCorrupt("decision input count overflows".to_string())
            })
    })?;
    let files_by_identity: HashMap<&str, &str> = files
        .iter()
        .map(|file| (file.file_identity.as_str(), file.relative_path.as_str()))
        .collect();
    if files_by_identity.len() != files.len()
        || decisions.iter().any(|record| {
            files_by_identity
                .get(record.file_identity.as_str())
                .is_none_or(|path| *path != record.decision.relative_path)
        })
    {
        return Err(InspectAuditError::RunCorrupt(
            "context decisions do not match file identities and paths".to_string(),
        ));
    }
    if summary.source_file_count != files.len() as u64
        || files.len() != decisions.len()
        || summary.success_count != success_count
        || summary.timeout_count != timeout_count
        || summary.error_file_count != error_count
        || summary.included_file_count != included_count
        || summary.omitted_file_count != omitted_count
        // spec Part 2.2 count equations:
        //   included = success, omitted = derived not_parsed,
        //   decision_error = error + timeout, and every file has one decision.
        || included_count != success_count
        || omitted_count != not_parsed_count
        || decision_error_count != error_count + timeout_count
        || included_count + omitted_count + decision_error_count != decisions.len() as u64
        || summary.input_chars != input_chars
        || summary.output_chars != context.final_context.chars().count() as u64
    {
        return Err(InspectAuditError::RunCorrupt(
            "context summary disagrees with file or decision rows".to_string(),
        ));
    }
    let stage_by_name: HashMap<StageName, &StageMetric> =
        stages.iter().map(|metric| (metric.stage, metric)).collect();
    if stages.len() != 4
        || stage_by_name.len() != 4
        || stage_by_name
            .get(&StageName::Discovery)
            .is_none_or(|metric| {
                metric.item_count != summary.source_file_count
                    || metric.duration_ms != summary.discovery_duration_ms
            })
        || stage_by_name
            .get(&StageName::Cache)
            .is_none_or(|metric| metric.item_count != summary.source_file_count)
        || stage_by_name
            .get(&StageName::Parse)
            .is_none_or(|metric| metric.duration_ms != summary.parse_duration_ms)
        || stage_by_name.get(&StageName::Context).is_none_or(|metric| {
            metric.item_count != decisions.len() as u64
                || metric.duration_ms != summary.compression_duration_ms
        })
    {
        return Err(InspectAuditError::RunCorrupt(
            "context summary disagrees with stage metrics".to_string(),
        ));
    }
    let measured_duration = stages.iter().try_fold(0_u64, |total, metric| {
        total.checked_add(metric.duration_ms).ok_or_else(|| {
            InspectAuditError::RunCorrupt("stage duration sum overflows".to_string())
        })
    })?;
    if summary.total_duration_ms < measured_duration {
        return Err(InspectAuditError::RunCorrupt(
            "total duration is shorter than its stages".to_string(),
        ));
    }
    let extension_totals = extensions.iter().try_fold(
        (0_u64, 0_u64, 0_u64, 0_u64),
        |(files, successes, errors, timeouts), metric| {
            Ok::<_, InspectAuditError>((
                files.checked_add(metric.file_count).ok_or_else(|| {
                    InspectAuditError::RunCorrupt("extension file count overflows".to_string())
                })?,
                successes.checked_add(metric.success_count).ok_or_else(|| {
                    InspectAuditError::RunCorrupt("extension success count overflows".to_string())
                })?,
                errors.checked_add(metric.error_count).ok_or_else(|| {
                    InspectAuditError::RunCorrupt("extension error count overflows".to_string())
                })?,
                timeouts.checked_add(metric.timeout_count).ok_or_else(|| {
                    InspectAuditError::RunCorrupt("extension timeout count overflows".to_string())
                })?,
            ))
        },
    )?;
    let (
        extension_file_count,
        extension_success_count,
        extension_error_count,
        extension_timeout_count,
    ) = extension_totals;
    if extension_file_count != summary.source_file_count
        || extension_success_count != summary.success_count
        || extension_error_count != summary.error_file_count
        || extension_timeout_count != summary.timeout_count
    {
        return Err(InspectAuditError::RunCorrupt(
            "context summary disagrees with extension metrics".to_string(),
        ));
    }
    Ok(())
}

fn parse_run_status(value: &str) -> Result<RunStatus, String> {
    match value {
        "running" => Ok(RunStatus::Running),
        "success" => Ok(RunStatus::Success),
        "partial" => Ok(RunStatus::Partial),
        "error" => Ok(RunStatus::Error),
        "abandoned" => Ok(RunStatus::Abandoned),
        _ => Err("run status is invalid".to_string()),
    }
}

fn parse_enum<T: serde::de::DeserializeOwned>(
    value: &str,
    field: &str,
) -> Result<T, InspectAuditError> {
    serde_json::from_value(serde_json::Value::String(value.to_string()))
        .map_err(|_| InspectAuditError::RunCorrupt(format!("persisted {field} is invalid")))
}

fn parse_bool(value: i64, field: &str) -> Result<bool, InspectAuditError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(InspectAuditError::RunCorrupt(format!(
            "persisted {field} is invalid"
        ))),
    }
}

fn to_u64(value: i64, field: &str) -> Result<u64, InspectAuditError> {
    u64::try_from(value)
        .map_err(|_| InspectAuditError::RunCorrupt(format!("persisted {field} is negative")))
}

fn empty_context_summary() -> ContextSummary {
    ContextSummary {
        source_file_count: 0,
        success_count: 0,
        timeout_count: 0,
        included_file_count: 0,
        omitted_file_count: 0,
        error_file_count: 0,
        input_chars: 0,
        output_chars: 0,
        total_duration_ms: 0,
        discovery_duration_ms: 0,
        parse_duration_ms: 0,
        compression_duration_ms: 0,
    }
}

pub(crate) fn relative_contract_path(work_dir: &Path, absolute_path: &str) -> Result<String, String> {
    let absolute = Path::new(absolute_path);
    let relative = absolute
        .strip_prefix(work_dir)
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(|| {
            let root = work_dir.to_string_lossy().replace('/', "\\");
            let path = absolute_path.replace('/', "\\");
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

fn classification_diagnostic(reason: ClassificationError, absolute_path: &str) -> Diagnostic {
    let (error_code, message) = match reason {
        ClassificationError::FileTooLarge => (
            ErrorCode::FileTooLarge,
            "file exceeds the configured size limit",
        ),
        ClassificationError::UnsupportedExtension => {
            (ErrorCode::ParserFailed, "file extension is not supported")
        }
        ClassificationError::UnsupportedBackend => (
            ErrorCode::ParserFailed,
            "configured parser backend is not supported",
        ),
        ClassificationError::LegacyExtensionDisabled => (
            ErrorCode::ParserFailed,
            "legacy Office extension is disabled by the scanner profile",
        ),
    };
    Diagnostic {
        error_code,
        message: message.to_string(),
        retryable: false,
        stage: DiagnosticStage::Parse,
        file_path: Nullable(Some(absolute_path.to_string())),
        backend: Nullable(None),
    }
}

fn parse_worker_lane(value: &str) -> Result<AuditWorkerLane, String> {
    match value {
        "rust_core" => Ok(AuditWorkerLane::RustCore),
        "rust_office_process" => Ok(AuditWorkerLane::RustOfficeProcess),
        "python_document_process" => Ok(AuditWorkerLane::PythonDocumentProcess),
        "not_parsed" => Ok(AuditWorkerLane::NotParsed),
        _ => Err("scanner worker lane is invalid".to_string()),
    }
}

fn enum_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .expect("contract enum must serialize to text")
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
