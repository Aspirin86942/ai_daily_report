use super::*;

impl ActiveBuild<'_> {
    fn fail(
        &mut self,
        warnings: Vec<Diagnostic>,
        error: Diagnostic,
    ) -> ScannerOperation<ContextEnvelope> {
        self.fail_with_summary(warnings, error, elapsed_summary(self.started_at))
    }

    fn fail_with_summary(
        &mut self,
        mut warnings: Vec<Diagnostic>,
        error: Diagnostic,
        summary: ContextSummary,
    ) -> ScannerOperation<ContextEnvelope> {
        self.heartbeat.stop_with_warning(&mut warnings);
        self.run.persist_error(
            warnings,
            error,
            summary.source_file_count,
            summary.discovery_duration_ms,
            self.handshake_ms,
            &self.rss_tracker,
        )
    }

    pub(super) fn execute(
        &mut self,
        profile: &ai_daily_scanner_contract::NormalizedScannerSettings,
        work_dir: &Path,
        worker_pools: &mut ScannerWorkerPools,
        sources: &mut Vec<crate::sources::ScanSource>,
    ) -> ScannerOperation<ContextEnvelope> {
        let request = self.run.request;
        let version = self.run.version;
        let timing = self.run.timing;
        let active = self.run.active;
        let started_at = self.started_at;

        // A Scanner owns the verified worker identities with its long-lived pools.
        // The first run preflights within the worker limit; later runs reuse those
        // identities and each session still validates its own hello on startup.
        let office_command = office::worker_command(&request.adapters);
        let pdf_command = document::worker_command(&request.adapters);
        let (office_result, pdf_result) = if let Some((office, pdf)) =
            worker_pools.registered_workers(&office_command, &pdf_command)
        {
            (Ok(office), Ok(pdf))
        } else {
            let remaining_handshake_ms = timing.remaining_work_ms();
            if remaining_handshake_ms == 0 {
                return self.fail(Vec::new(), stage_deadline_exhausted("worker handshake"));
            }
            let handshake_timeout =
                WORKER_HANDSHAKE_TIMEOUT.min(Duration::from_millis(remaining_handshake_ms.max(1)));
            let handshake_started = Instant::now();
            let pair = register_worker_pair_observed(
                &office_command,
                &pdf_command,
                handshake_timeout,
                &self.rss_tracker,
                profile.execution.max_workers,
            );
            self.handshake_ms = elapsed_ms(handshake_started);
            pair
        };
        let (office_worker, office_error) = split_handshake(office_result, "rust_office_oxide_v2");
        let (pdf_worker, pdf_error) = split_handshake(pdf_result, "rust_pdfium_v3");
        let office_fingerprint = office_worker.as_ref().map(worker_fingerprint);
        let pdf_fingerprint = pdf_worker.as_ref().map(worker_fingerprint);
        let fingerprints = current_time_millis()
            .map_err(|error| (error, DiagnosticStage::Internal))
            .and_then(|now_ms| {
                self.run
                    .store
                    .record_worker_fingerprints(
                        active,
                        office_fingerprint.as_ref(),
                        pdf_fingerprint.as_ref(),
                        now_ms,
                    )
                    .map_err(|error| (error, DiagnosticStage::Cache))
            });
        if let Err((error, stage)) = fingerprints {
            self.heartbeat.stop();
            return self.run.error_output(
                error.diagnostic(stage),
                Vec::new(),
                elapsed_summary(started_at),
            );
        }
        let mut handshake_errors: Vec<Diagnostic> =
            [office_error, pdf_error].into_iter().flatten().collect();
        if !handshake_errors.is_empty() {
            let error = handshake_errors.remove(0);
            return self.fail(handshake_errors, error);
        }
        let (Some(office_worker), Some(pdf_worker)) = (office_worker, pdf_worker) else {
            return self.fail(
                Vec::new(),
                diagnostic(
                    ErrorCode::InternalError,
                    "worker preflight completed without both identities".to_string(),
                    false,
                    DiagnosticStage::Process,
                ),
            );
        };
        let route_stacks = match route_stack_fingerprints(&office_worker, &pdf_worker) {
            Ok(value) => value,
            Err(message) => {
                return self.fail(Vec::new(), internal_error(message, DiagnosticStage::Cache))
            }
        };

        // ---- discovery (with engine-owned SourceGuardV2) ----
        let remaining_discovery_ms = timing.remaining_work_ms();
        if remaining_discovery_ms == 0 {
            return self.fail(Vec::new(), stage_deadline_exhausted("discovery"));
        }
        let discovery_started = Instant::now();
        let discovery_timeout = Duration::from_millis(
            profile
                .execution
                .discovery_timeout_ms
                .min(remaining_discovery_ms)
                .max(1),
        );
        let mut discovery =
            match discover_with_timeout(work_dir, request, profile, discovery_timeout) {
                Ok(report) => report,
                Err(error) => {
                    let mut summary = elapsed_summary(started_at);
                    summary.discovery_duration_ms = elapsed_ms(discovery_started);
                    return self.fail_with_summary(Vec::new(), error, summary);
                }
            };
        let mut warnings: Vec<Diagnostic> = discovery
            .issues
            .iter()
            .map(discovery_issue_diagnostic)
            .collect();
        attach_source_guards(&mut discovery.files, &timing.source_guards);
        let discovery_duration_ms = elapsed_ms(discovery_started);

        // ---- assemble + execute the deep-module scheduler ----
        let params = crate::session::SessionParams::from_settings(profile);
        let (office_pool, pdf_pool) = worker_pools.resolve(
            office_command,
            office_worker.clone(),
            pdf_command.clone(),
            pdf_worker.clone(),
            params,
        );
        let classifier_port = ClassifierPort::new(pdf_pool.clone());
        let parser_port = ProductionParser::new(profile, pdf_pool.clone(), office_pool.clone());
        let cache_port = StoreCachePort::new(
            PathBuf::from(&request.scan_db_path),
            route_stacks,
            profile.clone(),
            timing.deadlines,
            timing.clock.clone(),
        );
        let context_profile_hash = match context_profile_hash(1, &version.engine_build, profile) {
            Ok(value) => value,
            Err(message) => {
                return self.fail(warnings, internal_error(message, DiagnosticStage::Context))
            }
        };
        let rejected_profile_hash = match rejected_profile_hash(1, &version.engine_build, profile) {
            Ok(value) => value,
            Err(message) => {
                return self.fail(warnings, internal_error(message, DiagnosticStage::Cache))
            }
        };
        let started_at_ms = match run_timestamp() {
            Ok(now) => now,
            Err(error) => return self.fail(warnings, error),
        };
        let worker_identities = WorkerIdentities {
            office_contract: Some(office_worker.identity.worker_contract_version.clone()),
            office_version: Some(office_worker.identity.worker_version.clone()),
            office_build: Some(office_worker.identity.worker_build.clone()),
            pdf_contract: Some(pdf_worker.identity.worker_contract_version.clone()),
            pdf_version: Some(pdf_worker.identity.worker_version.clone()),
            pdf_build: Some(pdf_worker.identity.worker_build.clone()),
            // Classification records retain a 64-character digest of the complete
            // native worker identity, whose build includes the pinned PDFium input.
            classifier_build: Some(crate::store::sha256_hex(
                &serde_json::to_vec(&pdf_worker.identity).expect("validated worker identity"),
            )),
        };

        // Persistence boundary 2: the complete discovery inventory is committed in
        // fixed short transactions before either snapshot or parse/classification
        // cache lookup. Only the completed receipt opens those lookup paths.
        let inventory = match snapshot_inventory(&discovery.files, &request.work_dir) {
            Ok(records) => records,
            Err(message) => {
                return self.fail(warnings, internal_error(message, DiagnosticStage::Cache))
            }
        };
        let inventory_prepare_started = timing.clock.now_ms();
        let inventory_now_ms = match run_timestamp() {
            Ok(now) => now,
            Err(error) => return self.fail(warnings, error),
        };
        let scan_run_id = match i64::try_from(active.scan_run_id()) {
            Ok(value) => value,
            Err(_) => {
                return self.fail(
                    warnings,
                    diagnostic(
                        ErrorCode::InvalidRequest,
                        "scan_run_id exceeds SQLite integer range".to_string(),
                        false,
                        DiagnosticStage::Cache,
                    ),
                );
            }
        };
        let inventory_existed_before = match self.run.store.prepare_inventory_with_deadline(
            &inventory,
            scan_run_id,
            inventory_now_ms,
            timing.deadlines,
            &timing.clock,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                return self.fail(warnings, error.diagnostic(DiagnosticStage::Cache));
            }
        };
        let inventory_prepare_duration_ms = timing
            .clock
            .now_ms()
            .saturating_sub(inventory_prepare_started);

        // ---- snapshot fast path (spec Part 5.4) ----
        // The live worker handshake is kept; a snapshot skips classification/parse
        // lookup + execution, context recompute, and new artifact payload writes.
        // A hit requires a byte-exact `snapshot_key_json` AND at least one committed
        // Success source run referencing the artifact (orphan artifacts never hit).
        let snapshot_lookup_started = Instant::now();
        let classifier_identity = ClassifierIdentity {
            contract: CLASSIFIER_CONTRACT_VERSION.to_string(),
            build: worker_identities
                .classifier_build
                .clone()
                .unwrap_or_else(|| "0".repeat(64)),
            profile_hash: match crate::store::classifier_profile_hash(profile) {
                Ok(hash) => hash,
                Err(message) => {
                    return self.fail(warnings, internal_error(message, DiagnosticStage::Cache))
                }
            },
        };
        let key_parts = match snapshot_key_parts(
            request,
            &discovery.files,
            &discovery.issues,
            profile,
            &version.engine_build,
            &worker_identities,
            &classifier_identity,
        ) {
            Ok(parts) => parts,
            Err(message) => {
                return self.fail(warnings, internal_error(message, DiagnosticStage::Cache))
            }
        };
        // The strict guard span covers the pre-lookup discovery verification and,
        // for a SQL hit, loading + verifying every artifact row before reuse.
        let verified_hit = if discovery_guards_are_current(&discovery.files, &timing.source_guards)
        {
            let hit = match self.run.store.snapshot_lookup(&key_parts) {
                Ok(hit) => hit,
                Err(error) => {
                    return self.fail(warnings, error.diagnostic(DiagnosticStage::Cache));
                }
            };
            match hit {
                Some(hit) => {
                    let (artifact, pages) = match self
                        .run
                        .store
                        .load_artifact(hit.artifact_id)
                        .and_then(|artifact| {
                            let pages = self.run.store.snapshot_page_audits(&hit, &artifact)?;
                            Ok((artifact, pages))
                        }) {
                        Ok(value) => value,
                        Err(error) => {
                            return self.fail(warnings, error.diagnostic(DiagnosticStage::Cache));
                        }
                    };
                    artifact_guards_are_current(&artifact, &discovery.files, &timing.source_guards)
                        .then_some((hit, artifact, pages))
                }
                None => None,
            }
        } else {
            None
        };
        let snapshot_lookup_ms = elapsed_ms(snapshot_lookup_started);
        let verified_hit =
            verified_hit.and_then(
                |(hit, artifact, pages)| match crate::sources::snapshot_sources(
                    self.run.store,
                    &artifact,
                    &pages,
                    &discovery.files,
                    profile,
                ) {
                    Some(structured) => {
                        *sources = structured;
                        Some((hit, artifact, pages))
                    }
                    None if profile.parse.cache_payload_version == 0
                        && profile.parse.pdf.image_ocr.is_none()
                        && request.window_start_utc.is_none() =>
                    {
                        Some((hit, artifact, pages))
                    }
                    None => None,
                },
            );
        if let Some((hit, artifact, pages)) = verified_hit {
            crate::sources::fingerprint_sources(
                sources,
                work_dir,
                &timing.source_guards,
                profile.execution.max_file_size_bytes,
                Instant::now() + Duration::from_millis(timing.remaining_work_ms()),
            );
            return self.finalize_snapshot_hit(
                &discovery,
                inventory,
                &context_profile_hash,
                &classifier_identity,
                hit,
                artifact,
                pages,
                discovery_duration_ms,
                inventory_prepare_duration_ms,
                snapshot_lookup_ms,
            );
        }

        let input = match ScheduledRunInput::new_with_deadlines(
            active.scan_run_id(),
            started_at_ms,
            work_dir.to_string_lossy().into_owned(),
            discovery.files,
            discovery.issues,
            profile.clone(),
            worker_identities.clone(),
            version.engine_version.clone(),
            version.engine_build.clone(),
            context_profile_hash.clone(),
            rejected_profile_hash,
            discovery_duration_ms,
            timing.deadlines,
        )
        .and_then(|input| {
            input.with_prepared_inventory(
                inventory,
                inventory_existed_before,
                inventory_prepare_duration_ms,
            )
        }) {
            Ok(input) => input,
            Err(failure) => {
                return self.fail(warnings, failure.diagnostic);
            }
        };
        let scheduler = BudgetedContextScheduler::new(
            Box::new(classifier_port),
            Box::new(parser_port),
            Box::new(cache_port),
            Box::new(timing.clock.clone()),
            Box::new(RealGuardVerifier::new(timing.source_guards.clone())),
        );
        let mut outcome = match scheduler.execute(input) {
            Ok(outcome) => outcome,
            Err(failure) => {
                return self.fail(warnings, failure.diagnostic);
            }
        };
        let session_stats =
            crate::session::SessionPoolStats::combine(office_pool.stats(), pdf_pool.stats());
        let peak_worker_rss_bytes = combine_peak_rss(
            self.rss_tracker.peak_worker_rss_bytes(),
            session_stats.peak_worker_rss_bytes,
        );
        apply_worker_rss_observation(&mut outcome, peak_worker_rss_bytes);
        *sources = std::mem::take(&mut outcome.sources);
        crate::sources::fingerprint_sources(
            sources,
            work_dir,
            &timing.source_guards,
            profile.execution.max_file_size_bytes,
            Instant::now() + Duration::from_millis(timing.remaining_work_ms()),
        );

        // ---- terminal finalization (the ONLY linearization point) ----
        // spec Part 2.3: a committed Error run MUST carry a context_runs row
        // (artifact_id=NULL). The scheduler outcome has no context for Error, so a
        // reconciling context is derived from the persisted file/decision rows.
        let derived_error_context = if matches!(outcome.terminal_intent, TerminalIntent::Error) {
            Some(build_error_context_record(
                &outcome.file_results,
                &outcome.inventory,
                &outcome.stage_metrics,
                &context_profile_hash,
            ))
        } else {
            None
        };
        let (envelope, run_status) = scheduler_outcome_envelope(
            request,
            version,
            active,
            &outcome,
            derived_error_context.as_ref(),
        );
        // Build the frozen envelope first. Both payload failures use the observed
        // source count and leave the heartbeat running through the error commit.
        let payload = canonical_envelope_json(&envelope)
            .map_err(|error| error.diagnostic(DiagnosticStage::Internal))
            .and_then(|json| {
                build_batch_artifact(&outcome)
                    .map(|draft| (json, draft))
                    .map_err(|message| internal_error(message, DiagnosticStage::Internal))
            });
        let (envelope_json, artifact_draft) = match payload {
            Ok(payload) => payload,
            Err(error) => {
                let observed_count = outcome
                    .context
                    .as_ref()
                    .map_or(0, |context| context.summary.source_file_count);
                return self.run.persist_error(
                    warnings,
                    error,
                    observed_count,
                    discovery_duration_ms,
                    self.handshake_ms,
                    &self.rss_tracker,
                );
            }
        };
        let snapshot_key_for_batch = artifact_draft
            .as_ref()
            .filter(|draft| draft.snapshot_eligible)
            .map(|_| key_parts);
        let batch = FinalizationBatch {
            status: run_status,
            envelope_json,
            inventory: outcome.inventory,
            file_results: outcome.file_results,
            diagnostics: outcome.diagnostics,
            stage_metrics: outcome.stage_metrics,
            extension_metrics: outcome.extension_metrics,
            context: derived_error_context.or(outcome.context),
            artifact: artifact_draft,
            snapshot_key: snapshot_key_for_batch,
            snapshot_hit: None,
            execution_metrics: Some(assemble_scheduler_execution_metrics(
                &outcome.execution_metrics,
                Some(&session_stats),
                peak_worker_rss_bytes,
                self.handshake_ms,
                discovery_duration_ms,
                snapshot_lookup_ms,
            )),
        };
        self.heartbeat.stop_with_warning(&mut warnings);
        self.commit_outcome(&batch, envelope, warnings, discovery_duration_ms)
    }
}
