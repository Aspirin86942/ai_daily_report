use super::*;

impl ActiveBuild<'_> {
    pub(super) fn commit_outcome(
        &mut self,
        batch: &FinalizationBatch,
        envelope: ContextEnvelope,
        warnings: Vec<Diagnostic>,
        discovery_ms: u64,
    ) -> ScannerOperation<ContextEnvelope> {
        // The attempted commit time precedes validation. A rejected batch gets
        // its own new timestamp when the minimal error is ready to commit.
        let now_ms = match run_timestamp() {
            Ok(now) => now,
            Err(error) => {
                let summary = batch
                    .context
                    .as_ref()
                    .map(|context| context.summary.clone())
                    .unwrap_or_default();
                return self.run.error_output(error, warnings, summary);
            }
        };
        if let Some(output) = persist_post_outcome_failure_if_invalid(
            self.run.request,
            self.run.version,
            self.run.store,
            self.run.active,
            batch,
            self.handshake_ms,
            discovery_ms,
            &self.rss_tracker,
            self.run.timing,
        ) {
            return output;
        }
        self.run.commit_at(batch, now_ms, envelope, warnings)
    }
}

impl RunAttempt<'_> {
    pub(super) fn error_output(
        &self,
        error: Diagnostic,
        warnings: Vec<Diagnostic>,
        summary: ContextSummary,
    ) -> ScannerOperation<ContextEnvelope> {
        build_error_output(
            self.request,
            self.version,
            error,
            warnings,
            summary,
            Some(self.active.scan_run_id()),
        )
    }

    fn commit_at(
        &mut self,
        batch: &FinalizationBatch,
        now_ms: u64,
        envelope: ContextEnvelope,
        mut warnings: Vec<Diagnostic>,
    ) -> ScannerOperation<ContextEnvelope> {
        let exit_code = i32::from(batch.status == RunStatus::Error);
        match self.store.finalize(
            self.active,
            batch,
            now_ms,
            self.timing.deadlines,
            &self.timing.clock,
        ) {
            Ok(timings) => {
                complete_committed_terminal(self.store, self.timing, timings, envelope, exit_code)
            }
            Err(error) => {
                if let Err(cleanup_error) = abandon_run(self.store, self.active) {
                    warnings.push(cleanup_error.diagnostic(DiagnosticStage::Cache));
                }
                self.error_output(
                    error.diagnostic(DiagnosticStage::Cache),
                    warnings,
                    ContextSummary::default(),
                )
            }
        }
    }

    pub(super) fn persist_error(
        &mut self,
        mut warnings: Vec<Diagnostic>,
        error: Diagnostic,
        discovery_observed_file_count: u64,
        discovery_ms: u64,
        worker_handshake_ms: u64,
        rss_tracker: &WorkerRssTracker,
    ) -> ScannerOperation<ContextEnvelope> {
        let summary = ContextSummary::default();
        let peak_worker_rss_bytes = rss_tracker.peak_worker_rss_bytes();
        if peak_worker_rss_bytes.is_none()
            && !warnings
                .iter()
                .any(|warning| warning.error_code == ErrorCode::WorkerRssUnavailable)
        {
            warnings.push(worker_rss_unavailable_diagnostic());
        }
        warnings.truncate(100_000);
        let envelope = ContextEnvelope {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id: self.request.request_id.clone(),
            engine_version: self.version.engine_version.clone(),
            engine_build: self.version.engine_build.clone(),
            status: EngineStatus::Error,
            file_context: String::new(),
            summary: summary.clone(),
            scan_run_id: Nullable(Some(self.active.scan_run_id())),
            context_run_id: Nullable(Some(self.active.context_run_id())),
            warnings: warnings.clone(),
            error: Nullable(Some(error.clone())),
        };
        let envelope_json = match canonical_envelope_json(&envelope) {
            Ok(json) => json,
            Err(error) => {
                return self.error_output(
                    error.diagnostic(DiagnosticStage::Internal),
                    warnings,
                    summary,
                )
            }
        };
        let mut diagnostics: Vec<_> = warnings.iter().cloned().map(warning_record).collect();
        diagnostics.push(RunDiagnosticRecord {
            severity: DiagnosticSeverity::Error,
            diagnostic: error,
        });
        let batch = FinalizationBatch {
            status: RunStatus::Error,
            envelope_json,
            inventory: Vec::new(),
            file_results: Vec::new(),
            diagnostics,
            stage_metrics: crate::scheduler::zero_stage_metrics(),
            extension_metrics: Vec::new(),
            context: Some(ContextRunRecord {
                context_profile_hash: crate::store::sha256_hex(b"terminal_failure_v1"),
                status: RunStatus::Error,
                final_context: String::new(),
                context_sha256: crate::store::sha256_hex(b""),
                summary: summary.clone(),
                decisions: Vec::new(),
            }),
            artifact: None,
            snapshot_key: None,
            snapshot_hit: None,
            // spec Part 5.3: engine-error runs persist an authoritative metrics row
            // with the wall spans that genuinely ran (handshake + discovery), never
            // a fabricated-zero derive.
            execution_metrics: Some(base_execution_metrics(
                worker_handshake_ms,
                discovery_ms,
                discovery_observed_file_count,
                self.timing.source_guards.metrics(),
                peak_worker_rss_bytes,
            )),
        };
        let now_ms = match run_timestamp() {
            Ok(now) => now,
            Err(error) => return self.error_output(error, warnings, summary),
        };
        self.commit_at(&batch, now_ms, envelope, warnings)
    }
}

pub(super) fn warning_record(diagnostic: Diagnostic) -> RunDiagnosticRecord {
    RunDiagnosticRecord {
        severity: DiagnosticSeverity::Warning,
        diagnostic,
    }
}

pub(super) fn complete_committed_terminal(
    store: &mut ScannerStore,
    timing: &ActiveRunTiming,
    timings: TerminalAuditTimings,
    envelope: ContextEnvelope,
    exit_code: i32,
) -> ScannerOperation<ContextEnvelope> {
    if timings.busy_timeout_restore_failed {
        eprintln!("scanner warning: SQLite busy timeout restore failed after terminal commit");
        return ScannerOperation {
            value: envelope,
            exit_code,
        };
    }

    // Optional GC is admitted only after the authoritative terminal COMMIT.
    // Busy, deadline overshoot, time-read, or restore failures remain redacted
    // runtime warnings and can never rewrite that committed result.
    if timing.remaining_absolute_ms() >= crate::store::cache::OPPORTUNISTIC_GC_BUDGET_MS {
        match current_time_millis() {
            Ok(now_ms) => {
                if store
                    .run_opportunistic_gc(
                        now_ms,
                        crate::store::cache::OPPORTUNISTIC_GC_BUDGET_MS,
                        &timing.clock,
                    )
                    .is_err()
                {
                    eprintln!("scanner warning: opportunistic SQLite GC was skipped");
                }
            }
            Err(_) => {
                eprintln!("scanner warning: opportunistic SQLite GC was skipped");
            }
        }
    }
    ScannerOperation {
        value: envelope,
        exit_code,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn persist_post_outcome_failure_if_invalid(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    store: &mut ScannerStore,
    active: &ActiveRun,
    batch: &FinalizationBatch,
    worker_handshake_ms: u64,
    discovery_ms: u64,
    rss_tracker: &WorkerRssTracker,
    timing: &ActiveRunTiming,
) -> Option<ScannerOperation<ContextEnvelope>> {
    let error = store.validate_finalization_batch(active, batch).err()?;
    let observed_count = batch
        .context
        .as_ref()
        .map_or(0, |context| context.summary.source_file_count);
    let mut run = RunAttempt {
        request,
        version,
        store,
        active,
        timing,
    };
    Some(run.persist_error(
        Vec::new(),
        error.diagnostic(DiagnosticStage::Cache),
        observed_count,
        discovery_ms,
        worker_handshake_ms,
        rss_tracker,
    ))
}

pub(super) fn build_error_output(
    request: &BuildContextRequest,
    version: &EngineIdentity,
    error: Diagnostic,
    mut warnings: Vec<Diagnostic>,
    summary: ContextSummary,
    scan_run_id: Option<u64>,
) -> ScannerOperation<ContextEnvelope> {
    warnings.truncate(100_000);
    let response = ContextEnvelope {
        contract: "ai_daily_context".to_string(),
        protocol_version: 1,
        request_id: request.request_id.clone(),
        engine_version: version.engine_version.clone(),
        engine_build: version.engine_build.clone(),
        status: EngineStatus::Error,
        file_context: String::new(),
        summary,
        scan_run_id: Nullable(scan_run_id),
        context_run_id: Nullable(None),
        warnings,
        error: Nullable(Some(error)),
    };
    ScannerOperation {
        value: response,
        exit_code: 1,
    }
}
