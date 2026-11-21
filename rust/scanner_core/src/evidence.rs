//! Complete evidence assembly for the current scanner run.

use ai_daily_scanner_contract::{
    FileAuditV2, Nullable, ParseCacheStatus, PdfClassificationAuditV1, PdfClassificationStatus,
    ReuseKind, RunStatus, ScannerEvidence, SourceGuardKind, Validate,
};

use crate::artifact::PdfClassificationProvenanceV1;
use crate::context_audit::{assemble_execution_metrics_v2, EvidenceSnapshot, FileAuditV2Source};

/// Assembles complete evidence from the current schema's committed rows.
pub fn assemble_scanner_evidence(
    request_id: &str,
    scan_run_id: u64,
    mut snapshot: EvidenceSnapshot,
) -> Result<ScannerEvidence, String> {
    let execution_metrics = snapshot
        .execution_metrics
        .take()
        .unwrap_or_else(|| assemble_execution_metrics_v2(&snapshot));
    let reuse_kind = determine_reuse_kind(&snapshot, &execution_metrics);
    let files = snapshot
        .files_v2
        .into_iter()
        .map(assemble_file_audit_v2)
        .collect::<Result<Vec<_>, _>>()?;
    let evidence = ScannerEvidence {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request_id.to_string(),
        scan_run_id,
        context_run_id: Nullable(snapshot.context_run_id),
        run_status: snapshot.run_status,
        summary: snapshot.summary,
        stage_metrics: snapshot.stage_metrics,
        extension_metrics: snapshot.extension_metrics,
        files,
        decisions: snapshot.decisions,
        warnings: snapshot.warnings,
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
fn assemble_file_audit_v2(row: FileAuditV2Source) -> Result<FileAuditV2, String> {
    let source_guard_kind = match row.source_guard_kind.as_deref() {
        Some(kind) => crate::source_guard::source_guard_kind_from_text(kind)
            .ok_or_else(|| format!("unknown source guard kind: {kind}"))?,
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
    let (parse_transport, parse_attempt_count) =
        row.parse_transport
            .zip(row.parse_attempt_count)
            .ok_or_else(|| "file execution provenance is unavailable".to_string())?;
    // Executed rows keep their recorded audit. Only snapshot rows inherit artifact counts.
    let pdf_classification = row
        .classification_execution
        .or_else(|| match parse_cache_status {
            ParseCacheStatus::Snapshot => row.classifier.and_then(assemble_snapshot_classification),
            _ => None,
        });
    Ok(FileAuditV2 {
        relative_path: row.relative_path,
        file_identity: row.file_identity,
        source_version: row.source_version,
        source_guard_kind,
        source_guard_sha256: Nullable(row.source_guard_sha256),
        parse_status: row.parse_status,
        parser_backend: row.parser_backend,
        worker_lane: row.worker_lane,
        parse_cache_status,
        cache_miss_reason: row.cache_miss_reason,
        truncated: row.truncated,
        content_sha256: row.content_sha256,
        parse_duration_ms: row.parse_duration_ms,
        failure_class: row.failure_class,
        fallback_backend: row.fallback_backend,
        fallback_reason_code: row.fallback_reason_code,
        parse_transport,
        parse_attempt_count,
        final_diagnostic: Nullable(row.final_diagnostic),
        pdf_classification: Nullable(pdf_classification),
    })
}

/// Historical evidence rows omit unknown/error observations and normalize a skipped classifier.
fn assemble_snapshot_classification(
    mut provenance: PdfClassificationProvenanceV1,
) -> Option<PdfClassificationAuditV1> {
    match provenance.status {
        PdfClassificationStatus::TextInParseWindow
        | PdfClassificationStatus::NoTextInParseWindow => {}
        PdfClassificationStatus::NotClassifiedByBudget => {
            provenance.page_count = None;
            provenance.result_examined_pages = Some(0);
            provenance.nominal_charged_pages = 0;
        }
        PdfClassificationStatus::Unknown | PdfClassificationStatus::Error => return None,
    }
    Some(provenance.into_snapshot_audit())
}
