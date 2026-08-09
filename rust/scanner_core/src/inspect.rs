//! Inspect v2 assembly + v1 lossy projection (spec Part 5.3).
//!
//! `InspectRunResponseV2` is a separate strict observability interface: it is
//! never added to `ContextEnvelope v1`. Full_v2 runs produce the strict v2
//! object; migrated v1 runs fail closed with `INSPECT_V2_PROVENANCE_UNAVAILABLE`.
//! The default v1 inspect stays lossy for full_v2 rows and appends the four
//! projection warnings (output-only, never written back to full diagnostics).

use ai_daily_scanner_contract::{
    ClassificationCacheStatus, ClassificationTransport, Diagnostic, DiagnosticStage, ErrorCode,
    FileAuditV2, InspectRunRequest, InspectRunResponseV2, InspectStatus, Nullable,
    ParseCacheStatus,
    PdfClassificationAuditV1, PdfClassificationStatus, ReuseKind, RunStatus, SourceGuardKind,
    Validate,
};

use crate::artifact::PdfClassificationProvenanceV1;
use crate::context_audit::{
    assemble_execution_metrics_v2, AuditProvenanceVersion, FileAuditV2Source, InspectSnapshot,
};

/// Error sentinel execution object is validated by `InspectRunResponseV2`
/// `status=error`; it never represents the inspected run.
fn error_sentinel() -> ai_daily_scanner_contract::ExecutionMetricsV2 {
    ai_daily_scanner_contract::ExecutionMetricsV2 {
        discovery_observed_file_count: 0,
        source_guard_content_hash_file_count: 0,
        source_guard_unavailable_count: 0,
        source_guard_bytes_read: 0,
        candidate_file_count: 0,
        admitted_file_count: 0,
        classification_slot_count: 0,
        confirmed_run_inspected_pages_total: 0,
        unobserved_classification_attempt_count: 0,
        nominal_charged_pages_total: 0,
        extraction_slot_count: 0,
        pdfplumber_invocations: 0,
        snapshot_hit: false,
        parse_cache_lookup_count: 0,
        classification_cache_lookup_count: 0,
        parse_cache_all_hit: Nullable(None),
        classification_cache_all_hit: Nullable(None),
        stage_deadline_exhausted_count: 0,
        session_restart_count: 0,
        session_fallback_count: 0,
        classify_attempt_count: 0,
        parse_attempt_count: 0,
        reserved_chars: 0,
        rendered_chars: 0,
        worker_handshake_ms: 0,
        discovery_ms: 0,
        snapshot_lookup_ms: 0,
        current_run_audit_write_ms: 0,
        terminal_precommit_ms: 0,
        deadline_precommit_elapsed_ms: 0,
        envelope_rebuild_ms: 0,
        terminal_rows_written: 0,
        peak_worker_rss_bytes: Nullable(None),
    }
}

/// Assembles the strict v2 response for a full_v2 run. The caller has already
/// validated `audit_provenance_version == FullV2` (migrated runs fail closed in
/// `inspect_v2_error`).
pub fn assemble_inspect_v2(
    request: &InspectRunRequest,
    snapshot: &InspectSnapshot,
) -> Result<InspectRunResponseV2, String> {
    if snapshot.audit_provenance_version != Some(AuditProvenanceVersion::FullV2) {
        return Err("full_v2 provenance is required for inspect v2".to_string());
    }
    if snapshot.files_v2.iter().any(|row| {
        row.parse_transport.is_none() || row.parse_attempt_count.is_none()
    }) {
        return Err("full_v2 file execution provenance is unavailable".to_string());
    }
    let files = snapshot
        .files_v2
        .iter()
        .map(assemble_file_audit_v2)
        .collect::<Result<Vec<_>, _>>()?;
    for file in &files {
        file.validate()
            .map_err(|message| format!("file audit v2 is invalid: {message}"))?;
    }
    let execution_metrics = match snapshot.execution_metrics.as_ref() {
        Some(metrics) => metrics.clone(),
        // Engine-error runs with no scheduler outcome have no persisted row;
        // the derive reconstructs the mostly-zero object from the empty rows.
        None => assemble_execution_metrics_v2(snapshot),
    };
    execution_metrics.validate().map_err(|message| {
        format!("execution metrics are invalid: {message}")
    })?;
    let reuse_kind = determine_reuse_kind(snapshot, &execution_metrics);
    let response = InspectRunResponseV2 {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        response_version: 2,
        request_id: request.request_id.clone(),
        scan_run_id: request.scan_run_id,
        context_run_id: Nullable(snapshot.context_run_id),
        status: InspectStatus::Ok,
        run_status: Nullable(Some(snapshot.run_status)),
        summary: snapshot.summary.clone(),
        stage_metrics: snapshot.stage_metrics.clone(),
        extension_metrics: snapshot.extension_metrics.clone(),
        files,
        decisions: snapshot.decisions.clone(),
        warnings: snapshot.warnings.clone(),
        error: Nullable(None),
        artifact_id: Nullable(snapshot.artifact_id),
        reused_from_context_run_id: Nullable(snapshot.reused_from_context_run_id),
        reuse_kind,
        execution_metrics,
    };
    response
        .validate()
        .map_err(|message| format!("inspect v2 response is invalid: {message}"))?;
    Ok(response)
}

/// Spec Part 5.3 reuse-kind rules:
/// - Error runs always report `none` (and a null reused_from);
/// - `context_snapshot`: snapshot hit and reused_from non-null;
/// - `parse_cache`: no snapshot, >=1 parse lookup and parse_cache_all_hit=true;
/// - otherwise `none`.
fn determine_reuse_kind(
    snapshot: &InspectSnapshot,
    metrics: &ai_daily_scanner_contract::ExecutionMetricsV2,
) -> ReuseKind {
    if snapshot.run_status == RunStatus::Error {
        return ReuseKind::None;
    }
    if snapshot.snapshot_hit && snapshot.reused_from_context_run_id.is_some() {
        ReuseKind::ContextSnapshot
    } else if !snapshot.snapshot_hit
        && metrics.parse_cache_lookup_count > 0
        && metrics.parse_cache_all_hit.0 == Some(true)
    {
        ReuseKind::ParseCache
    } else {
        ReuseKind::None
    }
}

/// Strict error-arm v2 response (sentinel execution metrics; never the
/// inspected run's evidence). `migrated_v1` uses `INSPECT_V2_PROVENANCE_UNAVAILABLE`.
pub fn inspect_v2_error(
    request: &InspectRunRequest,
    error_code: ErrorCode,
    message: String,
    retryable: bool,
    run_status: Option<RunStatus>,
) -> InspectRunResponseV2 {
    let response = InspectRunResponseV2 {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        response_version: 2,
        request_id: request.request_id.clone(),
        scan_run_id: request.scan_run_id,
        context_run_id: Nullable(None),
        status: InspectStatus::Error,
        run_status: Nullable(run_status),
        summary: empty_summary(),
        stage_metrics: Vec::new(),
        extension_metrics: Vec::new(),
        files: Vec::new(),
        decisions: Vec::new(),
        warnings: Vec::new(),
        error: Nullable(Some(Diagnostic {
            error_code,
            message: message.chars().take(4_096).collect(),
            retryable,
            stage: DiagnosticStage::Inspect,
            file_path: Nullable(None),
            backend: Nullable(None),
        })),
        artifact_id: Nullable(None),
        reused_from_context_run_id: Nullable(None),
        reuse_kind: ReuseKind::None,
        execution_metrics: error_sentinel(),
    };
    debug_assert!(
        response.validate().is_ok(),
        "inspect v2 error sentinel violates the wire contract"
    );
    response
}

fn empty_summary() -> ai_daily_scanner_contract::ContextSummary {
    ai_daily_scanner_contract::ContextSummary {
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

/// Assembles one strict `FileAuditV2` from the persisted full_v2 row
/// (spec Part 5.3 field/order + nullability).
fn assemble_file_audit_v2(row: &FileAuditV2Source) -> Result<FileAuditV2, String> {
    let source_guard_kind = match row.source_guard_kind.as_deref() {
        Some(kind) => parse_source_guard_kind(kind)?,
        None => SourceGuardKind::Unavailable,
    };
    let parse_cache_status = match row.parse_cache_status.as_deref() {
        Some("fresh") => ParseCacheStatus::Fresh,
        Some("miss") => ParseCacheStatus::Miss,
        Some("snapshot") => ParseCacheStatus::Snapshot,
        Some("not_applicable") => ParseCacheStatus::NotApplicable,
        Some(value) => return Err(format!("unknown parse_cache_status: {value}")),
        None => return Err("full_v2 file row has no parse_cache_status".to_string()),
    };
    let parse_transport = row
        .parse_transport
        .ok_or_else(|| "full_v2 file execution provenance is unavailable".to_string())?;
    let parse_attempt_count = row
        .parse_attempt_count
        .ok_or_else(|| "full_v2 file execution provenance is unavailable".to_string())?;
    // spec Part 5.3/Part 3: the immutable artifact provenance only maps to a
    // snapshot audit when THIS current row is actually a snapshot row. A cold
    // run's own inspect must never stamp snapshot identity onto real execution
    // (its run pages/duration are not persisted), so `pdf_classification` is
    // null for miss/not_applicable rows.
    let pdf_classification = match row.classification_execution.as_ref() {
        Some(execution) => Some(execution.clone()),
        None => match row.classifier.as_ref() {
            Some(provenance) if row.parse_cache_status.as_deref() == Some("snapshot") => {
                assemble_snapshot_classification(provenance)
            }
            _ => None,
        },
    };
    if let Some(classification) = &pdf_classification {
        classification.validate().map_err(|message| {
            format!("pdf classification audit is invalid: {message}")
        })?;
    }
    let file = FileAuditV2 {
        relative_path: row.relative_path.clone(),
        file_identity: row.file_identity.clone(),
        source_version: row.source_version.clone(),
        source_guard_kind,
        source_guard_sha256: Nullable(row.source_guard_sha256.clone()),
        parse_status: row.parse_status,
        parser_backend: row.parser_backend.clone(),
        worker_lane: row.worker_lane,
        parse_cache_status,
        cache_miss_reason: row.cache_miss_reason.clone(),
        truncated: row.truncated,
        content_sha256: row.content_sha256.clone(),
        parse_duration_ms: row.parse_duration_ms,
        failure_class: row.failure_class.clone(),
        fallback_backend: row.fallback_backend.clone(),
        fallback_reason_code: row.fallback_reason_code.clone(),
        parse_transport,
        parse_attempt_count,
        final_diagnostic: Nullable(row.final_diagnostic.clone()),
        pdf_classification: Nullable(pdf_classification),
    };
    file.validate()
        .map_err(|message| format!("file audit v2 is invalid: {message}"))?;
    Ok(file)
}

/// Rebuilds the snapshot `PdfClassificationAuditV1` from the artifact's
/// immutable provenance (spec Part 3: text/no-text context snapshot row keeps
/// page counts, zeroes every execution field).
fn assemble_snapshot_classification(
    provenance: &PdfClassificationProvenanceV1,
) -> Option<PdfClassificationAuditV1> {
    match provenance.status {
        PdfClassificationStatus::TextInParseWindow
        | PdfClassificationStatus::NoTextInParseWindow => Some(PdfClassificationAuditV1 {
            status: provenance.status,
            page_count: Nullable(provenance.page_count),
            classification_cache_status: ClassificationCacheStatus::Snapshot,
            classification_cache_miss_reason: String::new(),
            result_examined_pages: Nullable(provenance.result_examined_pages),
            run_inspected_pages: Nullable(Some(0)),
            nominal_charged_pages: provenance.nominal_charged_pages,
            duration_ms: 0,
            transport: ClassificationTransport::Snapshot,
            attempt_count: 0,
            classifier_build: provenance.classifier_build.clone(),
            classifier_profile_hash: provenance.classifier_profile_hash.clone(),
        }),
        PdfClassificationStatus::NotClassifiedByBudget => Some(PdfClassificationAuditV1 {
            status: provenance.status,
            page_count: Nullable(None),
            classification_cache_status: ClassificationCacheStatus::NotEligible,
            classification_cache_miss_reason: String::new(),
            result_examined_pages: Nullable(Some(0)),
            run_inspected_pages: Nullable(Some(0)),
            nominal_charged_pages: 0,
            duration_ms: 0,
            transport: ClassificationTransport::NotApplicable,
            attempt_count: 0,
            classifier_build: provenance.classifier_build.clone(),
            classifier_profile_hash: provenance.classifier_profile_hash.clone(),
        }),
        PdfClassificationStatus::Unknown | PdfClassificationStatus::Error => None,
    }
}

fn parse_source_guard_kind(value: &str) -> Result<SourceGuardKind, String> {
    match value {
        "windows_file_id_change_time_v1" => Ok(SourceGuardKind::WindowsFileIdChangeTimeV1),
        "unix_inode_ctime_v1" => Ok(SourceGuardKind::UnixInodeCtimeV1),
        "content_sha256_v1" => Ok(SourceGuardKind::ContentSha256V1),
        "unavailable" => Ok(SourceGuardKind::Unavailable),
        _ => Err(format!("unknown source guard kind: {value}")),
    }
}

// ---------------------------------------------------------------------------
// v1 lossy projection (spec Part 5.3)
// ---------------------------------------------------------------------------

/// Projection warning diagnostic with the frozen `stage=inspect,
/// retryable=false, file_path/backend=null` shape.
fn projection_warning(error_code: ErrorCode, message: &str) -> Diagnostic {
    Diagnostic {
        error_code,
        message: message.to_string(),
        retryable: false,
        stage: DiagnosticStage::Inspect,
        file_path: Nullable(None),
        backend: Nullable(None),
    }
}

/// Adds the v1 lossy projection warnings for full_v2 rows to the existing
/// warnings (spec Part 5.3). Output-only: never written back to full
/// diagnostics, envelope metadata or snapshot eligibility. Merged warnings stay
/// within the 257 projection bound (256 detail + 1 `DIAGNOSTICS_AGGREGATED`).
pub fn v1_lossy_projection_warnings(
    snapshot: &InspectSnapshot,
    existing: &[Diagnostic],
) -> Vec<Diagnostic> {
    let mut warnings = existing.to_vec();
    let mut any_source_guard = false;
    for file in &snapshot.files_v2 {
        match file.parse_cache_status.as_deref() {
            Some("snapshot") => warnings.push(projection_warning(
                ErrorCode::SnapshotReuseProjectedAsFresh,
                "snapshot reuse projected as a fresh parse-cache hit",
            )),
            Some("not_applicable") => warnings.push(projection_warning(
                ErrorCode::ParseCacheNotApplicableProjectedAsMiss,
                "not-applicable parse cache projected as a miss",
            )),
            _ => {}
        }
        if file.cache_miss_reason == "entry_absent_or_evicted" {
            warnings.push(projection_warning(
                ErrorCode::CacheMissReasonProjectedAsNewFile,
                "cache miss reason projected as new_file",
            ));
        }
        if file.source_guard_kind.is_some() {
            any_source_guard = true;
        }
    }
    if any_source_guard {
        warnings.push(projection_warning(
            ErrorCode::SourceGuardNotProjected,
            "SourceGuardV2 is not representable in the v1 inspect projection",
        ));
    }
    apply_warning_bound(warnings)
}

/// Keeps at most 256 detail warnings plus one `DIAGNOSTICS_AGGREGATED` row.
fn apply_warning_bound(mut warnings: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let detail_limit = 256;
    if warnings.len() <= detail_limit {
        return warnings;
    }
    let aggregate = warnings
        .iter()
        .any(|warning| warning.error_code == ErrorCode::DiagnosticsAggregated);
    warnings.truncate(detail_limit);
    if aggregate {
        warnings
    } else {
        warnings.push(projection_warning(
            ErrorCode::DiagnosticsAggregated,
            "too many diagnostics were aggregated",
        ));
        warnings
    }
}
