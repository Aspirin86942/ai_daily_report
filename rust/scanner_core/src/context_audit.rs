//! Scanner evidence normalization, persistence DTO assembly, and inspect snapshots.

use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheMissReason, CacheStatus, ContextAction, ContextDecision,
    ContextProfile, ContextSummary, Diagnostic, DiagnosticStage, EngineStatus, ErrorCode,
    ExecutionMetricsV2, ExtensionMetric, FileAudit, NormalizedScannerProfileV1, Nullable,
    ParseStatus, PdfClassificationStatus, RunStatus, StageMetric, StageName, Validate,
};
use rusqlite::{OptionalExtension, Transaction};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use thiserror::Error;

use crate::classifier::ClassificationError;
use crate::artifact::PdfClassificationProvenanceV1;
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
        source_guard_kind: parsed
            .file
            .source_guard_kind
            .clone()
            .unwrap_or_else(|| "content_sha256_v1".to_string()),
        source_guard_sha256: parsed
            .file
            .source_guard_sha256
            .clone()
            .unwrap_or_else(|| "0".repeat(64)),
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
    // ---- Inspect v2 provenance (spec Part 5.3). `audit_provenance_version`
    // is None only for nonterminal (running/abandoned) rows; `migrated_v1`
    // rows fail closed with `INSPECT_V2_PROVENANCE_UNAVAILABLE`.
    pub audit_provenance_version: Option<AuditProvenanceVersion>,
    pub artifact_id: Option<u64>,
    pub reused_from_context_run_id: Option<u64>,
    pub snapshot_hit: bool,
    /// `reserved_chars`/`rendered_chars` reconstructed from the artifact's
    /// immutable semantic summary (spec Part 5.3 `execution_metrics`).
    pub reserved_chars: u64,
    pub rendered_chars: u64,
    /// WorkDeadline run-level trigger fired during this run (0 or 1).
    pub stage_deadline_exhausted: bool,
    /// Full_v2 per-file rows (v1 FileAudit + SourceGuardV2 + classifier
    /// provenance). Empty for migrated_v1/nonterminal rows.
    pub files_v2: Vec<FileAuditV2Source>,
    /// Authoritative `execution_metrics` persisted at finalize (spec Part 5.3).
    /// `None` for migrated_v1/nonterminal rows and engine-error runs that had no
    /// scheduler outcome.
    pub execution_metrics: Option<ExecutionMetricsV2>,
}

/// `scan_runs.audit_provenance_version` (spec Part 8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditProvenanceVersion {
    MigratedV1,
    FullV2,
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
    pub final_diagnostic: Option<Diagnostic>,
    pub classifier: Option<PdfClassificationProvenanceV1>,
}

/// Assembles the strict `execution_metrics` object (spec Part 5.3) from the
/// persisted full_v2 run. Counts that the scheduler owns and that cannot be
/// reconstructed from persisted rows are only populated for non-snapshot runs
/// via best-effort derivation from the per-file audit rows; a snapshot-hit
/// current run legitimately reports 0 for every plan/execution count because
/// the scheduler did not run. The two `*_all_hit` nullables follow the
/// `lookup_count=0 -> null` rule.
pub fn assemble_execution_metrics_v2(snapshot: &InspectSnapshot) -> ExecutionMetricsV2 {
    let discovery_observed_file_count = snapshot
        .stage_metrics
        .iter()
        .find(|metric| metric.stage == StageName::Discovery)
        .map(|metric| metric.item_count)
        .unwrap_or(snapshot.summary.source_file_count);
    let discovery_ms = snapshot
        .stage_metrics
        .iter()
        .find(|metric| metric.stage == StageName::Discovery)
        .map(|metric| metric.duration_ms)
        .unwrap_or(snapshot.summary.discovery_duration_ms);

    let mut source_guard_content_hash_file_count = 0_u64;
    let mut source_guard_unavailable_count = 0_u64;
    let mut source_guard_bytes_read = 0_u64;
    let mut parse_cache_lookup_count = 0_u64;
    let mut parse_cache_fresh_count = 0_u64;
    let mut pdfplumber_invocations = 0_u64;
    let mut parse_attempt_count = 0_u64;
    for file in &snapshot.files_v2 {
        match file.source_guard_kind.as_deref() {
            Some("content_sha256_v1") => {
                source_guard_content_hash_file_count = source_guard_content_hash_file_count + 1;
                // spec Part 5.3: a full-content hash attempt reads the entire
                // file; the inventory size is the best persisted proxy.
                source_guard_bytes_read = source_guard_bytes_read.saturating_add(file.size_bytes);
            }
            Some("unavailable") => source_guard_unavailable_count = source_guard_unavailable_count + 1,
            _ => {}
        }
        let cache_status = file.parse_cache_status.as_deref();
        if matches!(cache_status, Some("fresh") | Some("miss")) {
            parse_cache_lookup_count = parse_cache_lookup_count + 1;
            if cache_status == Some("fresh") {
                parse_cache_fresh_count = parse_cache_fresh_count + 1;
            }
        }
        if file.parser_backend == "pdf_text_v1" && cache_status == Some("miss") {
            pdfplumber_invocations = pdfplumber_invocations + 1;
        }
        if cache_status == Some("miss")
            && matches!(
                file.parse_status,
                ParseStatus::Success | ParseStatus::Error | ParseStatus::Timeout
            )
        {
            parse_attempt_count = parse_attempt_count + 1;
        }
    }
    let parse_cache_all_hit = if parse_cache_lookup_count > 0 {
        Some(parse_cache_fresh_count == parse_cache_lookup_count)
    } else {
        None
    };

    // Plan/execution counts are authoritative only when the scheduler ran; a
    // snapshot-hit current run skips planning + classification + parse, so 0 is
    // the truthful value. For non-snapshot runs the derived values below are
    // best-effort reconstructions from the persisted per-file rows.
    let (candidate_file_count, admitted_file_count, classification_slot_count, extraction_slot_count, nominal_charged_pages_total, classify_attempt_count, classification_lookup_count) =
        if snapshot.snapshot_hit {
            (0, 0, 0, 0, 0, 0, 0)
        } else {
            let mut admitted = 0_u64;
            let mut classification_slot = 0_u64;
            let mut extraction_slot = 0_u64;
            let mut nominal = 0_u64;
            let mut classify_attempt = 0_u64;
            let mut classification_lookup = 0_u64;
            for file in &snapshot.files_v2 {
                let cache_status = file.parse_cache_status.as_deref();
                if file.parser_backend != "not_parsed" || cache_status == Some("fresh") {
                    admitted = admitted + 1;
                }
                if let Some(classifier) = &file.classifier {
                    classification_slot = classification_slot + 1;
                    nominal = nominal.saturating_add(classifier.nominal_charged_pages);
                    if cache_status != Some("snapshot") {
                        classification_lookup = classification_lookup + 1;
                        classify_attempt = classify_attempt + 1;
                    }
                    if matches!(
                        classifier.status,
                        PdfClassificationStatus::TextInParseWindow
                            | PdfClassificationStatus::NoTextInParseWindow
                    ) {
                        extraction_slot = extraction_slot + 1;
                    }
                }
            }
            (
                snapshot.files_v2.len() as u64,
                admitted,
                classification_slot,
                extraction_slot,
                nominal,
                classify_attempt,
                classification_lookup,
            )
        };
    let classification_cache_all_hit = if classification_lookup_count > 0 {
        // Exact per-file classification cache status is not persisted; a
        // conservative false never fabricates an all-hit claim.
        Some(false)
    } else {
        None
    };

    ExecutionMetricsV2 {
        discovery_observed_file_count,
        source_guard_content_hash_file_count,
        source_guard_unavailable_count,
        source_guard_bytes_read,
        candidate_file_count,
        admitted_file_count,
        classification_slot_count,
        confirmed_run_inspected_pages_total: 0,
        unobserved_classification_attempt_count: 0,
        nominal_charged_pages_total,
        extraction_slot_count,
        pdfplumber_invocations,
        snapshot_hit: snapshot.snapshot_hit,
        parse_cache_lookup_count,
        classification_cache_lookup_count: classification_lookup_count,
        parse_cache_all_hit: Nullable(parse_cache_all_hit),
        classification_cache_all_hit: Nullable(classification_cache_all_hit),
        stage_deadline_exhausted_count: u64::from(snapshot.stage_deadline_exhausted),
        session_restart_count: 0,
        session_fallback_count: 0,
        classify_attempt_count,
        parse_attempt_count,
        reserved_chars: snapshot.reserved_chars,
        rendered_chars: snapshot.rendered_chars,
        worker_handshake_ms: 0,
        discovery_ms,
        snapshot_lookup_ms: 0,
        current_run_audit_write_ms: 0,
        terminal_precommit_ms: 0,
        deadline_precommit_elapsed_ms: 0,
        envelope_rebuild_ms: 0,
        terminal_rows_written: 0,
        peak_worker_rss_bytes: Nullable(None),
    }
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
    let run_row: Option<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    )> = transaction
        .query_row(
            "SELECT request_id, canonical_request_json, request_hash, status,
                    final_envelope_metadata_json, audit_provenance_version
             FROM scan_runs WHERE scan_run_id=?1",
            [scan_run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .map_err(|error| InspectLoadError::before_status(InspectAuditError::Sql(error)))?;
    let (
        stored_request_id,
        canonical_request_json,
        request_hash,
        run_status_text,
        envelope_metadata_json,
        provenance_version,
    ) = run_row.ok_or_else(|| InspectLoadError::before_status(InspectAuditError::RunNotFound))?;
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

    // spec Part 5.1: the full ContextEnvelope is REBUILT from the body-free
    // final_envelope_metadata_json + summary + artifact (never from a
    // body-carrying scan_runs JSON).
    let envelope = match (run_status, envelope_metadata_json) {
        (RunStatus::Running | RunStatus::Abandoned, None) => None,
        (RunStatus::Running | RunStatus::Abandoned, Some(_)) => {
            return Err(InspectLoadError::after_status(
                InspectAuditError::RunCorrupt(
                    "nonterminal run has a final envelope".to_string(),
                ),
                run_status,
            ));
        }
        (_, None) => {
            return Err(InspectLoadError::after_status(
                InspectAuditError::RunCorrupt(
                    "terminal run has no final envelope metadata".to_string(),
                ),
                run_status,
            ));
        }
        (_, Some(metadata_json)) => {
            let rebuilt = crate::store::rebuild_envelope_from_metadata(
                transaction,
                scan_run_id,
                &metadata_json,
            )
            .map_err(|error| {
                InspectLoadError::after_status(
                    InspectAuditError::RunCorrupt(error.to_string()),
                    run_status,
                )
            })?;
            let status_matches = matches!(
                (run_status, rebuilt.status),
                (RunStatus::Success, EngineStatus::Ok)
                    | (RunStatus::Partial, EngineStatus::Partial)
                    | (RunStatus::Error, EngineStatus::Error)
            );
            if !status_matches
                || rebuilt.request_id != stored_request_id
                || rebuilt.scan_run_id.0 != Some(scan_run_id as u64)
            {
                return Err(InspectLoadError::after_status(
                    InspectAuditError::RunCorrupt(
                        "rebuilt envelope does not match its run".to_string(),
                    ),
                    run_status,
                ));
            }
            Some(rebuilt)
        }
    };
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
                // spec Part 2.3: an Error run legitimately carries an error
                // diagnostic (envelope.error == persisted Error row); any other
                // mismatch is corrupt.
                || persisted_error != envelope.error.0
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

    // ---- Inspect v2 provenance (spec Part 5.3): only terminal full_v2 rows
    // carry it; migrated_v1 fails closed and nonterminal rows expose none.
    let audit_provenance_version = match (run_status, provenance_version.as_deref()) {
        (RunStatus::Running | RunStatus::Abandoned, None) => None,
        (RunStatus::Running | RunStatus::Abandoned, Some(_)) => {
            return Err(InspectLoadError::after_status(
                InspectAuditError::RunCorrupt(
                    "nonterminal run has an audit provenance version".to_string(),
                ),
                run_status,
            ));
        }
        (_, Some("full_v2")) => Some(AuditProvenanceVersion::FullV2),
        (_, Some("migrated_v1")) => Some(AuditProvenanceVersion::MigratedV1),
        (_, Some(_)) => {
            return Err(InspectLoadError::after_status(
                InspectAuditError::RunCorrupt(
                    "run has an unknown audit provenance version".to_string(),
                ),
                run_status,
            ));
        }
        (_, None) => {
            return Err(InspectLoadError::after_status(
                InspectAuditError::RunCorrupt(
                    "terminal run has no audit provenance version".to_string(),
                ),
                run_status,
            ));
        }
    };
    let (artifact_id, reused_from_context_run_id, snapshot_hit) = match context.as_ref() {
        Some(_) => {
            let (artifact_id, reused_from, snapshot_hit) =
                load_context_run_provenance(transaction, scan_run_id)
                    .map_err(|error| InspectLoadError::after_status(error, run_status))?;
            if artifact_id.is_none() && !matches!(run_status, RunStatus::Error) {
                return Err(InspectLoadError::after_status(
                    InspectAuditError::RunCorrupt(
                        "success/partial context run has no artifact".to_string(),
                    ),
                    run_status,
                ));
            }
            (artifact_id, reused_from, snapshot_hit)
        }
        None => (None, None, false),
    };
    let (reserved_chars, rendered_chars) = match (context.as_ref(), artifact_id) {
        (Some(_), Some(artifact_id)) => {
            let artifact_id = i64::try_from(artifact_id).map_err(|_| {
                InspectLoadError::after_status(
                    InspectAuditError::RunCorrupt("artifact_id exceeds i64".to_string()),
                    run_status,
                )
            })?;
            load_artifact_semantic_chars(transaction, artifact_id)
                .map_err(|error| InspectLoadError::after_status(error, run_status))?
        }
        _ => (0, 0),
    };
    let files_v2 = if matches!(run_status, RunStatus::Success | RunStatus::Partial | RunStatus::Error)
    {
        let artifact_id = match artifact_id {
            Some(value) => Some(i64::try_from(value).map_err(|_| {
                InspectLoadError::after_status(
                    InspectAuditError::RunCorrupt("artifact_id exceeds i64".to_string()),
                    run_status,
                )
            })?),
            None => None,
        };
        load_file_audits_v2(transaction, scan_run_id, artifact_id)
            .map_err(|error| InspectLoadError::after_status(error, run_status))?
    } else {
        Vec::new()
    };
    let stage_deadline_exhausted = load_stage_deadline_exhausted(transaction, scan_run_id)
        .map_err(|error| InspectLoadError::after_status(error, run_status))?;
    let execution_metrics = load_execution_metrics(transaction, scan_run_id)
        .map_err(|error| InspectLoadError::after_status(error, run_status))?;

    Ok(InspectSnapshot {
        context_run_id,
        run_status,
        summary,
        stage_metrics,
        extension_metrics,
        files,
        decisions,
        warnings,
        audit_provenance_version,
        artifact_id,
        reused_from_context_run_id,
        snapshot_hit,
        reserved_chars,
        rendered_chars,
        stage_deadline_exhausted,
        files_v2,
        execution_metrics,
    })
}

/// Loads the authoritative `execution_metrics` row persisted at finalize
/// (spec Part 5.3). Migrated v1 / nonterminal / engine-error runs have none.
fn load_execution_metrics(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Option<ExecutionMetricsV2>, InspectAuditError> {
    #[allow(clippy::type_complexity)]
    let row: Option<(
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
        i64,
        i64,
        i64,
        Option<i64>,
        Option<i64>,
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
        i64,
        i64,
        i64,
        Option<i64>,
    )> = transaction
        .query_row(
            "SELECT discovery_observed_file_count,
                    source_guard_content_hash_file_count, source_guard_unavailable_count,
                    source_guard_bytes_read, candidate_file_count, admitted_file_count,
                    classification_slot_count, confirmed_run_inspected_pages_total,
                    unobserved_classification_attempt_count, nominal_charged_pages_total,
                    extraction_slot_count, pdfplumber_invocations, snapshot_hit,
                    parse_cache_lookup_count, classification_cache_lookup_count,
                    parse_cache_all_hit, classification_cache_all_hit,
                    stage_deadline_exhausted_count, session_restart_count,
                    session_fallback_count, classify_attempt_count, parse_attempt_count,
                    reserved_chars, rendered_chars, worker_handshake_ms, discovery_ms,
                    snapshot_lookup_ms, current_run_audit_write_ms, terminal_precommit_ms,
                    deadline_precommit_elapsed_ms, envelope_rebuild_ms,
                    terminal_rows_written, peak_worker_rss_bytes
             FROM scan_execution_metrics WHERE scan_run_id=?1",
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
                    row.get(17)?,
                    row.get(18)?,
                    row.get(19)?,
                    row.get(20)?,
                    row.get(21)?,
                    row.get(22)?,
                    row.get(23)?,
                    row.get(24)?,
                    row.get(25)?,
                    row.get(26)?,
                    row.get(27)?,
                    row.get(28)?,
                    row.get(29)?,
                    row.get(30)?,
                    row.get(31)?,
                    row.get(32)?,
                ))
            },
        )
        .optional()
        .map_err(InspectAuditError::Sql)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let parse_bool = |value: i64| match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(InspectAuditError::RunCorrupt("persisted metric bool is invalid".to_string())),
    };
    let metrics = ExecutionMetricsV2 {
        discovery_observed_file_count: to_u64(row.0, "discovery_observed_file_count")?,
        source_guard_content_hash_file_count: to_u64(
            row.1,
            "source_guard_content_hash_file_count",
        )?,
        source_guard_unavailable_count: to_u64(row.2, "source_guard_unavailable_count")?,
        source_guard_bytes_read: to_u64(row.3, "source_guard_bytes_read")?,
        candidate_file_count: to_u64(row.4, "candidate_file_count")?,
        admitted_file_count: to_u64(row.5, "admitted_file_count")?,
        classification_slot_count: to_u64(row.6, "classification_slot_count")?,
        confirmed_run_inspected_pages_total: to_u64(
            row.7,
            "confirmed_run_inspected_pages_total",
        )?,
        unobserved_classification_attempt_count: to_u64(
            row.8,
            "unobserved_classification_attempt_count",
        )?,
        nominal_charged_pages_total: to_u64(row.9, "nominal_charged_pages_total")?,
        extraction_slot_count: to_u64(row.10, "extraction_slot_count")?,
        pdfplumber_invocations: to_u64(row.11, "pdfplumber_invocations")?,
        snapshot_hit: parse_bool(row.12)?,
        parse_cache_lookup_count: to_u64(row.13, "parse_cache_lookup_count")?,
        classification_cache_lookup_count: to_u64(
            row.14,
            "classification_cache_lookup_count",
        )?,
        parse_cache_all_hit: Nullable(row.15.map(parse_bool).transpose()?),
        classification_cache_all_hit: Nullable(row.16.map(parse_bool).transpose()?),
        stage_deadline_exhausted_count: to_u64(row.17, "stage_deadline_exhausted_count")?,
        session_restart_count: to_u64(row.18, "session_restart_count")?,
        session_fallback_count: to_u64(row.19, "session_fallback_count")?,
        classify_attempt_count: to_u64(row.20, "classify_attempt_count")?,
        parse_attempt_count: to_u64(row.21, "parse_attempt_count")?,
        reserved_chars: to_u64(row.22, "reserved_chars")?,
        rendered_chars: to_u64(row.23, "rendered_chars")?,
        worker_handshake_ms: to_u64(row.24, "worker_handshake_ms")?,
        discovery_ms: to_u64(row.25, "discovery_ms")?,
        snapshot_lookup_ms: to_u64(row.26, "snapshot_lookup_ms")?,
        current_run_audit_write_ms: to_u64(row.27, "current_run_audit_write_ms")?,
        terminal_precommit_ms: to_u64(row.28, "terminal_precommit_ms")?,
        deadline_precommit_elapsed_ms: to_u64(row.29, "deadline_precommit_elapsed_ms")?,
        envelope_rebuild_ms: to_u64(row.30, "envelope_rebuild_ms")?,
        terminal_rows_written: to_u64(row.31, "terminal_rows_written")?,
        peak_worker_rss_bytes: Nullable(row.32.map(|value| to_u64(value, "peak_worker_rss_bytes")).transpose()?),
    };
    metrics
        .validate()
        .map_err(|message| InspectAuditError::RunCorrupt(message.to_string()))?;
    Ok(Some(metrics))
}

/// `stage_deadline_exhausted_count` is 0 or 1 (spec Part 5.3): true exactly
/// when a `STAGE_DEADLINE_EXHAUSTED` diagnostic was persisted for this run.
fn load_stage_deadline_exhausted(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<bool, InspectAuditError> {
    let count: i64 = transaction
        .query_row(
            "SELECT count(*) FROM run_diagnostics
             WHERE scan_run_id=?1 AND error_code='STAGE_DEADLINE_EXHAUSTED'",
            [scan_run_id],
            |row| row.get(0),
        )
        .map_err(InspectAuditError::Sql)?;
    if count > 1 {
        return Err(InspectAuditError::RunCorrupt(
            "run persisted multiple stage deadline triggers".to_string(),
        ));
    }
    Ok(count == 1)
}

/// Loads the current `context_runs` snapshot relationship columns:
/// `artifact_id`, `reused_from_context_run_id`, `snapshot_hit`.
fn load_context_run_provenance(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<(Option<u64>, Option<u64>, bool), InspectAuditError> {
    let row: Option<(Option<i64>, Option<i64>, i64)> = transaction
        .query_row(
            "SELECT artifact_id, reused_from_context_run_id, snapshot_hit
             FROM context_runs WHERE scan_run_id=?1",
            [scan_run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(InspectAuditError::Sql)?;
    let Some((artifact_id, reused_from, snapshot_hit)) = row else {
        return Ok((None, None, false));
    };
    let snapshot_hit = match snapshot_hit {
        0 => false,
        1 => true,
        _ => {
            return Err(InspectAuditError::RunCorrupt(
                "context run snapshot_hit is invalid".to_string(),
            ));
        }
    };
    if snapshot_hit && reused_from.is_none() {
        return Err(InspectAuditError::RunCorrupt(
            "snapshot hit context run has no reused_from source".to_string(),
        ));
    }
    if !snapshot_hit && reused_from.is_some() {
        return Err(InspectAuditError::RunCorrupt(
            "non-snapshot context run has a reused_from source".to_string(),
        ));
    }
    Ok((
        artifact_id.map(|value| value as u64),
        reused_from.map(|value| value as u64),
        snapshot_hit,
    ))
}

/// Loads `reserved_chars`/`rendered_chars` from the artifact's immutable
/// semantic summary JSON (spec Part 5.3 `execution_metrics`).
fn load_artifact_semantic_chars(
    transaction: &Transaction<'_>,
    artifact_id: i64,
) -> Result<(u64, u64), InspectAuditError> {
    let summary_json: String = transaction
        .query_row(
            "SELECT semantic_summary_json FROM context_artifacts WHERE artifact_id=?1",
            [artifact_id],
            |row| row.get(0),
        )
        .map_err(InspectAuditError::Sql)?;
    let value: serde_json::Value = serde_json::from_str(&summary_json)
        .map_err(|error| InspectAuditError::RunCorrupt(error.to_string()))?;
    let reserved = value
        .get("reserved_chars")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            InspectAuditError::RunCorrupt("artifact summary is missing reserved_chars".to_string())
        })?;
    let rendered = value
        .get("rendered_chars")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            InspectAuditError::RunCorrupt("artifact summary is missing rendered_chars".to_string())
        })?;
    Ok((reserved, rendered))
}

/// Loads full_v2 per-file rows (SourceGuardV2 + classifier provenance) for
/// `FileAuditV2` assembly (spec Part 5.3). `artifact_id` selects the current
/// run's `context_artifact_files` rows (none for ineligible/migrated runs).
fn load_file_audits_v2(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    artifact_id: Option<i64>,
) -> Result<Vec<FileAuditV2Source>, InspectAuditError> {
    #[allow(clippy::type_complexity)]
    let mut statement = transaction.prepare(
        "SELECT fr.relative_path, fr.file_identity, fr.source_version,
                fi.source_guard_kind, fi.source_guard_sha256, fi.file_type, fi.size_bytes,
                fr.parse_status, fr.parser_backend, fr.worker_lane,
                fr.parse_cache_status, fr.cache_miss_reason,
                fr.truncated, fr.content_sha256, fr.parse_duration_ms,
                fr.failure_class, fr.fallback_backend, fr.fallback_reason_code,
                fr.error_code, fr.error_message, fr.error_retryable, fr.error_stage,
                fr.error_file_path, fr.error_backend,
                af.classifier_status, af.classifier_page_count,
                af.classifier_result_examined_pages, af.classifier_nominal_charged_pages,
                af.classifier_build, af.classifier_profile_hash
         FROM scan_file_results fr
         LEFT JOIN file_inventory fi ON fi.file_identity = fr.file_identity
         LEFT JOIN context_artifact_files af
             ON af.file_identity = fr.file_identity AND af.artifact_id = ?2
         WHERE fr.scan_run_id = ?1
         ORDER BY lower(fr.relative_path), fr.relative_path, fr.file_identity",
    )
    .map_err(InspectAuditError::Sql)?;
    let raw = statement
        .query_map([scan_run_id, artifact_id.unwrap_or(-1)], |row| {
            #[allow(clippy::type_complexity)]
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, i64>(14)?,
                row.get::<_, String>(15)?,
                row.get::<_, String>(16)?,
                row.get::<_, String>(17)?,
                row.get::<_, Option<String>>(18)?,
                row.get::<_, Option<String>>(19)?,
                row.get::<_, Option<i64>>(20)?,
                row.get::<_, Option<String>>(21)?,
                row.get::<_, Option<String>>(22)?,
                row.get::<_, Option<String>>(23)?,
                row.get::<_, Option<String>>(24)?,
                row.get::<_, Option<i64>>(25)?,
                row.get::<_, Option<i64>>(26)?,
                row.get::<_, Option<i64>>(27)?,
                row.get::<_, Option<String>>(28)?,
                row.get::<_, Option<String>>(29)?,
            ))
        })
        .map_err(InspectAuditError::Sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(InspectAuditError::Sql)?;

    let mut rows = Vec::with_capacity(raw.len());
    for row in raw {
        let parse_status: ParseStatus = parse_enum(&row.7, "file v2 parse_status")?;
        let worker_lane: AuditWorkerLane = parse_enum(&row.9, "file v2 worker_lane")?;
        // spec Part 5.3: FileAuditV2.final_diagnostic is non-null only for
        // Error/Timeout; Success and every NotParsed (semantic/policy/runtime
        // and pre-classification reject) carry null. The reject diagnostic
        // stays in the ContextDecision row, not the file audit.
        let final_diagnostic = match parse_status {
            ParseStatus::Error | ParseStatus::Timeout => assemble_final_diagnostic(
                &row.18,
                &row.19,
                row.20,
                &row.21,
                &row.22,
                &row.23,
            )?,
            ParseStatus::Success | ParseStatus::NotParsed => None,
        };
        if let Some(diagnostic) = &final_diagnostic {
            diagnostic
                .validate()
                .map_err(|_| InspectAuditError::RunCorrupt("file final diagnostic is invalid".to_string()))?;
        }
        if matches!(parse_status, ParseStatus::Error | ParseStatus::Timeout)
            && final_diagnostic.is_none()
        {
            return Err(InspectAuditError::RunCorrupt(
                "error/timeout file audit is missing its final diagnostic".to_string(),
            ));
        }
        if matches!(parse_status, ParseStatus::Success | ParseStatus::NotParsed)
            && final_diagnostic.is_some()
        {
            return Err(InspectAuditError::RunCorrupt(
                "success/not_parsed file audit carries a final diagnostic".to_string(),
            ));
        }
        let classifier = match (
            row.24.as_deref(),
            row.25,
            row.26,
            row.27,
            row.28.as_deref(),
            row.29.as_deref(),
        ) {
            (Some(status), page_count, result_pages, nominal, Some(build), Some(profile)) => {
                let status: PdfClassificationStatus = parse_enum(status, "classifier status")?;
                Some(PdfClassificationProvenanceV1 {
                    status,
                    page_count: to_positive_u64(page_count, "classifier page_count")?,
                    result_examined_pages: to_positive_u64(
                        result_pages,
                        "classifier result pages",
                    )?,
                    nominal_charged_pages: to_u64_opt(nominal, "classifier nominal pages")?,
                    classifier_build: build.to_string(),
                    classifier_profile_hash: profile.to_string(),
                })
            }
            (None, None, None, None, None, None) => None,
            _ => {
                return Err(InspectAuditError::RunCorrupt(
                    "classifier provenance columns are inconsistent".to_string(),
                ));
            }
        };
        rows.push(FileAuditV2Source {
            relative_path: row.0,
            file_identity: row.1,
            source_version: row.2,
            source_guard_kind: row.3,
            source_guard_sha256: row.4,
            file_type: row.5,
            size_bytes: to_u64_opt(Some(row.6), "file size_bytes")?,
            parse_status,
            parser_backend: row.8,
            worker_lane,
            parse_cache_status: row.10,
            cache_miss_reason: row.11,
            truncated: parse_bool(row.12, "file v2 truncated")?,
            content_sha256: row.13,
            parse_duration_ms: to_u64_opt(Some(row.14), "file v2 parse_duration_ms")?,
            failure_class: row.15,
            fallback_backend: row.16,
            fallback_reason_code: row.17,
            final_diagnostic,
            classifier,
        });
    }
    Ok(rows)
}

/// Rebuilds the nullable `final_diagnostic` from the persisted error columns.
fn assemble_final_diagnostic(
    error_code: &Option<String>,
    error_message: &Option<String>,
    error_retryable: Option<i64>,
    error_stage: &Option<String>,
    error_file_path: &Option<String>,
    error_backend: &Option<String>,
) -> Result<Option<Diagnostic>, InspectAuditError> {
    match (error_code, error_message, error_retryable, error_stage) {
        (None, None, None, None) => Ok(None),
        (Some(_), Some(_), Some(_), Some(_)) => {
            let diagnostic = Diagnostic {
                error_code: parse_enum(
                    error_code.as_deref().unwrap_or_default(),
                    "final diagnostic error_code",
                )?,
                message: error_message.clone().unwrap_or_default(),
                retryable: parse_bool(error_retryable.unwrap_or_default(), "diagnostic retryable")?,
                stage: parse_enum(
                    error_stage.as_deref().unwrap_or_default(),
                    "final diagnostic stage",
                )?,
                file_path: Nullable(error_file_path.clone()),
                backend: Nullable(error_backend.clone()),
            };
            diagnostic.validate().map_err(|_| {
                InspectAuditError::RunCorrupt("final diagnostic is invalid".to_string())
            })?;
            Ok(Some(diagnostic))
        }
        _ => Err(InspectAuditError::RunCorrupt(
            "persisted final diagnostic is partially populated".to_string(),
        )),
    }
}

fn to_positive_u64(value: Option<i64>, field: &str) -> Result<Option<u64>, InspectAuditError> {
    value
        .map(|value| u64::try_from(value).map_err(|_| {
            InspectAuditError::RunCorrupt(format!("persisted {field} is negative"))
        }))
        .transpose()
}

fn to_u64_opt(value: Option<i64>, field: &str) -> Result<u64, InspectAuditError> {
    let value = value.ok_or_else(|| {
        InspectAuditError::RunCorrupt(format!("persisted {field} is missing"))
    })?;
    u64::try_from(value).map_err(|_| {
        InspectAuditError::RunCorrupt(format!("persisted {field} is negative"))
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
            // spec Part 5.3 v1 lossy projection: v2-only miss reasons are
            // remapped to the frozen v1 values (`parser_identity_changed` is a
            // lossless projection to `parser_profile_changed`;
            // `entry_absent_or_evicted` projects as `new_file` and the caller
            // adds the `CACHE_MISS_REASON_PROJECTED_AS_NEW_FILE` warning).
            let miss_reason = match row.7.as_str() {
                "parser_identity_changed" => "parser_profile_changed".to_string(),
                "entry_absent_or_evicted" => "new_file".to_string(),
                other => other.to_string(),
            };
            let file = FileAudit {
                relative_path: row.0,
                file_identity: row.1,
                source_version: row.2,
                parse_status: parse_enum(&row.3, "file parse_status")?,
                parser_backend: row.4,
                worker_lane: parse_enum(&row.5, "file worker_lane")?,
                cache_status: parse_enum(&row.6, "file cache_status")?,
                cache_miss_reason: parse_enum(&miss_reason, "file cache_miss_reason")?,
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
