use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn build_evidence(
    snapshot: &HashMap<String, &DiscoveredFileOut>,
    work_dir: &str,
    classified: &[ClassifiedPlan],
    admission: &[AdmissionDecision],
    classifications: &BTreeMap<String, crate::admission::PdfClassificationResult>,
    runtime_classification: &HashSet<String>,
    parse_outputs: &ParseOutputs,
    mut record: impl FnMut(&DiscoveredFileOut, &PlanAction, &ContextFileEvidence),
) -> Result<Vec<ContextFileEvidence>, SchedulerFailure> {
    let mut evidence = Vec::with_capacity(classified.len());
    for plan in classified {
        let file = snapshot
            .get(&plan.file_identity)
            .ok_or_else(|| SchedulerFailure::internal("classified plan has no discovery file"))?;
        let relative =
            crate::context_audit::relative_contract_path(Path::new(work_dir), &file.path)
                .map_err(|message| SchedulerFailure::internal(&message))?;
        let action = admission
            .iter()
            .find(|decision| decision.file_identity == plan.file_identity)
            .map_or(&plan.action, |decision| &decision.action);
        let view = file_evidence(
            file,
            &relative,
            action,
            classifications,
            runtime_classification,
            parse_outputs,
        );
        record(file, action, &view);
        evidence.push(view);
    }
    Ok(evidence)
}

fn file_evidence(
    file: &DiscoveredFileOut,
    relative_path: &str,
    action: &PlanAction,
    classifications: &BTreeMap<String, crate::admission::PdfClassificationResult>,
    runtime_classification: &HashSet<String>,
    parse_outputs: &ParseOutputs,
) -> ContextFileEvidence {
    let evidence = |parse_status, backend: &str, worker_lane| ContextFileEvidence {
        file_identity: file.file_identity.clone(),
        absolute_path: file.path.clone(),
        relative_path: relative_path.to_owned(),
        extension: file.extension.clone(),
        size_bytes: Some(file.size_bytes),
        parse_status,
        parser_backend: backend.to_owned(),
        worker_lane,
        cache_status: CacheStatus::Miss,
        content: String::new(),
        evidence_audit: None,
        truncated: false,
        error: None,
        reason: None,
    };
    if let Some(result) = parse_outputs
        .results
        .get(&file.file_identity)
        .filter(|result| is_source_version_changed_result(result))
    {
        return ContextFileEvidence {
            error: result.error.clone(),
            reason: Some("source_version_changed".to_string()),
            ..evidence(
                ParseStatus::Error,
                &result.parser_backend,
                parse_worker_lane(&result.worker_lane),
            )
        };
    }
    match action {
        PlanAction::NotParsed { reason } => ContextFileEvidence {
            reason: Some(reason.as_str().to_string()),
            ..evidence(
                ParseStatus::NotParsed,
                "not_parsed",
                AuditWorkerLane::NotParsed,
            )
        },
        PlanAction::Reject { reason } => {
            let (code, retryable) = match reason {
                RejectReason::SourceGuardUnavailable => (ErrorCode::SourceGuardUnavailable, true),
                RejectReason::ProfileRouteInvariant => (ErrorCode::ProfileRouteInvariant, false),
            };
            ContextFileEvidence {
                error: Some(Diagnostic {
                    error_code: code,
                    message: reason.as_str().to_string(),
                    retryable,
                    stage: DiagnosticStage::Parse,
                    file_path: Nullable(Some(file.path.clone())),
                    backend: Nullable(None),
                }),
                reason: Some(reason.as_str().to_string()),
                ..evidence(ParseStatus::Error, "not_parsed", AuditWorkerLane::NotParsed)
            }
        }
        PlanAction::ClassifierFailed { status } => {
            // spec Part 3.2: a classifier per-file TIMEOUT maps to unknown ->
            // Timeout; crash / transient I/O / protocol failure maps to unknown ->
            // Error with retryable=true. The classification error_code carries
            // the distinguishing PARSER_TIMEOUT / PARSER_FAILED marker.
            let (parse_status, code, retryable) =
                classifier_failure_semantics(status, classifications.get(&file.file_identity));
            ContextFileEvidence {
                error: Some(Diagnostic {
                    error_code: code,
                    message: format!("pdf classification {status:?}"),
                    retryable,
                    stage: DiagnosticStage::Parse,
                    file_path: Nullable(Some(file.path.clone())),
                    backend: Nullable(Some("pdf_classifier".to_string())),
                }),
                reason: Some("parse_error".to_string()),
                ..evidence(parse_status, "not_parsed", AuditWorkerLane::NotParsed)
            }
        }
        PlanAction::Admit { route } => {
            if runtime_classification.contains(&file.file_identity)
                || parse_outputs
                    .runtime_not_parsed
                    .contains(&file.file_identity)
            {
                // spec Part 2.3: runtime NotParsed (deadline) — NEVER snapshot.
                ContextFileEvidence {
                    reason: Some("runtime_deadline_exhausted".to_string()),
                    ..evidence(
                        ParseStatus::NotParsed,
                        "not_parsed",
                        AuditWorkerLane::NotParsed,
                    )
                }
            } else if let Some(result) = parse_outputs.results.get(&file.file_identity) {
                ContextFileEvidence {
                    evidence_audit: result.evidence_audit.clone(),
                    content: result.content.clone(),
                    truncated: result.truncated,
                    error: result.error.clone(),
                    reason: match (result.parse_status, route) {
                        (ParseStatus::Success, _) => None,
                        (ParseStatus::Timeout, RouteKind::PdfImageOcr { .. }) => {
                            Some("pdf_image_ocr_timeout".into())
                        }
                        (_, RouteKind::PdfImageOcr { .. }) => Some("pdf_image_ocr_failed".into()),
                        _ => Some("parse_error".into()),
                    },
                    ..evidence(
                        result.parse_status,
                        &result.parser_backend,
                        parse_worker_lane(&result.worker_lane),
                    )
                }
            } else {
                // no-text PDF metadata-only draft (no parser started).
                let status = classifications
                    .get(&file.file_identity)
                    .map(|r| r.status)
                    .unwrap_or(PdfClassificationStatus::NoTextInParseWindow);
                if status == PdfClassificationStatus::NoTextInParseWindow {
                    ContextFileEvidence {
                        reason: Some("pdf_no_text_in_parse_window".to_string()),
                        ..evidence(
                            ParseStatus::Success,
                            "pdf_metadata_v2",
                            AuditWorkerLane::RustCore,
                        )
                    }
                } else {
                    ContextFileEvidence {
                        error: Some(Diagnostic {
                            error_code: ErrorCode::InternalError,
                            message: "admitted text pdf has no parse result".to_string(),
                            retryable: false,
                            stage: DiagnosticStage::Parse,
                            file_path: Nullable(Some(file.path.clone())),
                            backend: Nullable(None),
                        }),
                        reason: Some("parse_error".to_string()),
                        ..evidence(ParseStatus::Error, "not_parsed", AuditWorkerLane::NotParsed)
                    }
                }
            }
        }
    }
}

pub(super) fn enforce_rendered_within_reserved(
    rendered: &ContextBuildOutput,
    admission: &[AdmissionDecision],
    model: &ContextBudgetModel,
) -> Result<(), SchedulerFailure> {
    for decision in admission {
        if let PlanAction::Admit { .. } = decision.action {
            if let Some(rendered_chars) = rendered.rendered_by_identity.get(&decision.file_identity)
            {
                model
                    .check_rendered_within_reserved(*rendered_chars, decision.reserved_chars)
                    .map_err(|_| {
                        SchedulerFailure::new(
                            ErrorCode::BudgetModelMismatch,
                            format!(
                                "rendered section for {} exceeded its reserved budget",
                                decision.relative_path
                            ),
                            false,
                            DiagnosticStage::Context,
                        )
                    })?;
            }
        }
    }
    Ok(())
}

pub(super) fn terminal_state(
    rendered: &ContextBuildOutput,
    file_results: &[FileResultRecord],
    runtime_classification: &HashSet<String>,
    parse_outputs: &ParseOutputs,
    stage_deadline_exhausted_count: u64,
) -> (TerminalIntent, Vec<RunDiagnosticRecord>) {
    let has_failure = rendered.error_file_count > 0
        || rendered.timeout_count > 0
        || !runtime_classification.is_empty()
        || !parse_outputs.runtime_not_parsed.is_empty()
        || stage_deadline_exhausted_count > 0;
    let mut diagnostics = Vec::new();
    if stage_deadline_exhausted_count > 0 {
        diagnostics.push(RunDiagnosticRecord {
            severity: DiagnosticSeverity::Warning,
            diagnostic: Diagnostic {
                error_code: ErrorCode::StageDeadlineExhausted,
                message: "work deadline exhausted; remaining work stopped".to_string(),
                retryable: true,
                stage: DiagnosticStage::Parse,
                file_path: Nullable(None),
                backend: Nullable(None),
            },
        });
    }
    // Per-file Error/Timeout become warnings (spec Part 2.3). The REAL final
    // Diagnostic is preserved verbatim (error_code/retryable/file_path/backend);
    // the run shell must not re-synthesize or collapse it.
    let mut primary_error: Option<Diagnostic> = None;
    if has_failure {
        for result in file_results {
            if let Some(error) = &result.error {
                if primary_error.is_none() {
                    primary_error = Some(error.clone());
                }
                diagnostics.push(RunDiagnosticRecord {
                    severity: DiagnosticSeverity::Warning,
                    diagnostic: error.clone(),
                });
            }
        }
    }
    let intent = if !has_failure {
        TerminalIntent::Success
    } else if rendered.included_file_count == 0 {
        // Cannot construct a valid non-empty context with included files.
        TerminalIntent::Error
    } else {
        TerminalIntent::Partial
    };
    if intent == TerminalIntent::Error {
        // spec Part 2.3: an Error envelope MUST carry an error Diagnostic. A
        // deadline-driven empty context uses the run-level deadline diagnostic
        // as the envelope error.
        if let Some(error) = primary_error {
            diagnostics.push(RunDiagnosticRecord {
                severity: DiagnosticSeverity::Error,
                diagnostic: error,
            });
        } else if stage_deadline_exhausted_count > 0 {
            diagnostics.push(RunDiagnosticRecord {
                severity: DiagnosticSeverity::Error,
                diagnostic: Diagnostic {
                    error_code: ErrorCode::StageDeadlineExhausted,
                    message: "work deadline exhausted before any included context".to_string(),
                    retryable: true,
                    stage: DiagnosticStage::Parse,
                    file_path: Nullable(None),
                    backend: Nullable(None),
                },
            });
        }
    }
    (intent, diagnostics)
}

/// Defines a minimal Error outcome for defined internal terminal states
/// (`BUDGET_MODEL_MISMATCH`, `CONTEXT_FIXED_SECTIONS_OVER_BUDGET`,
/// `enforce_rendered_within_reserved`) — returned as `Ok`, never `Err`
/// (spec Solution: defined business terminal states are `Ok(outcome)`).
#[allow(clippy::too_many_arguments)]
pub(super) fn internal_error_outcome(
    scan_run_id: u64,
    metrics: ExecutionMetrics,
    diagnostics: Vec<RunDiagnosticRecord>,
    parse_outputs: ParseOutputs,
    error_code: ErrorCode,
    message: String,
) -> BudgetedScanOutcome {
    let mut diagnostics = project_warnings(diagnostics);
    diagnostics.push(RunDiagnosticRecord {
        severity: DiagnosticSeverity::Error,
        diagnostic: Diagnostic {
            error_code,
            message: message.chars().take(4_096).collect(),
            retryable: false,
            stage: DiagnosticStage::Context,
            file_path: Nullable(None),
            backend: Nullable(None),
        },
    });
    BudgetedScanOutcome {
        scan_run_id,
        terminal_intent: TerminalIntent::Error,
        inventory: Vec::new(),
        file_results: Vec::new(),
        sources: Vec::new(),
        parse_cache_receipts: parse_outputs.parse_cache_receipts,
        classification_cache_receipts: parse_outputs.classification_cache_receipts,
        classifications: std::collections::BTreeMap::new(),
        diagnostics,
        stage_metrics: zero_stage_metrics(),
        extension_metrics: Vec::new(),
        context: None,
        execution_metrics: metrics,
    }
}

pub(super) fn file_record(
    file: &DiscoveredFileOut,
    view: &ContextFileEvidence,
    admitted: bool,
    skipped: bool,
    outputs: &ParseOutputs,
    classification_audits: &BTreeMap<String, PdfClassificationAuditV1>,
    rejected_profile_hash: &str,
) -> FileResultRecord {
    let id = &file.file_identity;
    let observed = outputs.results.get(id);
    let changed = observed.is_some_and(is_source_version_changed_result);
    let selected = observed.filter(|_| changed || (admitted && !skipped));
    // A missing admitted parse historically persisted a metadata row. The
    // evidence view independently flags an inconsistent text classification.
    let metadata = admitted && !skipped && observed.is_none();
    let profile = if changed || admitted {
        outputs.parse_profile_hashes.get(id)
    } else {
        None
    };
    let (cache_status, cache_miss_reason) = if changed {
        (CacheStatus::Miss, CacheMissReason::SourceVersionChanged)
    } else if admitted {
        (
            outputs
                .parse_cache_status
                .get(id)
                .copied()
                .unwrap_or(CacheStatus::Miss),
            outputs
                .parse_cache_miss_reason
                .get(id)
                .copied()
                .unwrap_or(CacheMissReason::NewFile),
        )
    } else {
        (CacheStatus::Miss, CacheMissReason::NewFile)
    };
    FileResultRecord {
        file_identity: id.clone(),
        relative_path: view.relative_path.clone(),
        source_version: file.source_version.clone(),
        parse_profile_hash: profile
            .map(String::as_str)
            .unwrap_or(rejected_profile_hash)
            .to_owned(),
        cache_status,
        cache_miss_reason,
        parse_status: if metadata {
            ParseStatus::Success
        } else {
            view.parse_status
        },
        parser_backend: if metadata {
            "pdf_metadata_v2".into()
        } else {
            view.parser_backend.clone()
        },
        worker_lane: if metadata {
            AuditWorkerLane::RustCore
        } else {
            view.worker_lane
        },
        truncated: view.truncated,
        error: if metadata { None } else { view.error.clone() },
        content_sha256: selected
            .map(|r| r.content_sha256.clone())
            .unwrap_or_else(|| crate::store::sha256_hex(b"")),
        primary_duration_ms: selected.map_or(0, |r| r.primary_duration_ms),
        fallback_duration_ms: selected.map_or(0, |r| r.fallback_duration_ms),
        parse_duration_ms: selected.map_or(0, |r| r.parse_duration_ms),
        failure_class: selected
            .map(|r| r.failure_class.clone())
            .unwrap_or_default(),
        fallback_backend: selected
            .map(|r| r.fallback_backend.clone())
            .unwrap_or_default(),
        fallback_reason_code: selected
            .map(|r| r.fallback_reason_code.clone())
            .unwrap_or_default(),
        parse_transport: observed.map_or(ParseTransport::NotApplicable, |r| r.parse_transport),
        parse_attempt_count: observed.map_or(0, |r| r.parse_attempt_count),
        pdf_classification: classification_audits.get(id).cloned(),
    }
}

// Keep terminal stage counters explicit for direct audit-field reconciliation.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_stage_metrics(
    source_file_count: u64,
    context_item_count: u64,
    cache_item_count: u64,
    parse_item_count: u64,
    discovery_duration_ms: u64,
    cache_duration_ms: u64,
    parse_duration_ms: u64,
    context_duration_ms: u64,
) -> Vec<StageMetric> {
    vec![
        StageMetric {
            stage: StageName::Discovery,
            item_count: source_file_count,
            duration_ms: discovery_duration_ms,
        },
        StageMetric {
            stage: StageName::Cache,
            item_count: cache_item_count,
            duration_ms: cache_duration_ms,
        },
        StageMetric {
            stage: StageName::Parse,
            item_count: parse_item_count,
            duration_ms: parse_duration_ms,
        },
        StageMetric {
            stage: StageName::Context,
            item_count: context_item_count,
            duration_ms: context_duration_ms,
        },
    ]
}

/// Spec Part 2.3: zero-file scheduler Error outcomes (source-file ceiling,
/// BUDGET_MODEL_MISMATCH, enforced-render mismatch) still commit a `context_runs`
/// row whose relational summary must reconcile with exactly 4 stage rows, so the
/// terminal batch carries four all-zero stage metrics instead of none.
pub(crate) fn zero_stage_metrics() -> Vec<StageMetric> {
    vec![
        StageMetric {
            stage: StageName::Discovery,
            item_count: 0,
            duration_ms: 0,
        },
        StageMetric {
            stage: StageName::Cache,
            item_count: 0,
            duration_ms: 0,
        },
        StageMetric {
            stage: StageName::Parse,
            item_count: 0,
            duration_ms: 0,
        },
        StageMetric {
            stage: StageName::Context,
            item_count: 0,
            duration_ms: 0,
        },
    ]
}
