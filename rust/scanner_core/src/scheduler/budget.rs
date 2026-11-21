use super::*;

impl BudgetedContextScheduler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        classifier: Box<dyn PdfClassifierPort>,
        parser: Box<dyn ParserPort>,
        cache: Box<dyn CachePort>,
        clock: Box<dyn Clock>,
        guard: Box<dyn GuardVerifier>,
    ) -> Self {
        Self {
            classifier,
            parser,
            cache,
            clock,
            guard,
            stored_work_deadline: 0,
            stored_workers: WorkerIdentities::default(),
            stored_engine_version: String::new(),
            stored_engine_build: String::new(),
        }
    }

    pub fn execute(
        mut self,
        input: ScheduledRunInput,
    ) -> Result<BudgetedScanOutcome, SchedulerFailure> {
        if self.clock.now_ms() >= input.absolute_deadline_ms {
            return Err(SchedulerFailure::internal(
                "absolute deadline already exhausted before execution",
            ));
        }
        self.stored_work_deadline = input.work_deadline_ms;
        self.stored_workers = input.workers.clone();
        self.stored_engine_version = input.engine_version.clone();
        self.stored_engine_build = input.engine_build.clone();
        // spec Part 2.1: engine-owned `MAX_SOURCE_FILES_PER_RUN` hard ceiling.
        // The 1,000,001st accepted source file fails closed as a non-retryable
        // run-level Error BEFORE `prepare_inventory`, with zero file rows and
        // `discovery_observed_file_count = ceiling + 1`.
        if input.discovery.len() as u64 > ai_daily_scanner_contract::MAX_SOURCE_FILES_PER_RUN {
            let mut metrics = source_guard_metrics(&input.discovery);
            apply_observed_source_guard_metrics(&mut metrics, self.guard.as_ref());
            metrics.discovery_observed_file_count =
                ai_daily_scanner_contract::MAX_SOURCE_FILES_PER_RUN + 1;
            metrics.deadline_precommit_elapsed_ms = self.clock.now_ms();
            return Ok(BudgetedScanOutcome {
                scan_run_id: input.scan_run_id,
                terminal_intent: TerminalIntent::Error,
                inventory: Vec::new(),
                file_results: Vec::new(),
                sources: Vec::new(),
                parse_cache_receipts: Vec::new(),
                classification_cache_receipts: Vec::new(),
                classifications: std::collections::BTreeMap::new(),
                diagnostics: vec![RunDiagnosticRecord {
                    severity: DiagnosticSeverity::Error,
                    diagnostic: Diagnostic {
                        error_code: ErrorCode::SourceFileLimitExceeded,
                        message: format!(
                            "discovery observed {} source files, exceeding the engine ceiling of {}",
                            input.discovery.len(),
                            ai_daily_scanner_contract::MAX_SOURCE_FILES_PER_RUN
                        ),
                        retryable: false,
                        stage: DiagnosticStage::Discovery,
                        file_path: Nullable(None),
                        backend: Nullable(None),
                    },
                }],
                stage_metrics: zero_stage_metrics(),
                extension_metrics: Vec::new(),
                context: None,
                execution_metrics: metrics,
            });
        }
        // spec Part 5.3: discovery issues surface as run-level warnings (they
        // mark the run Partial and never silently disappear).
        let mut diagnostics: Vec<RunDiagnosticRecord> = input
            .discovery_issues
            .iter()
            .map(discovery_issue_warning)
            .collect();
        let (inventory, existed_before, cache_duration_ms) = match (
            input.prepared_inventory,
            input.prepared_inventory_existed_before,
        ) {
            (Some(inventory), Some(existed_before)) => (
                inventory,
                existed_before,
                input.prepared_inventory_duration_ms,
            ),
            (None, None) => {
                let inventory = build_inventory(&input.discovery, &input.work_dir)?;
                let cache_started = self.clock.now_ms();
                let existed_before = self
                    .cache
                    .prepare_inventory(input.scan_run_id, input.started_at_ms, &inventory)
                    .map_err(|error| SchedulerFailure {
                        diagnostic: error.diagnostic(DiagnosticStage::Cache),
                    })?;
                let cache_duration_ms = self.clock.now_ms().saturating_sub(cache_started);
                (inventory, existed_before, cache_duration_ms)
            }
            _ => {
                return Err(SchedulerFailure::internal(
                    "prepared inventory receipt is incomplete",
                ))
            }
        };

        let discovery_count = input.discovery.len() as u64;
        let profile = input.profile.clone();
        let mut metrics = source_guard_metrics(&input.discovery);
        apply_observed_source_guard_metrics(&mut metrics, self.guard.as_ref());
        let executor = RollingExecutor::new(profile.execution.max_workers.max(1) as usize);

        // ---- Stage A: freeze ClassificationPlan before any classification I/O ----
        let candidates: Vec<PlanCandidate> = input.discovery.iter().map(plan_candidate).collect();
        let mut classified = ClassificationPlan::build(
            candidates,
            &profile,
            profile.max_total_pdf_classification_pages,
        );
        metrics.candidate_file_count = classified
            .iter()
            .filter(|plan| matches!(plan.action, PlanAction::Admit { .. }))
            .count() as u64;
        metrics.classification_slot_count = classified
            .iter()
            .filter(|plan| {
                matches!(
                    plan.pdf_classification,
                    crate::admission::PdfClassificationPlan::Classify { .. }
                )
            })
            .count() as u64;
        metrics.nominal_charged_pages_total = classified
            .iter()
            .filter_map(|plan| match plan.pdf_classification {
                crate::admission::PdfClassificationPlan::Classify { charged_pages } => {
                    Some(charged_pages)
                }
                _ => None,
            })
            .fold(0_u64, u64::saturating_add);

        // ---- Execute the selected PDF classifications ----
        let classifier_profile_hash = crate::store::classifier_profile_hash(&profile)
            .map_err(|message| SchedulerFailure::internal(&message))?;
        let needs_classifier = classified.iter().any(|plan| {
            !matches!(
                plan.pdf_classification,
                crate::admission::PdfClassificationPlan::NotPdf
            )
        });
        let classifier_build = match &input.workers.classifier_build {
            Some(build) => build.clone(),
            None if needs_classifier => {
                return Err(SchedulerFailure::new(
                    ErrorCode::WorkerHandshakeFailed,
                    "classifier build is required when PDFs are classified".to_string(),
                    false,
                    DiagnosticStage::Process,
                ))
            }
            None => "0".repeat(64),
        };
        let snapshot: HashMap<String, &DiscoveredFileOut> = input
            .discovery
            .iter()
            .map(|file| (file.file_identity.clone(), file))
            .collect();
        let (
            mut classifications,
            mut classification_audits,
            mut runtime_classification,
            mut classification_fresh_hits,
            classification_cache_receipts,
            classification_cache_write_warnings,
        ) = self.run_classifications(
            &classified,
            &snapshot,
            &profile,
            &classifier_profile_hash,
            &classifier_build,
            &existed_before,
            &mut metrics,
            &executor,
        )?;
        diagnostics.extend(
            classification_cache_write_warnings
                .into_iter()
                .map(|diagnostic| RunDiagnosticRecord {
                    severity: DiagnosticSeverity::Warning,
                    diagnostic,
                }),
        );

        // ---- Fixed context sections + budget model ----
        let mut fixed =
            fixed_context_sections(&profile.context, profile.report_mode, discovery_count);
        for plan in &classified {
            if let Some(line) =
                classifier_error_line(plan, &classifications, &runtime_classification)
            {
                fixed.push(line);
            }
        }
        let mut model = ContextBudgetModel::new(&profile.context, &fixed).map_err(|error| {
            SchedulerFailure::new(
                ErrorCode::ContextFixedSectionsOverBudget,
                error.to_string(),
                false,
                DiagnosticStage::Context,
            )
        })?;

        // ---- Freeze ContentAdmissionPlan BEFORE any parse I/O ----
        let filtered: Vec<ClassifiedPlan> = classified
            .iter()
            .filter(|plan| {
                !runtime_classification.contains(&plan.file_identity)
                    && plan.pdf_classification
                        != crate::admission::PdfClassificationPlan::NotClassifiedByBudget
            })
            .cloned()
            .collect();
        let mut admission =
            ContentAdmissionPlan::build(&filtered, &profile, &model, &classifications);
        metrics.admitted_file_count = admission
            .iter()
            .filter(|decision| matches!(decision.action, PlanAction::Admit { .. }))
            .count() as u64;
        metrics.extraction_slot_count = admission
            .iter()
            .filter(|decision| {
                matches!(
                    decision.action,
                    PlanAction::Admit {
                        route: RouteKind::Pdf
                    }
                ) && classifications
                    .get(&decision.file_identity)
                    .map(|r| r.status)
                    == Some(PdfClassificationStatus::TextInParseWindow)
            })
            .count() as u64;

        // ---- Execute admitted parses ----
        let parse_started = self.clock.now_ms();
        let mut parse_outputs = self.run_parses(
            &admission,
            &snapshot,
            &classifications,
            &profile,
            &existed_before,
            &runtime_classification,
            &mut metrics,
            &executor,
        )?;
        parse_outputs.classification_cache_receipts = classification_cache_receipts;
        if profile.admission_policy_version == "budget_refill_v3" {
            self.run_refills(
                &input.discovery,
                &input.work_dir,
                &profile,
                &snapshot,
                &existed_before,
                &mut classified,
                &mut admission,
                &mut classifications,
                &mut classification_audits,
                &mut runtime_classification,
                &mut classification_fresh_hits,
                &mut parse_outputs,
                &mut model,
                &classifier_profile_hash,
                &classifier_build,
                &mut metrics,
                &executor,
            )?;
        }
        if profile.parse.pdf.image_ocr.is_some() {
            let image_ocr = ContentAdmissionPlan::admit_image_ocr(
                &mut admission,
                &profile,
                &model,
                &classification_audits,
            );
            metrics.admitted_file_count = admission
                .iter()
                .filter(|decision| matches!(decision.action, PlanAction::Admit { .. }))
                .count() as u64;
            if !image_ocr.is_empty() {
                let outputs = self.run_parses(
                    &image_ocr,
                    &snapshot,
                    &classifications,
                    &profile,
                    &existed_before,
                    &runtime_classification,
                    &mut metrics,
                    &executor,
                )?;
                refill::merge_parses(&mut parse_outputs, outputs);
            }
        }
        for file in &input.discovery {
            if self.guard.is_tainted(&file.path) {
                parse_outputs
                    .results
                    .entry(file.file_identity.clone())
                    .or_insert_with(|| source_version_changed_parse(file));
                parse_outputs
                    .parse_cache_status
                    .insert(file.file_identity.clone(), CacheStatus::Miss);
                parse_outputs.parse_cache_miss_reason.insert(
                    file.file_identity.clone(),
                    CacheMissReason::SourceVersionChanged,
                );
            }
        }
        let parse_duration_ms = self.clock.now_ms().saturating_sub(parse_started);

        // Cache-access touches are optional metadata, but they still obey the
        // shared WorkDeadline and must never fail silently. A deadline becomes
        // the normal run-level deadline terminal state; other store failures are
        // projected as a bounded cache-write warning.
        if self.clock.now_ms() >= self.stored_work_deadline {
            metrics.stage_deadline_exhausted_count = 1;
        } else if !parse_outputs.parse_fresh_hits.is_empty()
            || !classification_fresh_hits.is_empty()
        {
            let touch = cache_timestamp_ms().and_then(|now_ms| {
                self.cache.touch_access(
                    now_ms,
                    &parse_outputs.parse_fresh_hits,
                    &classification_fresh_hits,
                )
            });
            match touch {
                Ok(()) => {}
                Err(CachePortError::DeadlineExhausted { .. }) => {
                    metrics.stage_deadline_exhausted_count = 1;
                }
                Err(error) => {
                    let diagnostic = error.diagnostic(DiagnosticStage::Cache);
                    parse_outputs.cache_write_warnings.push(Diagnostic {
                        error_code: ErrorCode::CacheWriteFailed,
                        message: format!("cache access touch skipped: {}", diagnostic.message),
                        retryable: true,
                        stage: DiagnosticStage::Cache,
                        file_path: Nullable(None),
                        backend: Nullable(None),
                    });
                }
            }
        }

        // ---- Build per-file evidence + decide + render ----
        let mut file_results = Vec::with_capacity(classified.len());
        let evidence = build_evidence(
            &snapshot,
            &input.work_dir,
            &classified,
            &admission,
            &classifications,
            &runtime_classification,
            &parse_outputs,
            |file, action, view| {
                let skipped = runtime_classification.contains(&file.file_identity)
                    || parse_outputs
                        .runtime_not_parsed
                        .contains(&file.file_identity);
                file_results.push(file_record(
                    file,
                    view,
                    matches!(action, PlanAction::Admit { .. }),
                    skipped,
                    &parse_outputs,
                    &classification_audits,
                    &input.rejected_profile_hash,
                ));
            },
        )?;
        let render_started = self.clock.now_ms();
        let mut sources = crate::sources::collect_sources(&evidence, &input.discovery, &profile);
        if metrics
            .refill_audit
            .as_ref()
            .is_some_and(|audit| audit.stop_reason == "round_limit")
        {
            for source in &mut sources {
                if source.parse_status == ParseStatus::NotParsed
                    && matches!(
                        source.reason.as_deref(),
                        Some(
                            "semantic_file_quota_exhausted"
                                | "pdf_classification_page_quota_exhausted"
                                | "pdf_text_extraction_quota_exhausted"
                                | "global_context_budget_exceeded"
                        )
                    )
                {
                    source.coverage_limits["evidence_audit"]["secondary_tags"] =
                        serde_json::json!(["REFILL_ROUND_LIMIT_REACHED"]);
                }
            }
        }
        let rendered = match build_context(evidence, &profile.context, profile.report_mode) {
            Ok(rendered) => rendered,
            Err(message) => {
                // spec Solution: BUDGET_MODEL_MISMATCH is a defined terminal
                // state -> Ok(outcome) with Error intent, never Err(SchedulerFailure).
                apply_observed_source_guard_metrics(&mut metrics, self.guard.as_ref());
                metrics.deadline_precommit_elapsed_ms = self.clock.now_ms();
                return Ok(internal_error_outcome(
                    input.scan_run_id,
                    metrics,
                    diagnostics,
                    parse_outputs,
                    ErrorCode::BudgetModelMismatch,
                    message,
                ));
            }
        };
        let compression_duration_ms = self.clock.now_ms().saturating_sub(render_started);

        // ---- Enforce the rendered <= reserved budget-model invariant ----
        if let Err(failure) = enforce_rendered_within_reserved(&rendered, &admission, &model) {
            apply_observed_source_guard_metrics(&mut metrics, self.guard.as_ref());
            metrics.deadline_precommit_elapsed_ms = self.clock.now_ms();
            return Ok(internal_error_outcome(
                input.scan_run_id,
                metrics,
                diagnostics,
                parse_outputs,
                failure.diagnostic.error_code,
                failure.diagnostic.message,
            ));
        }

        metrics.reserved_chars = model
            .base_chars()
            .saturating_add(admission.iter().map(|d| d.reserved_chars).sum::<u64>());
        metrics.rendered_chars = rendered.output_chars;

        // ---- Terminal file results + intent + bounded diagnostics ----
        let (mut terminal_intent, run_diagnostics) = terminal_state(
            &rendered,
            &file_results,
            &runtime_classification,
            &parse_outputs,
            metrics.stage_deadline_exhausted_count,
        );
        diagnostics.extend(
            parse_outputs
                .cache_write_warnings
                .iter()
                .cloned()
                .map(|diagnostic| RunDiagnosticRecord {
                    severity: DiagnosticSeverity::Warning,
                    diagnostic,
                }),
        );
        diagnostics.extend(
            parse_outputs
                .parse_warnings
                .iter()
                .cloned()
                .map(|diagnostic| RunDiagnosticRecord {
                    severity: DiagnosticSeverity::Warning,
                    diagnostic,
                }),
        );
        diagnostics.extend(run_diagnostics);
        // spec Part 2.2: 257-warning bounded projection (run-level first,
        // 256 detail + 1 DIAGNOSTICS_AGGREGATED).
        diagnostics = project_warnings(diagnostics);
        // Discovery issues / parser degradation / cache-write warnings are
        // run-level warnings: they mark the run Partial, never Success.
        if terminal_intent == TerminalIntent::Success
            && diagnostics
                .iter()
                .any(|record| record.severity == DiagnosticSeverity::Warning)
        {
            terminal_intent = TerminalIntent::Partial;
        }

        let stage_metrics = build_stage_metrics(
            discovery_count,
            rendered.decisions.len() as u64,
            discovery_count,
            metrics.parse_attempt_count,
            input.discovery_duration_ms,
            cache_duration_ms,
            parse_duration_ms,
            compression_duration_ms,
        );
        let extension_metrics = crate::context_audit::extension_metrics(&inventory, &file_results)
            .map_err(|message| SchedulerFailure::internal(&message))?;

        let total_duration_ms = input
            .discovery_duration_ms
            .saturating_add(cache_duration_ms)
            .saturating_add(parse_duration_ms)
            .saturating_add(compression_duration_ms);
        let context = match terminal_intent {
            // Error envelopes carry an EMPTY file_context; run.rs builds the
            // error envelope from the outcome diagnostics.
            TerminalIntent::Error => None,
            _ if rendered.content.is_empty() => None,
            _ => {
                let summary = ContextSummary {
                    source_file_count: rendered.source_file_count,
                    success_count: rendered.success_count,
                    timeout_count: rendered.timeout_count,
                    included_file_count: rendered.included_file_count,
                    omitted_file_count: rendered.omitted_file_count,
                    error_file_count: rendered.error_file_count,
                    input_chars: rendered.input_chars,
                    output_chars: rendered.output_chars,
                    total_duration_ms,
                    discovery_duration_ms: input.discovery_duration_ms,
                    parse_duration_ms,
                    compression_duration_ms,
                };
                let decisions = rendered
                    .decisions
                    .into_iter()
                    .map(|record| ContextDecisionRecord {
                        file_identity: record.file_identity,
                        decision: record.decision,
                    })
                    .collect();
                Some(ContextRunRecord {
                    context_profile_hash: input.context_profile_hash,
                    status: match terminal_intent {
                        TerminalIntent::Success => RunStatus::Success,
                        TerminalIntent::Partial => RunStatus::Partial,
                        TerminalIntent::Error => RunStatus::Error,
                    },
                    context_sha256: crate::store::sha256_hex(rendered.content.as_bytes()),
                    final_context: rendered.content,
                    summary,
                    decisions,
                })
            }
        };

        apply_observed_source_guard_metrics(&mut metrics, self.guard.as_ref());
        metrics.deadline_precommit_elapsed_ms = self.clock.now_ms();

        Ok(BudgetedScanOutcome {
            scan_run_id: input.scan_run_id,
            terminal_intent,
            inventory,
            file_results,
            sources,
            parse_cache_receipts: parse_outputs.parse_cache_receipts,
            classification_cache_receipts: parse_outputs.classification_cache_receipts,
            classifications,
            diagnostics,
            stage_metrics,
            extension_metrics,
            context,
            execution_metrics: metrics,
        })
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

pub(crate) fn source_guard_metrics(discovery: &[DiscoveredFileOut]) -> ExecutionMetrics {
    let mut metrics = ExecutionMetrics {
        discovery_observed_file_count: discovery.len() as u64,
        ..ExecutionMetrics::default()
    };
    for file in discovery {
        match source_guard_kind_from_text(file.source_guard_kind.as_deref().unwrap_or("")) {
            Some(SourceGuardKind::Unavailable) => metrics.source_guard_unavailable_count += 1,
            Some(SourceGuardKind::ContentSha256V1) => {
                metrics.source_guard_content_hash_file_count += 1;
                // Exact bytes require a run observer; file size is not an I/O
                // receipt and must never be substituted here.
            }
            _ => {}
        }
    }
    metrics
}

fn apply_observed_source_guard_metrics(metrics: &mut ExecutionMetrics, guard: &dyn GuardVerifier) {
    let Some(observed) = guard.metrics() else {
        return;
    };
    metrics.source_guard_content_hash_file_count = observed.content_hash_file_count;
    metrics.source_guard_unavailable_count = observed.unavailable_file_count;
    metrics.source_guard_bytes_read = observed.bytes_read;
}

/// Surfaces a discovery issue as a run-level warning (spec Part 5.3); an
/// unreadable discovery entry must mark the run Partial, never silently vanish.
fn discovery_issue_warning(issue: &DiscoveryIssue) -> RunDiagnosticRecord {
    let file_path = issue
        .path
        .as_ref()
        .filter(|path| Path::new(path).is_absolute());
    RunDiagnosticRecord {
        severity: DiagnosticSeverity::Warning,
        diagnostic: Diagnostic {
            error_code: ErrorCode::DiscoveryEntryUnreadable,
            message: issue.message.chars().take(4_096).collect(),
            retryable: true,
            stage: DiagnosticStage::Discovery,
            file_path: Nullable(file_path.cloned()),
            backend: Nullable(None),
        },
    }
}

fn build_inventory(
    discovery: &[DiscoveredFileOut],
    work_dir: &str,
) -> Result<Vec<InventoryRecord>, SchedulerFailure> {
    let mut inventory = Vec::with_capacity(discovery.len());
    for file in discovery {
        let relative_path =
            crate::context_audit::relative_contract_path(Path::new(work_dir), &file.path)
                .map_err(|message| SchedulerFailure::internal(&message))?;
        inventory.push(
            InventoryRecord::from_discovered(file, relative_path)
                .map_err(|message| SchedulerFailure::internal(&message))?,
        );
    }
    Ok(inventory)
}

pub(super) fn plan_candidate(file: &DiscoveredFileOut) -> PlanCandidate {
    let guard_kind = file
        .source_guard_kind
        .as_deref()
        .and_then(source_guard_kind_from_text)
        .unwrap_or(SourceGuardKind::Unavailable);
    PlanCandidate {
        file_identity: file.file_identity.clone(),
        relative_path: file.path.clone(),
        extension: file.extension.clone(),
        size_bytes: file.size_bytes,
        source_guard_kind: guard_kind,
    }
}

pub(super) fn expected_guard(file: &DiscoveredFileOut) -> Option<SourceGuardV2> {
    let kind = file
        .source_guard_kind
        .as_deref()
        .and_then(source_guard_kind_from_text)?;
    if kind == SourceGuardKind::Unavailable {
        return None;
    }
    let hash = file.source_guard_sha256.clone()?;
    let guard = SourceGuardV2 {
        kind,
        guard_sha256: Some(hash),
    };
    guard.validate().ok()?;
    Some(guard)
}
