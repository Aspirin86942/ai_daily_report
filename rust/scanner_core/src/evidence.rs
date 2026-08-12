//! Complete evidence assembly for the current scanner run.

use ai_daily_scanner_contract::{
    ClassificationCacheStatus, ClassificationTransport, FileAuditV2, Nullable, ParseCacheStatus,
    PdfClassificationAuditV1, PdfClassificationStatus, ReuseKind, RunStatus, ScannerEvidence,
    SourceGuardKind, Validate,
};

use crate::artifact::PdfClassificationProvenanceV1;
use crate::context_audit::{assemble_execution_metrics_v2, EvidenceSnapshot, FileAuditV2Source};

/// Assembles complete evidence from the current schema's committed rows.
pub fn assemble_scanner_evidence(
    request_id: &str,
    scan_run_id: u64,
    snapshot: &EvidenceSnapshot,
) -> Result<ScannerEvidence, String> {
    if snapshot
        .files_v2
        .iter()
        .any(|row| row.parse_transport.is_none() || row.parse_attempt_count.is_none())
    {
        return Err("file execution provenance is unavailable".to_string());
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
    execution_metrics
        .validate()
        .map_err(|message| format!("execution metrics are invalid: {message}"))?;
    let reuse_kind = determine_reuse_kind(snapshot, &execution_metrics);
    let evidence = ScannerEvidence {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request_id.to_string(),
        scan_run_id,
        context_run_id: Nullable(snapshot.context_run_id),
        run_status: snapshot.run_status,
        summary: snapshot.summary.clone(),
        stage_metrics: snapshot.stage_metrics.clone(),
        extension_metrics: snapshot.extension_metrics.clone(),
        files,
        decisions: snapshot.decisions.clone(),
        warnings: snapshot.warnings.clone(),
        artifact_id: Nullable(snapshot.artifact_id),
        reused_from_context_run_id: Nullable(snapshot.reused_from_context_run_id),
        reuse_kind,
        execution_metrics,
    };
    evidence
        .validate()
        .map_err(|message| format!("scanner evidence is invalid: {message}"))?;
    Ok(evidence)
}

/// Spec Part 5.3 reuse-kind rules:
/// - Error runs always report `none` (and a null reused_from);
/// - `context_snapshot`: snapshot hit and reused_from non-null;
/// - `parse_cache`: no snapshot, >=1 parse lookup and parse_cache_all_hit=true;
/// - otherwise `none`.
fn determine_reuse_kind(
    snapshot: &EvidenceSnapshot,
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

/// Assembles one strict `FileAuditV2` from a persisted current-schema row.
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
        None => return Err("file row has no parse_cache_status".to_string()),
    };
    let parse_transport = row
        .parse_transport
        .ok_or_else(|| "file execution provenance is unavailable".to_string())?;
    let parse_attempt_count = row
        .parse_attempt_count
        .ok_or_else(|| "file execution provenance is unavailable".to_string())?;
    // spec Part 5.3/Part 3: the immutable artifact provenance only maps to a
    // snapshot audit when THIS current row is actually a snapshot row. A cold
    // run must never stamp snapshot identity onto real execution
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
        classification
            .validate()
            .map_err(|message| format!("pdf classification audit is invalid: {message}"))?;
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
