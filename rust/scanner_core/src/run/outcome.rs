use super::*;

pub(super) fn apply_worker_rss_observation(
    outcome: &mut BudgetedScanOutcome,
    peak_worker_rss_bytes: Option<u64>,
) {
    if peak_worker_rss_bytes.is_some() {
        return;
    }
    outcome.diagnostics.push(RunDiagnosticRecord {
        severity: DiagnosticSeverity::Warning,
        diagnostic: worker_rss_unavailable_diagnostic(),
    });
}

pub(super) fn combine_peak_rss(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        _ => None,
    }
}

pub(super) fn worker_rss_unavailable_diagnostic() -> Diagnostic {
    diagnostic(
        ErrorCode::WorkerRssUnavailable,
        "worker peak RSS could not be observed".to_string(),
        false,
        DiagnosticStage::Process,
    )
}

/// Rebuilds the frozen `ContextEnvelope` from a scheduler outcome. The caller
/// must not re-decide actions/counts/admission; this only projects the outcome
/// onto the envelope shape and the terminal record.
pub(super) fn scheduler_outcome_envelope(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    active: &ActiveRun,
    outcome: &crate::scheduler::BudgetedScanOutcome,
    error_context: Option<&ContextRunRecord>,
) -> (ContextEnvelope, RunStatus) {
    let (run_status, engine_status) = match outcome.terminal_intent {
        TerminalIntent::Success => (RunStatus::Success, EngineStatus::Ok),
        TerminalIntent::Partial => (RunStatus::Partial, EngineStatus::Partial),
        TerminalIntent::Error => (RunStatus::Error, EngineStatus::Error),
    };
    let warnings: Vec<Diagnostic> = outcome
        .diagnostics
        .iter()
        .filter(|record| record.severity == DiagnosticSeverity::Warning)
        .map(|record| record.diagnostic.clone())
        .take(100_000)
        .collect();
    let error: Option<Diagnostic> = outcome
        .diagnostics
        .iter()
        .find(|record| record.severity == DiagnosticSeverity::Error)
        .map(|record| record.diagnostic.clone());
    // spec Part 2.3: every committed terminal run (Success/Partial/Error) returns
    // `context_run_id = scan_run_id`. An Error run's summary is the derived
    // reconciling summary (empty file_context), never a fabricated zero object.
    let (file_context, summary, context_run_id) = match &outcome.context {
        Some(context) => (
            context.final_context.clone(),
            context.summary.clone(),
            Some(active.context_run_id()),
        ),
        None if run_status == RunStatus::Error => (
            String::new(),
            error_context
                .map(|context| context.summary.clone())
                .unwrap_or_default(),
            Some(active.context_run_id()),
        ),
        None => (String::new(), ContextSummary::default(), None),
    };
    let envelope = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        engine_version: version.engine_version.clone(),
        engine_build: version.engine_build.clone(),
        status: engine_status,
        file_context,
        summary,
        scan_run_id: Nullable(Some(active.scan_run_id())),
        context_run_id: Nullable(context_run_id),
        warnings,
        error: Nullable(error),
    };
    (envelope, run_status)
}

/// Derives the minimal `context_runs` record for an Error outcome (spec
/// Part 2.3 + Part 2.2 count equations): one decision per file result
/// (Error/Timeout → Error, NotParsed → Omit, Success → Keep), an empty
/// `file_context`, and a summary that reconciles with the persisted
/// file/decision/stage rows. Durations come from the stage metrics so the
/// relational summary validation passes; never copies a Success/Partial render.
pub(super) fn build_error_context_record(
    file_results: &[FileResultRecord],
    inventory: &[InventoryRecord],
    stage_metrics: &[StageMetric],
    context_profile_hash: &str,
) -> ContextRunRecord {
    let size_by_identity: std::collections::HashMap<&str, u64> = inventory
        .iter()
        .map(|item| (item.file_identity.as_str(), item.size_bytes))
        .collect();
    let mut summary = ContextSummary {
        source_file_count: file_results.len() as u64,
        ..ContextSummary::default()
    };
    let mut decisions = Vec::with_capacity(file_results.len());
    for result in file_results {
        let (action, reason) = match result.parse_status {
            ParseStatus::Success => {
                summary.success_count += 1;
                (ContextAction::Keep, "small_file_keep")
            }
            ParseStatus::NotParsed => {
                summary.omitted_file_count += 1;
                (ContextAction::Omit, "not_parsed")
            }
            ParseStatus::Error => {
                summary.error_file_count += 1;
                (ContextAction::Error, "parse_error")
            }
            ParseStatus::Timeout => {
                summary.timeout_count += 1;
                (ContextAction::Error, "parse_error")
            }
        };
        let input_chars = size_by_identity
            .get(result.file_identity.as_str())
            .copied()
            .unwrap_or(0);
        summary.input_chars += input_chars;
        decisions.push(ContextDecisionRecord {
            file_identity: result.file_identity.clone(),
            decision: ContextDecision {
                relative_path: result.relative_path.clone(),
                action,
                reason: reason.into(),
                priority: 0,
                input_chars,
                output_chars: 0,
                truncated: result.truncated,
                error_code: result
                    .error
                    .as_ref()
                    .map(|diagnostic| crate::store::inventory::enum_text(&diagnostic.error_code))
                    .unwrap_or_default(),
            },
        });
    }
    summary.included_file_count = summary.success_count;
    for metric in stage_metrics {
        summary.total_duration_ms += metric.duration_ms;
        match metric.stage {
            StageName::Discovery => summary.discovery_duration_ms = metric.duration_ms,
            StageName::Parse => summary.parse_duration_ms = metric.duration_ms,
            StageName::Context => summary.compression_duration_ms = metric.duration_ms,
            StageName::Cache => {}
        }
    }
    ContextRunRecord {
        context_profile_hash: context_profile_hash.to_string(),
        status: RunStatus::Error,
        final_context: String::new(),
        context_sha256: crate::store::sha256_hex(b""),
        summary,
        decisions,
    }
}

/// Assembles the strict `execution_metrics` for the scheduler path (spec
/// Part 5.3). The scheduler owns the plan/execution counts; the run shell owns
/// the wall timings. `current_run_audit_write_ms`/`terminal_precommit_ms`/
/// `envelope_rebuild_ms`/`terminal_rows_written` are filled by `store.finalize`.
pub(super) fn assemble_scheduler_execution_metrics(
    metrics: &crate::scheduler::ExecutionMetrics,
    session: Option<&crate::session::SessionPoolStats>,
    peak_worker_rss_bytes: Option<u64>,
    worker_handshake_ms: u64,
    discovery_ms: u64,
    snapshot_lookup_ms: u64,
) -> ai_daily_scanner_contract::ExecutionMetricsV2 {
    let mut output = base_execution_metrics(
        worker_handshake_ms,
        discovery_ms,
        metrics.discovery_observed_file_count,
        SourceGuardObservationMetrics {
            content_hash_file_count: metrics.source_guard_content_hash_file_count,
            unavailable_file_count: metrics.source_guard_unavailable_count,
            bytes_read: metrics.source_guard_bytes_read,
        },
        peak_worker_rss_bytes,
    );
    output.refill_audit = metrics.refill_audit.clone();
    output.candidate_file_count = metrics.candidate_file_count;
    output.admitted_file_count = metrics.admitted_file_count;
    output.classification_slot_count = metrics.classification_slot_count;
    output.confirmed_run_inspected_pages_total = metrics.confirmed_run_inspected_pages_total;
    output.unobserved_classification_attempt_count =
        metrics.unobserved_classification_attempt_count;
    output.nominal_charged_pages_total = metrics.nominal_charged_pages_total;
    output.extraction_slot_count = metrics.extraction_slot_count;
    output.pdfplumber_invocations = metrics.pdfplumber_invocations;
    output.parse_cache_lookup_count = metrics.parse_cache_lookup_count;
    output.classification_cache_lookup_count = metrics.classification_cache_lookup_count;
    output.parse_cache_all_hit = Nullable(metrics.parse_cache_all_hit);
    output.classification_cache_all_hit = Nullable(metrics.classification_cache_all_hit);
    output.stage_deadline_exhausted_count = metrics.stage_deadline_exhausted_count;
    output.session_restart_count = session.map_or(0, |stats| stats.session_restart_count);
    output.classify_attempt_count = metrics.classify_attempt_count;
    output.parse_attempt_count = metrics.parse_attempt_count;
    output.reserved_chars = metrics.reserved_chars;
    output.rendered_chars = metrics.rendered_chars;
    output.snapshot_lookup_ms = snapshot_lookup_ms;
    output.deadline_precommit_elapsed_ms = metrics.deadline_precommit_elapsed_ms;
    output
}

/// Assembles the strict `execution_metrics` for a snapshot-hit current run
/// (spec Part 5.4): the scheduler did not run, so every plan/execution count is
/// 0; discovery/source-guard counts and the live-handshake/lookup/terminal wall
/// spans are this run's real measured values.
#[allow(clippy::too_many_arguments)]
pub(super) fn assemble_snapshot_execution_metrics(
    discovery_observed_file_count: u64,
    guards: SourceGuardObservationMetrics,
    reserved_chars: u64,
    rendered_chars: u64,
    worker_handshake_ms: u64,
    discovery_ms: u64,
    snapshot_lookup_ms: u64,
    deadline_precommit_elapsed_ms: u64,
    peak_worker_rss_bytes: Option<u64>,
) -> ai_daily_scanner_contract::ExecutionMetricsV2 {
    let mut output = base_execution_metrics(
        worker_handshake_ms,
        discovery_ms,
        discovery_observed_file_count,
        guards,
        peak_worker_rss_bytes,
    );
    output.snapshot_hit = true;
    output.reserved_chars = reserved_chars;
    output.rendered_chars = rendered_chars;
    output.snapshot_lookup_ms = snapshot_lookup_ms;
    output.deadline_precommit_elapsed_ms = deadline_precommit_elapsed_ms;
    output
}

/// Shared initial state uses current discovery measurements and no execution counts.
/// Normal and snapshot paths add only the work they actually performed.
pub(super) fn base_execution_metrics(
    worker_handshake_ms: u64,
    discovery_ms: u64,
    discovery_observed_file_count: u64,
    guards: SourceGuardObservationMetrics,
    peak_worker_rss_bytes: Option<u64>,
) -> ai_daily_scanner_contract::ExecutionMetricsV2 {
    ai_daily_scanner_contract::ExecutionMetricsV2 {
        discovery_observed_file_count,
        source_guard_content_hash_file_count: guards.content_hash_file_count,
        source_guard_unavailable_count: guards.unavailable_file_count,
        source_guard_bytes_read: guards.bytes_read,
        worker_handshake_ms,
        discovery_ms,
        peak_worker_rss_bytes: Nullable(peak_worker_rss_bytes),
        ..Default::default()
    }
}

/// Builds the `ArtifactDraft` for a scheduler outcome (spec Part 5.1).
/// `None` for Error runs; `Some` for Success/Partial. An eligible artifact
/// (Ok + warnings-empty + no Error/Timeout) carries the snapshot key plus
/// per-source-file rows; ineligible runs persist a payload artifact with no rows.
pub(super) fn build_batch_artifact(
    outcome: &BudgetedScanOutcome,
) -> Result<Option<ArtifactDraft>, String> {
    let Some(context) = &outcome.context else {
        return Ok(None);
    };
    let eligible = matches!(outcome.terminal_intent, TerminalIntent::Success)
        && !outcome
            .diagnostics
            .iter()
            .any(|record| record.severity == DiagnosticSeverity::Warning)
        && outcome.file_results.iter().all(|record| {
            !matches!(
                record.parse_status,
                ParseStatus::Error | ParseStatus::Timeout
            )
        });
    let semantic = SemanticSummary {
        source_file_count: context.summary.source_file_count,
        success_count: context.summary.success_count,
        timeout_count: context.summary.timeout_count,
        included_file_count: context.summary.included_file_count,
        omitted_file_count: context.summary.omitted_file_count,
        error_file_count: context.summary.error_file_count,
        input_chars: context.summary.input_chars,
        output_chars: context.summary.output_chars,
        reserved_chars: outcome.execution_metrics.reserved_chars,
        rendered_chars: outcome.execution_metrics.rendered_chars,
    };
    let (file_rows, decision_rows) = if eligible {
        (
            artifact_file_rows(outcome),
            artifact_decision_rows(&context.decisions),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    ArtifactDraft::new(
        eligible,
        context.final_context.clone(),
        semantic,
        file_rows,
        decision_rows,
    )
    .map(Some)
}

pub(super) fn artifact_file_rows(outcome: &BudgetedScanOutcome) -> Vec<ArtifactFileRow> {
    let result_by_identity: std::collections::HashMap<&str, &FileResultRecord> = outcome
        .file_results
        .iter()
        .map(|record| (record.file_identity.as_str(), record))
        .collect();
    outcome
        .inventory
        .iter()
        .map(|item| {
            let result = result_by_identity.get(item.file_identity.as_str());
            // spec Part 3.2: the artifact stores only the immutable classifier
            // provenance subset (status/page/result/nominal/build/profile-hash);
            // cache status / miss reason / run pages / duration / transport /
            // attempt are current-run execution fields and never reused.
            let classifier = result
                .and_then(|record| record.pdf_classification.as_ref())
                .map(|audit| PdfClassificationProvenanceV1 {
                    status: audit.status,
                    page_count: audit.page_count.0,
                    result_examined_pages: audit.result_examined_pages.0,
                    nominal_charged_pages: audit.nominal_charged_pages,
                    classifier_build: audit.classifier_build.clone(),
                    classifier_profile_hash: audit.classifier_profile_hash.clone(),
                });
            ArtifactFileRow {
                file_identity: item.file_identity.clone(),
                relative_path: item.relative_path.clone(),
                legacy_source_version: item.source_version.clone(),
                source_guard_kind: item.source_guard_kind.clone(),
                source_guard_sha256: item.source_guard_sha256.clone(),
                parse_profile_hash: result
                    .map(|record| record.parse_profile_hash.clone())
                    .unwrap_or_else(|| "0".repeat(64)),
                parse_status: result
                    .map(|record| record.parse_status)
                    .unwrap_or(ParseStatus::NotParsed),
                parser_backend: result
                    .map(|record| record.parser_backend.clone())
                    .unwrap_or_else(|| "not_parsed".to_string()),
                worker_lane: result
                    .map(|record| record.worker_lane.as_str().to_owned())
                    .unwrap_or_else(|| "not_parsed".to_string()),
                truncated: result.is_some_and(|record| record.truncated),
                content_sha256: result
                    .map(|record| record.content_sha256.clone())
                    .unwrap_or_else(|| crate::store::sha256_hex(b"")),
                classifier,
            }
        })
        .collect()
}

fn artifact_decision_rows(decisions: &[ContextDecisionRecord]) -> Vec<ArtifactDecisionRow> {
    decisions
        .iter()
        .map(|record| ArtifactDecisionRow {
            file_identity: record.file_identity.clone(),
            relative_path: record.decision.relative_path.clone(),
            action: record.decision.action,
            reason: record.decision.reason.clone(),
            priority: record.decision.priority,
            input_chars: record.decision.input_chars,
            output_chars: record.decision.output_chars,
            truncated: record.decision.truncated,
            error_code: record.decision.error_code.clone(),
        })
        .collect()
}

pub(super) fn snapshot_inventory(
    files: &[DiscoveredFileOut],
    work_dir: &str,
) -> Result<Vec<InventoryRecord>, String> {
    files
        .iter()
        .map(|file| {
            let relative_path =
                crate::context_audit::relative_contract_path(Path::new(work_dir), &file.path)?;
            InventoryRecord::from_discovered(file, relative_path)
        })
        .collect()
}

/// Current-run rows for a snapshot hit (spec Part 5.2): `parse_cache_status`
/// becomes `snapshot`, `cache_miss_reason=''`, durations/attempts 0 — never the
/// source run's miss/hit or old timings.
pub(super) fn snapshot_file_results(
    artifact: &ArtifactDraft,
    classifier_identity: Option<&ClassifierIdentity>,
) -> Vec<FileResultRecord> {
    let decision_reason_by_identity: std::collections::HashMap<&str, &str> = artifact
        .decision_rows
        .iter()
        .map(|row| (row.file_identity.as_str(), row.reason.as_str()))
        .collect();
    artifact
        .file_rows
        .iter()
        .map(|row| FileResultRecord {
            file_identity: row.file_identity.clone(),
            relative_path: row.relative_path.clone(),
            source_version: row.legacy_source_version.clone(),
            parse_profile_hash: row.parse_profile_hash.clone(),
            cache_status: CacheStatus::Fresh,
            cache_miss_reason: CacheMissReason::None,
            parse_status: row.parse_status,
            parser_backend: row.parser_backend.clone(),
            worker_lane: crate::scheduler::parse_worker_lane(&row.worker_lane),
            truncated: row.truncated,
            content_sha256: row.content_sha256.clone(),
            primary_duration_ms: 0,
            fallback_duration_ms: 0,
            parse_duration_ms: 0,
            failure_class: String::new(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport: ai_daily_scanner_contract::ParseTransport::Snapshot,
            parse_attempt_count: 0,
            pdf_classification: snapshot_classification_audit(
                row.classifier.as_ref(),
                decision_reason_by_identity
                    .get(row.file_identity.as_str())
                    .copied(),
                classifier_identity,
            ),
            error: None,
        })
        .collect()
}

fn snapshot_classification_audit(
    provenance: Option<&PdfClassificationProvenanceV1>,
    decision_reason: Option<&str>,
    classifier_identity: Option<&ClassifierIdentity>,
) -> Option<ai_daily_scanner_contract::PdfClassificationAuditV1> {
    if let Some(provenance) = provenance {
        return Some(provenance.clone().into_snapshot_audit());
    }
    // Older artifacts omitted classification for PDFs skipped by this precise quota.
    let identity = classifier_identity
        .filter(|_| decision_reason == Some("pdf_classification_page_quota_exhausted"))?;
    Some(
        PdfClassificationProvenanceV1 {
            status: ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget,
            page_count: None,
            result_examined_pages: Some(0),
            nominal_charged_pages: 0,
            classifier_build: identity.build.clone(),
            classifier_profile_hash: identity.profile_hash.clone(),
        }
        .into_snapshot_audit(),
    )
}
