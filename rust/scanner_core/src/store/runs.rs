use super::*;

impl ScannerStore {
    pub fn canonicalize_request(
        request: &BuildContextRequest,
        normalized_profile: &NormalizedScannerSettings,
    ) -> Result<CanonicalRequest, StoreError> {
        request.validate().map_err(StoreError::InvalidRequest)?;
        normalized_profile
            .validate()
            .map_err(StoreError::InvalidRequest)?;
        if request.report_mode != normalized_profile.report_mode {
            return Err(StoreError::InvalidRequest(
                "request and normalized report modes disagree".to_string(),
            ));
        }
        let canonical_work_dir = fs::canonicalize(&request.work_dir)
            .map_err(|_| StoreError::InvalidRequest("work_dir is unavailable".to_string()))?;
        let canonical_work_dir =
            normalize_contract_path_text(&canonical_work_dir.to_string_lossy());
        let canonical = CanonicalLogicalRequest {
            contract: &request.contract,
            protocol_version: request.protocol_version,
            work_dir: &canonical_work_dir,
            start_date: &request.start_date,
            end_date: &request.end_date,
            report_mode: request.report_mode,
            scanner_settings: CanonicalScannerSettings {
                core_build_identity: env!("AI_DAILY_ENGINE_BUILD"),
                discovery: &normalized_profile.discovery,
                execution: &normalized_profile.execution,
                parse: &normalized_profile.parse,
            },
            context_profile: &normalized_profile.context,
        };
        let json = serde_json::to_string(&canonical).map_err(|error| {
            StoreError::InvalidRequest(format!("canonical request serialization failed: {error}"))
        })?;
        let hash = cache::domain_hash(b"request-v1\0", json.as_bytes());
        Ok(CanonicalRequest {
            request_id: request.request_id.clone(),
            json,
            hash_algorithm: REQUEST_HASH_ALGORITHM,
            hash,
        })
    }

    pub fn begin_run(
        &mut self,
        request_id: &str,
        canonical: &CanonicalRequest,
        runtime: &AttemptRuntime,
        now_ms: u64,
    ) -> Result<BeginRunOutcome, StoreError> {
        runtime.validate()?;
        let now_ms = checked_i64(now_ms, "run timestamp")?;
        if canonical.hash_algorithm != REQUEST_HASH_ALGORITHM
            || canonical.request_id != request_id
            || !inventory::is_sha256(&canonical.hash)
            || canonical.json.is_empty()
            || cache::domain_hash(b"request-v1\0", canonical.json.as_bytes()) != canonical.hash
        {
            return Err(StoreError::InvalidRequest(
                "canonical request fingerprint is invalid".to_string(),
            ));
        }
        if normalize_runtime_path(&self.path.to_string_lossy()) != runtime.normalized_scan_db_path {
            return Err(StoreError::InvalidRequest(
                "attempt scan database path does not match the opened store".to_string(),
            ));
        }

        if let Some(existing) = query_existing_run(&self.connection, request_id)? {
            ensure_request_hash(&existing, canonical)?;
            if existing.status.is_terminal() {
                return load_stored_envelope(&self.connection, existing, request_id);
            }
        }

        let owner_id = random_owner_id(&self.connection)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        let mut existing = query_existing_run(&transaction, request_id)?;
        if let Some(value) = &existing {
            ensure_request_hash(value, canonical)?;
            if value.status.is_terminal() {
                let stored = load_stored_envelope_ref(&transaction, value, request_id)?;
                transaction.commit().map_err(cache_write)?;
                return Ok(BeginRunOutcome::Stored(Box::new(stored)));
            }
        }

        let lease = query_lease(&transaction)?;
        if let Some(active_lease) = &lease {
            if lease_is_live(active_lease, now_ms) {
                if existing
                    .as_ref()
                    .is_some_and(|run| run.status == PersistedRunStatus::Running)
                {
                    return Err(StoreError::RequestInProgress);
                }
                return Err(StoreError::ScanAlreadyRunning);
            }
            reclaim_expired_lease(&transaction, active_lease, now_ms)?;
            if existing
                .as_ref()
                .is_some_and(|run| run.status == PersistedRunStatus::Running)
            {
                existing.as_mut().expect("existing run").status = PersistedRunStatus::Abandoned;
            }
        } else if existing
            .as_ref()
            .is_some_and(|run| run.status == PersistedRunStatus::Running)
        {
            return Err(StoreError::RunCorrupt(
                "running row has no engine lease".to_string(),
            ));
        }

        let expires_at_ms = lease_expiry(now_ms)?;
        transaction
            .execute(
                "INSERT INTO engine_lease(
                    lease_key, owner_id, owner_pid, acquired_at_ms, heartbeat_at_ms, expires_at_ms
                 ) VALUES (1, ?1, ?2, ?3, ?3, ?4)",
                params![
                    owner_id,
                    i64::from(std::process::id()),
                    now_ms,
                    expires_at_ms,
                ],
            )
            .map_err(cache_write)?;

        let scan_run_id = if let Some(existing) = existing {
            if existing.status != PersistedRunStatus::Abandoned {
                return Err(StoreError::RunCorrupt(
                    "nonterminal run cannot be restarted".to_string(),
                ));
            }
            clear_staging_rows(&transaction, existing.scan_run_id)?;
            let updated = transaction
                .execute(
                    "UPDATE scan_runs
                     SET owner_id=?1, status='running', started_at_ms=?2,
                         updated_at_ms=?2, finished_at_ms=NULL, final_envelope_json=NULL
                     WHERE scan_run_id=?3 AND status='abandoned'",
                    params![owner_id, now_ms, existing.scan_run_id],
                )
                .map_err(cache_write)?;
            if updated != 1 {
                return Err(StoreError::RunCorrupt(
                    "abandoned run could not be reactivated".to_string(),
                ));
            }
            existing.scan_run_id
        } else {
            transaction
                .execute(
                    "INSERT INTO scan_runs(
                        request_id, canonical_request_json, request_hash_algorithm,
                        request_hash, owner_id, status, created_at_ms, started_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?6, ?6)",
                    params![
                        request_id,
                        canonical.json,
                        canonical.hash_algorithm,
                        canonical.hash,
                        owner_id,
                        now_ms,
                    ],
                )
                .map_err(cache_write)?;
            transaction.last_insert_rowid()
        };
        let attempt_number: i64 = transaction
            .query_row(
                "SELECT coalesce(max(attempt_number), 0) + 1
                 FROM scan_run_attempts WHERE scan_run_id=?1",
                [scan_run_id],
                |row| row.get(0),
            )
            .map_err(cache_write)?;
        transaction
            .execute(
                "INSERT INTO scan_run_attempts(
                    scan_run_id, attempt_number, owner_id, normalized_scan_db_path,
                    normalized_office_worker_path, normalized_pdf_worker_path,
                    engine_fingerprint, started_at_ms, status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'running')",
                params![
                    scan_run_id,
                    attempt_number,
                    owner_id,
                    runtime.normalized_scan_db_path,
                    runtime.normalized_office_worker_path,
                    runtime.normalized_pdf_worker_path,
                    runtime.engine_fingerprint,
                    now_ms,
                ],
            )
            .map_err(cache_write)?;
        transaction.commit().map_err(cache_write)?;
        Ok(BeginRunOutcome::Started(ActiveRun {
            scan_run_id,
            attempt_number,
            owner_id,
            request_id: request_id.to_string(),
        }))
    }

    pub fn record_worker_fingerprints(
        &mut self,
        active: &ActiveRun,
        office: Option<&WorkerFingerprint>,
        pdf: Option<&WorkerFingerprint>,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        if let Some(fingerprint) = office {
            fingerprint.validate()?;
        }
        if let Some(fingerprint) = pdf {
            fingerprint.validate()?;
        }
        let now_ms = checked_i64(now_ms, "worker fingerprint timestamp")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        ensure_owner(&transaction, active)?;
        let updated = transaction
            .execute(
                "UPDATE scan_run_attempts SET
                    office_worker_contract=?1, office_worker_version=?2, office_worker_build=?3,
                    pdf_worker_contract=?4, pdf_worker_version=?5, pdf_worker_build=?6
                 WHERE scan_run_id=?7 AND attempt_number=?8 AND owner_id=?9 AND status='running'",
                params![
                    office.map(|value| value.contract.as_str()),
                    office.map(|value| value.version.as_str()),
                    office.map(|value| value.build.as_str()),
                    pdf.map(|value| value.contract.as_str()),
                    pdf.map(|value| value.version.as_str()),
                    pdf.map(|value| value.build.as_str()),
                    active.scan_run_id,
                    active.attempt_number,
                    active.owner_id,
                ],
            )
            .map_err(cache_write)?;
        if updated != 1 {
            return Err(StoreError::LeaseLost);
        }
        heartbeat_in_transaction(&transaction, active, now_ms)?;
        transaction.commit().map_err(cache_write)
    }

    pub fn heartbeat(&mut self, active: &ActiveRun, now_ms: u64) -> Result<(), StoreError> {
        let now_ms = checked_i64(now_ms, "heartbeat timestamp")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        heartbeat_in_transaction(&transaction, active, now_ms)?;
        transaction.commit().map_err(cache_write)
    }

    pub fn abandon_active_run(
        &mut self,
        active: &ActiveRun,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let now_ms = checked_i64(now_ms, "abandon timestamp")?;
        self.connection
            .busy_timeout(Duration::from_millis(0))
            .map_err(cache_open)?;
        let outcome = (|| -> Result<(), StoreError> {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(cache_write)?;
            ensure_owner(&transaction, active)?;
            let run_updated = transaction
                .execute(
                    "UPDATE scan_runs
                     SET status='abandoned', updated_at_ms=?1, finished_at_ms=?1
                     WHERE scan_run_id=?2 AND owner_id=?3 AND status='running'",
                    params![now_ms, active.scan_run_id, active.owner_id],
                )
                .map_err(cache_write)?;
            let attempt_updated = transaction
                .execute(
                    "UPDATE scan_run_attempts
                     SET status='abandoned', finished_at_ms=?1
                     WHERE scan_run_id=?2 AND attempt_number=?3
                       AND owner_id=?4 AND status='running'",
                    params![
                        now_ms,
                        active.scan_run_id,
                        active.attempt_number,
                        active.owner_id,
                    ],
                )
                .map_err(cache_write)?;
            let lease_deleted = transaction
                .execute(
                    "DELETE FROM engine_lease WHERE lease_key=1 AND owner_id=?1",
                    [&active.owner_id],
                )
                .map_err(cache_write)?;
            if run_updated != 1 || attempt_updated != 1 || lease_deleted != 1 {
                return Err(StoreError::LeaseLost);
            }
            transaction.commit().map_err(cache_write)
        })();
        let restore = restore_default_busy_timeout(&self.connection);
        match (outcome, restore) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(_)) => {
                // The cleanup COMMIT already atomically abandoned the run and
                // released its lease. Surface a redacted warning without
                // pretending cleanup failed or attempting a second mutation.
                eprintln!(
                    "scanner warning: SQLite busy timeout restore failed after abandon commit"
                );
                Ok(())
            }
            (Err(error), Ok(())) => Err(error),
            (Err(_), Err(error)) => Err(StoreError::BusyTimeoutRestore {
                detail: error.to_string(),
            }),
        }
    }

    pub fn finalize(
        &mut self,
        active: &ActiveRun,
        batch: &FinalizationBatch,
        now_ms: u64,
        deadlines: RunDeadlines,
        clock: &dyn Clock,
    ) -> Result<TerminalAuditTimings, StoreError> {
        let remaining_ms = deadlines.remaining_to_absolute_deadline(clock);
        if remaining_ms == 0 {
            return Err(StoreError::AbsoluteDeadlineExhausted);
        }
        self.connection
            .busy_timeout(Duration::from_millis(remaining_ms.min(BUSY_TIMEOUT_MS)))
            .map_err(cache_write)?;
        let result =
            self.finalize_with_bounded_busy_timeout(active, batch, now_ms, deadlines, clock);
        let restore = restore_default_busy_timeout(&self.connection);
        match (result, restore) {
            (Ok(timings), Ok(())) => Ok(timings),
            (Ok(mut timings), Err(_)) => {
                // COMMIT already linearized the terminal result. Do not rewrite
                // it as Abandoned merely because this connection cannot be
                // safely reused for optional post-commit work.
                timings.busy_timeout_restore_failed = true;
                Ok(timings)
            }
            (Err(error), Ok(())) => Err(error),
            (Err(_), Err(error)) => Err(StoreError::BusyTimeoutRestore {
                detail: error.to_string(),
            }),
        }
    }

    fn finalize_with_bounded_busy_timeout(
        &mut self,
        active: &ActiveRun,
        batch: &FinalizationBatch,
        now_ms: u64,
        deadlines: RunDeadlines,
        clock: &dyn Clock,
    ) -> Result<TerminalAuditTimings, StoreError> {
        let ensure_deadline = || {
            if deadlines.remaining_to_absolute_deadline(clock) == 0 {
                Err(StoreError::AbsoluteDeadlineExhausted)
            } else {
                Ok(())
            }
        };
        ensure_deadline()?;
        let envelope = validate_finalization_with_check(active, batch, &ensure_deadline)?;
        ensure_deadline()?;
        let now_ms = checked_i64(now_ms, "finalization timestamp")?;
        let metadata_json = envelope_metadata_json(
            &envelope,
            batch
                .execution_metrics
                .as_ref()
                .and_then(|metrics| metrics.refill_audit.as_ref()),
            batch,
        )?;
        let metadata_value: serde_json::Value = serde_json::from_str(&metadata_json)
            .map_err(|error| StoreError::RunCorrupt(error.to_string()))?;
        let status = terminal_status_text(batch.status)?;
        let audit_size = compute_audit_size_with_check(batch, &ensure_deadline)?;
        ensure_deadline()?;
        schema::require_durable_finalization(&self.connection)
            .map_err(|error| cache_write(error.to_string()))?;
        ensure_deadline()?;

        // All JSON construction, artifact loading/dedup comparison, size
        // calculation, and semantic validation happen before opening the
        // terminal transaction. The transaction only applies prepared rows and
        // rechecks indexed database invariants.
        let snapshot_artifact = match &batch.snapshot_hit {
            Some(hit) => Some(load_artifact_from_connection_with_check(
                &self.connection,
                hit.artifact_id,
                &ensure_deadline,
            )?),
            None => None,
        };
        ensure_deadline()?;
        let artifact_for_rebuild = snapshot_artifact.as_ref().or(batch.artifact.as_ref());
        if let Some(context) = &batch.context {
            if batch.status != RunStatus::Error {
                let semantic = &artifact_for_rebuild
                    .ok_or_else(|| {
                        StoreError::RunCorrupt(
                            "success/partial run must carry artifact semantics".to_string(),
                        )
                    })?
                    .semantic_summary;
                let summary = &context.summary;
                if semantic.source_file_count != summary.source_file_count
                    || semantic.success_count != summary.success_count
                    || semantic.timeout_count != summary.timeout_count
                    || semantic.included_file_count != summary.included_file_count
                    || semantic.omitted_file_count != summary.omitted_file_count
                    || semantic.error_file_count != summary.error_file_count
                    || semantic.input_chars != summary.input_chars
                    || semantic.output_chars != summary.output_chars
                {
                    return Err(StoreError::RunCorrupt(
                        "artifact semantic summary disagrees with the current summary".to_string(),
                    ));
                }
            }
        }
        let prepared_semantic_json = batch
            .artifact
            .as_ref()
            .map(semantic_summary_json_for)
            .transpose()?;
        let prepared_artifact_size = match (&batch.artifact, &prepared_semantic_json) {
            (Some(draft), Some(semantic_json)) => Some(artifact_size_bytes_with_check(
                &draft.final_context,
                &draft.context_sha256,
                semantic_json,
                batch.snapshot_key.as_ref(),
                &draft.file_rows,
                &draft.decision_rows,
                &ensure_deadline,
            )?),
            (None, None) => None,
            _ => {
                return Err(StoreError::RunCorrupt(
                    "artifact preparation is incomplete".to_string(),
                ))
            }
        };
        let dedup_artifact_id = match (&batch.artifact, &batch.snapshot_key) {
            (Some(draft), Some(key)) => {
                dedup_artifact(&self.connection, draft, key, &ensure_deadline)?
            }
            _ => None,
        };
        let mut protected_runs: HashSet<i64> = HashSet::new();
        protected_runs.insert(active.scan_run_id);
        let mut protected_artifacts: HashSet<i64> = HashSet::new();
        if let Some(hit) = &batch.snapshot_hit {
            protected_runs.insert(hit.reused_from_context_run_id);
            protected_artifacts.insert(hit.artifact_id);
        }
        if let Some(artifact_id) = dedup_artifact_id {
            protected_artifacts.insert(artifact_id);
        }
        let new_artifact_size = if batch.artifact.is_some() && dedup_artifact_id.is_none() {
            prepared_artifact_size
        } else {
            None
        };
        let retention_plan = prepare_finalization_retention_plan(
            &self.connection,
            active.scan_run_id,
            &protected_runs,
            &protected_artifacts,
            new_artifact_size,
            now_ms,
            audit_size,
            &ensure_deadline,
        )?;
        ensure_deadline()?;
        let rebuild_started = clock.now_ms();
        let rebuilt = crate::artifact::rebuild_envelope(
            &metadata_value,
            &envelope.summary,
            artifact_for_rebuild,
        )
        .map_err(|message| {
            StoreError::RunCorrupt(format!("rebuilt envelope is invalid: {message}"))
        })?;
        if canonical_envelope_json(&rebuilt)? != batch.envelope_json {
            return Err(StoreError::RunCorrupt(
                "rebuilt envelope disagrees with the committed envelope".to_string(),
            ));
        }
        let envelope_rebuild_ms = clock.now_ms().saturating_sub(rebuild_started);
        let terminal_rows_written =
            compute_terminal_rows_written(batch, dedup_artifact_id.is_some());
        ensure_deadline()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(cache_write)?;
        let transaction_started = clock.now_ms();
        heartbeat_in_transaction(&transaction, active, now_ms)?;
        ensure_engine_fingerprint(&transaction, active, &envelope)?;
        let handshake_failed = batch.diagnostics.iter().any(|record| {
            record.severity == DiagnosticSeverity::Error
                && matches!(
                    record.diagnostic.error_code,
                    ErrorCode::WorkerHandshakeFailed | ErrorCode::WorkerVersionMismatch
                )
        });
        let fingerprints_required = matches!(batch.status, RunStatus::Success | RunStatus::Partial)
            || !batch.inventory.is_empty()
            || !batch.file_results.is_empty()
            || !handshake_failed;
        if fingerprints_required {
            ensure_worker_fingerprints(&transaction, active)?;
        }

        let audit_write_started = clock.now_ms();
        let snapshot_rows = batch.snapshot_hit.is_some();
        for records in batch.inventory.chunks(TERMINAL_WRITE_BATCH_SIZE) {
            ensure_deadline()?;
            verify_inventory_snapshot(&transaction, active.scan_run_id, records)?;
        }
        for records in batch.file_results.chunks(TERMINAL_WRITE_BATCH_SIZE) {
            ensure_deadline()?;
            inventory::insert_file_results(
                &transaction,
                active.scan_run_id,
                records,
                snapshot_rows,
            )
            .map_err(cache_write)?;
        }
        for (batch_index, records) in batch
            .diagnostics
            .chunks(TERMINAL_WRITE_BATCH_SIZE)
            .enumerate()
        {
            ensure_deadline()?;
            insert_diagnostics(
                &transaction,
                active.scan_run_id,
                batch_index * TERMINAL_WRITE_BATCH_SIZE,
                records,
            )
            .map_err(cache_write)?;
            #[cfg(test)]
            TEST_TERMINAL_DIAGNOSTIC_BATCHES_WRITTEN.with(|count| {
                count.set(count.get().saturating_add(1));
            });
        }
        for records in batch.stage_metrics.chunks(TERMINAL_WRITE_BATCH_SIZE) {
            ensure_deadline()?;
            crate::metrics::insert_stage_metrics(&transaction, active.scan_run_id, records)
                .map_err(cache_write)?;
        }
        for records in batch.extension_metrics.chunks(TERMINAL_WRITE_BATCH_SIZE) {
            ensure_deadline()?;
            crate::metrics::insert_extension_metrics(&transaction, active.scan_run_id, records)
                .map_err(cache_write)?;
        }
        ensure_deadline()?;
        let current_run_audit_write_ms = clock.now_ms().saturating_sub(audit_write_started);

        // ---- artifact write / snapshot-hit reference + context_runs ----
        // spec Part 4/5.2: establish the current `context_runs.artifact_id`
        // reference and a temporary protected set BEFORE the retention/orphan
        // sweep, so a just-hit artifact can never be reclaimed in the
        // "old reference deleted, current reference not yet created" window.
        if let Some(context) = &batch.context {
            if batch.status == RunStatus::Error {
                // spec Part 2.3: a committed Error run MUST write a context_runs
                // row with artifact_id=NULL (no artifact, no snapshot, no reuse).
                insert_context(
                    &transaction,
                    active.scan_run_id,
                    context,
                    None,
                    false,
                    None,
                    now_ms,
                    &ensure_deadline,
                )?;
            } else {
                let artifact_id = if let Some(hit) = &batch.snapshot_hit {
                    let still_present: bool = transaction
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM context_artifacts WHERE artifact_id=?1)",
                            [hit.artifact_id],
                            |row| row.get(0),
                        )
                        .map_err(cache_write)?;
                    if !still_present {
                        return Err(StoreError::RunCorrupt(
                            "snapshot artifact disappeared before terminal commit".to_string(),
                        ));
                    }
                    protected_artifacts.insert(hit.artifact_id);
                    protected_runs.insert(hit.reused_from_context_run_id);
                    hit.artifact_id
                } else {
                    let draft = batch.artifact.as_ref().ok_or_else(|| {
                        StoreError::RunCorrupt(
                            "success/partial run must carry an artifact draft".to_string(),
                        )
                    })?;
                    if draft.snapshot_eligible != batch.snapshot_key.is_some() {
                        return Err(StoreError::RunCorrupt(
                            "artifact eligibility disagrees with the snapshot key".to_string(),
                        ));
                    }
                    let artifact_id = if let Some(existing) = dedup_artifact_id {
                        existing
                    } else {
                        let semantic_json = prepared_semantic_json.as_deref().ok_or_else(|| {
                            StoreError::RunCorrupt(
                                "artifact semantic JSON was not prepared".to_string(),
                            )
                        })?;
                        let size = prepared_artifact_size.ok_or_else(|| {
                            StoreError::RunCorrupt("artifact size was not prepared".to_string())
                        })?;
                        apply_artifact_retention_plan(
                            &transaction,
                            &retention_plan,
                            &protected_runs,
                            &protected_artifacts,
                            &ensure_deadline,
                        )?;
                        insert_artifact(
                            &transaction,
                            draft,
                            batch.snapshot_key.as_ref(),
                            semantic_json,
                            size,
                            now_ms,
                            &ensure_deadline,
                        )?
                    };
                    protected_artifacts.insert(artifact_id);
                    artifact_id
                };
                insert_context(
                    &transaction,
                    active.scan_run_id,
                    context,
                    Some(artifact_id),
                    batch.snapshot_hit.is_some(),
                    batch
                        .snapshot_hit
                        .as_ref()
                        .map(|hit| hit.reused_from_context_run_id),
                    now_ms,
                    &ensure_deadline,
                )?;
            }
        }

        let updated = transaction
            .execute(
                "UPDATE scan_runs
                 SET status=?1, updated_at_ms=?2, finished_at_ms=?2, final_envelope_json=?3,
                     final_envelope_metadata_json=?4, audit_size_bytes=?5
                 WHERE scan_run_id=?6 AND owner_id=?7 AND status='running'",
                params![
                    status,
                    now_ms,
                    // spec Part 5.1: the body is stored ONCE in the artifact; the
                    // persisted scan_runs JSON is the body-free metadata, and
                    // idempotent replay rebuilds the full ContextEnvelope from
                    // metadata + summary + artifact.
                    metadata_json,
                    metadata_json,
                    audit_size,
                    active.scan_run_id,
                    active.owner_id,
                ],
            )
            .map_err(cache_write)?;
        if updated != 1 {
            return Err(StoreError::LeaseLost);
        }
        let attempt_updated = transaction
            .execute(
                "UPDATE scan_run_attempts SET status=?1, finished_at_ms=?2
                 WHERE scan_run_id=?3 AND attempt_number=?4 AND owner_id=?5 AND status='running'",
                params![
                    status,
                    now_ms,
                    active.scan_run_id,
                    active.attempt_number,
                    active.owner_id,
                ],
            )
            .map_err(cache_write)?;
        if attempt_updated != 1 {
            return Err(StoreError::LeaseLost);
        }
        // spec Part 4/5.2 terminal run GC: protected set = current run +
        // snapshot-hit source run; the just-hit artifact is protected by the
        // current context_runs reference.
        apply_terminal_retention_plan(
            &transaction,
            &retention_plan,
            &protected_runs,
            &ensure_deadline,
        )?;
        ensure_deadline()?;
        // spec Part 5.3: the authoritative `execution_metrics` row is bound at
        // the precommit checkpoint. `terminal_precommit_ms` runs from transaction
        // begin to just before the metrics write; `envelope_rebuild_ms` covers the
        // metadata+summary+artifact rebuild and validation.
        let terminal_precommit_ms = clock.now_ms().saturating_sub(transaction_started);
        if let Some(metrics) = &batch.execution_metrics {
            let mut metrics = metrics.clone();
            metrics.current_run_audit_write_ms = current_run_audit_write_ms;
            metrics.terminal_precommit_ms = terminal_precommit_ms;
            metrics.terminal_rows_written = terminal_rows_written;
            metrics.envelope_rebuild_ms = envelope_rebuild_ms;
            metrics.deadline_precommit_elapsed_ms =
                clock.now_ms().saturating_sub(deadlines.origin_ms);
            metrics.validate().map_err(|message| {
                StoreError::RunCorrupt(format!("execution metrics are invalid: {message}"))
            })?;
            insert_execution_metrics(&transaction, active.scan_run_id, &metrics)?;
        }
        ensure_deadline()?;
        let lease_deleted = transaction
            .execute(
                "DELETE FROM engine_lease WHERE lease_key=1 AND owner_id=?1",
                [&active.owner_id],
            )
            .map_err(cache_write)?;
        if lease_deleted != 1 {
            return Err(StoreError::LeaseLost);
        }
        // COMMIT is non-preemptible. The last legal check is immediately before
        // calling it; a successful COMMIT remains authoritative if it overshoots.
        ensure_deadline()?;
        transaction.commit().map_err(cache_write)?;

        // The exact bytes committed above must still parse as the validated object.
        debug_assert_eq!(envelope.request_id, active.request_id);
        Ok(TerminalAuditTimings {
            current_run_audit_write_ms,
            terminal_precommit_ms,
            envelope_rebuild_ms,
            terminal_rows_written,
            busy_timeout_restore_failed: false,
        })
    }

    pub(crate) fn validate_finalization_batch(
        &self,
        active: &ActiveRun,
        batch: &FinalizationBatch,
    ) -> Result<(), StoreError> {
        validate_finalization(active, batch).map(|_| ())
    }

    pub fn load_terminal_envelope(&self, scan_run_id: u64) -> Result<StoredEnvelope, StoreError> {
        let scan_run_id = checked_i64(scan_run_id, "scan run id")?;
        let row: (String, String, String, String, Option<String>) = self
            .connection
            .query_row(
                "SELECT request_id, canonical_request_json, request_hash, status,
                        final_envelope_metadata_json
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
            .map_err(cache_open)?
            .ok_or(StoreError::RunNotFound)?;
        let existing = ExistingRun {
            scan_run_id,
            canonical_request_json: row.1,
            request_hash: row.2,
            status: PersistedRunStatus::parse(&row.3)?,
            final_envelope_metadata_json: row.4,
        };
        load_stored_envelope_ref(&self.connection, &existing, &row.0)
    }

    pub fn load_diagnostics(
        &self,
        scan_run_id: u64,
    ) -> Result<Vec<RunDiagnosticRecord>, StoreError> {
        let scan_run_id = checked_i64(scan_run_id, "scan run id")?;
        let exists: bool = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scan_runs WHERE scan_run_id=?1)",
                [scan_run_id],
                |row| row.get(0),
            )
            .map_err(cache_open)?;
        if !exists {
            return Err(StoreError::RunNotFound);
        }
        let mut statement = self
            .connection
            .prepare(
                "SELECT severity, error_code, message, retryable, stage, file_path, backend
                 FROM run_diagnostics WHERE scan_run_id=?1 ORDER BY sequence",
            )
            .map_err(cache_open)?;
        let rows: Vec<RawDiagnosticRow> = statement
            .query_map([scan_run_id], |row| {
                Ok(RawDiagnosticRow {
                    severity: row.get(0)?,
                    error_code: row.get(1)?,
                    message: row.get(2)?,
                    retryable: row.get::<_, i64>(3)? != 0,
                    stage: row.get(4)?,
                    file_path: row.get(5)?,
                    backend: row.get(6)?,
                })
            })
            .map_err(cache_open)?
            .collect::<Result<_, _>>()
            .map_err(cache_open)?;
        rows.into_iter()
            .map(|row| {
                let severity = match row.severity.as_str() {
                    "warning" => DiagnosticSeverity::Warning,
                    "error" => DiagnosticSeverity::Error,
                    _ => {
                        return Err(StoreError::RunCorrupt(
                            "diagnostic severity is invalid".to_string(),
                        ));
                    }
                };
                let diagnostic = Diagnostic {
                    error_code: parse_contract_enum(&row.error_code)?,
                    message: row.message,
                    retryable: row.retryable,
                    stage: parse_contract_enum(&row.stage)?,
                    file_path: Nullable(row.file_path),
                    backend: Nullable(row.backend),
                };
                diagnostic.validate().map_err(|_| {
                    StoreError::RunCorrupt("persisted diagnostic is invalid".to_string())
                })?;
                Ok(RunDiagnosticRecord {
                    severity,
                    diagnostic,
                })
            })
            .collect()
    }

    pub fn load_evidence(
        &mut self,
        scan_run_id: u64,
    ) -> Result<crate::context_audit::EvidenceSnapshot, crate::context_audit::EvidenceLoadError>
    {
        let scan_run_id =
            i64::try_from(scan_run_id).map_err(|_| crate::context_audit::EvidenceLoadError {
                error: crate::context_audit::EvidenceAuditError::RunNotFound,
                run_status: None,
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| crate::context_audit::EvidenceLoadError {
                error: crate::context_audit::EvidenceAuditError::Sql(error),
                run_status: None,
            })?;
        let snapshot = crate::context_audit::load_evidence_snapshot(&transaction, scan_run_id)?;
        transaction
            .commit()
            .map_err(|error| crate::context_audit::EvidenceLoadError {
                error: crate::context_audit::EvidenceAuditError::Sql(error),
                run_status: Some(snapshot.run_status),
            })?;
        Ok(snapshot)
    }
}

/// `scan_runs.audit_size_bytes` 的逻辑 payload 近似（spec Part 4）：覆盖 run 的
/// envelope/attempt/diagnostic/current file/decision/stage/extension/context-run
/// 元数据；不含其引用的 artifact、全局 inventory 或 cache。Store 在 insert 时
/// 计算，不接受 wire 输入。
/// Logical INSERT/UPDATE row count of the terminal transaction (spec Part 5.3
/// `terminal_rows_written`): current run/artifact/context/metric rows only;
/// retention deletions and cache-transaction rows are excluded.
pub(super) fn compute_terminal_rows_written(
    batch: &FinalizationBatch,
    artifact_deduplicated: bool,
) -> u64 {
    let mut rows = 0_u64;
    rows = rows.saturating_add(batch.file_results.len() as u64);
    rows = rows.saturating_add(batch.file_results.len() as u64); // scan_file_execution
    rows = rows.saturating_add(batch.diagnostics.len() as u64);
    rows = rows.saturating_add(batch.stage_metrics.len() as u64);
    rows = rows.saturating_add(batch.extension_metrics.len() as u64);
    if let Some(context) = &batch.context {
        rows = rows.saturating_add(1); // context_runs row
        rows = rows.saturating_add(context.decisions.len() as u64);
        if batch.snapshot_hit.is_none() && !artifact_deduplicated {
            if let Some(artifact) = &batch.artifact {
                rows = rows.saturating_add(1); // artifact parent
                rows = rows.saturating_add(artifact.file_rows.len() as u64);
                rows = rows.saturating_add(artifact.decision_rows.len() as u64);
            }
        }
    }
    rows = rows.saturating_add(2); // scan_runs + scan_run_attempts UPDATE
    rows = rows.saturating_add(1); // execution_metrics row
    rows
}

#[cfg(test)]
pub(super) fn compute_audit_size(batch: &FinalizationBatch) -> i64 {
    compute_audit_size_with_check(batch, &|| Ok(())).unwrap_or(i64::MAX)
}

pub(super) fn compute_audit_size_with_check(
    batch: &FinalizationBatch,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<i64, StoreError> {
    let mut total = batch.envelope_json.len() as i64;
    if let Some(refill) = batch
        .execution_metrics
        .as_ref()
        .and_then(|metrics| metrics.refill_audit.as_ref())
    {
        let bytes = serde_json::to_vec(refill)
            .map_err(|error| StoreError::InvalidRequest(error.to_string()))?;
        // Metadata is retained in both legacy-envelope and metadata columns.
        total = total.saturating_add(bytes.len() as i64 * 2 + 32);
    }
    for record in &batch.file_results {
        if let Some(audit) = record
            .pdf_classification
            .as_ref()
            .and_then(|item| item.page_audit.as_ref())
        {
            let bytes = serde_json::to_vec(audit)
                .map_err(|error| StoreError::InvalidRequest(error.to_string()))?;
            total =
                total.saturating_add((bytes.len() + record.file_identity.len()) as i64 * 2 + 64);
        }
    }
    for (index, record) in batch.file_results.iter().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        total += record.relative_path.len() as i64 + 256;
        total += file_execution_audit_size(record, batch.snapshot_hit.is_some());
    }
    for (index, record) in batch.diagnostics.iter().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        total += record.diagnostic.message.len() as i64 + 160;
    }
    total += batch.stage_metrics.len() as i64 * 48;
    for record in &batch.extension_metrics {
        total += record.extension.len() as i64 + 64;
    }
    if let Some(context) = &batch.context {
        total += context.final_context.len() as i64 + 320;
        for (index, record) in context.decisions.iter().enumerate() {
            if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
                ensure_deadline()?;
            }
            total += record.decision.relative_path.len() as i64 + 160;
        }
    }
    Ok(total)
}

pub(super) fn file_execution_audit_size(record: &FileResultRecord, snapshot_rows: bool) -> i64 {
    const INTEGER_BYTES: i64 = 8;
    const NULL_MARKER_BYTES: i64 = 1;

    let text_bytes = |value: &str| i64::try_from(value.len()).unwrap_or(i64::MAX);
    let nullable_integer_bytes = |value: Option<u64>| {
        if value.is_some() {
            INTEGER_BYTES
        } else {
            NULL_MARKER_BYTES
        }
    };
    let parse_transport = if snapshot_rows {
        "snapshot".to_string()
    } else {
        inventory::enum_text(&record.parse_transport)
    };
    let mut total = 64_i64
        .saturating_add(INTEGER_BYTES)
        .saturating_add(text_bytes(&record.file_identity))
        .saturating_add(text_bytes(&parse_transport))
        .saturating_add(INTEGER_BYTES);

    let Some(classification) = record.pdf_classification.as_ref() else {
        return total.saturating_add(12 * NULL_MARKER_BYTES);
    };
    let classification_cache_status = if snapshot_rows
        && classification.status
            != ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget
    {
        "snapshot".to_string()
    } else {
        inventory::enum_text(&classification.classification_cache_status)
    };
    let classification_miss_reason = if snapshot_rows {
        ""
    } else {
        classification.classification_cache_miss_reason.as_str()
    };
    let classification_transport = if snapshot_rows
        && classification.status
            != ai_daily_scanner_contract::PdfClassificationStatus::NotClassifiedByBudget
    {
        "snapshot".to_string()
    } else {
        inventory::enum_text(&classification.transport)
    };
    total = total
        .saturating_add(text_bytes(&inventory::enum_text(&classification.status)))
        .saturating_add(nullable_integer_bytes(classification.page_count.0))
        .saturating_add(text_bytes(&classification_cache_status))
        .saturating_add(text_bytes(classification_miss_reason))
        .saturating_add(nullable_integer_bytes(
            classification.result_examined_pages.0,
        ))
        .saturating_add(nullable_integer_bytes(classification.run_inspected_pages.0))
        .saturating_add(INTEGER_BYTES)
        .saturating_add(INTEGER_BYTES)
        .saturating_add(text_bytes(&classification_transport))
        .saturating_add(INTEGER_BYTES)
        .saturating_add(text_bytes(&classification.classifier_build))
        .saturating_add(text_bytes(&classification.classifier_profile_hash));
    total
}

pub(super) fn query_existing_run(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<ExistingRun>, StoreError> {
    let row: Option<(i64, String, String, String, Option<String>)> = connection
        .query_row(
            "SELECT scan_run_id, canonical_request_json, request_hash, status,
                    final_envelope_metadata_json
             FROM scan_runs WHERE request_id=?1",
            [request_id],
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
        .map_err(cache_open)?;
    row.map(|row| {
        Ok(ExistingRun {
            scan_run_id: row.0,
            canonical_request_json: row.1,
            request_hash: row.2,
            status: PersistedRunStatus::parse(&row.3)?,
            final_envelope_metadata_json: row.4,
        })
    })
    .transpose()
}

pub(super) fn ensure_request_hash(
    existing: &ExistingRun,
    canonical: &CanonicalRequest,
) -> Result<(), StoreError> {
    if cache::domain_hash(b"request-v1\0", existing.canonical_request_json.as_bytes())
        != existing.request_hash
    {
        return Err(StoreError::RunCorrupt(
            "stored logical request hash is invalid".to_string(),
        ));
    }
    if existing.request_hash != canonical.hash || existing.canonical_request_json != canonical.json
    {
        return Err(StoreError::RequestIdConflict);
    }
    Ok(())
}

pub(super) fn load_stored_envelope(
    connection: &Connection,
    existing: ExistingRun,
    request_id: &str,
) -> Result<BeginRunOutcome, StoreError> {
    load_stored_envelope_ref(connection, &existing, request_id)
        .map(Box::new)
        .map(BeginRunOutcome::Stored)
}

/// Spec Part 5.1 idempotent replay: the full `ContextEnvelope v1` is REBUILT
/// from the body-free `final_envelope_metadata_json` + the stored summary +
/// the artifact's `final_context` (Success/Partial), never read from a
/// body-carrying scan_runs JSON. `final_context` is stored exactly once.
pub(super) fn load_stored_envelope_ref(
    connection: &Connection,
    existing: &ExistingRun,
    request_id: &str,
) -> Result<StoredEnvelope, StoreError> {
    if cache::domain_hash(b"request-v1\0", existing.canonical_request_json.as_bytes())
        != existing.request_hash
    {
        return Err(StoreError::RunCorrupt(
            "stored logical request hash is invalid".to_string(),
        ));
    }
    if !existing.status.is_terminal() {
        return Err(StoreError::RunCorrupt(
            "nonterminal run has no reusable envelope".to_string(),
        ));
    }
    let metadata_json = existing
        .final_envelope_metadata_json
        .clone()
        .ok_or_else(|| {
            StoreError::RunCorrupt("terminal run has no envelope metadata".to_string())
        })?;
    let envelope =
        rebuild_envelope_from_metadata(connection, existing.scan_run_id, &metadata_json)?;
    let envelope_json = canonical_envelope_json(&envelope)?;
    if envelope.request_id != request_id
        || envelope.scan_run_id.0 != Some(existing.scan_run_id as u64)
        || !envelope_status_matches(existing.status, envelope.status)
    {
        return Err(StoreError::RunCorrupt(
            "rebuilt envelope does not match its run".to_string(),
        ));
    }
    Ok(StoredEnvelope {
        scan_run_id: existing.scan_run_id as u64,
        envelope_json,
        envelope,
    })
}

/// Shared spec Part 5.1 envelope rebuild for idempotent replay: parse the
/// body-free `final_envelope_metadata_json`, read the stored summary, load the
/// referenced artifact, and rebuild + re-validate the full `ContextEnvelope v1`.
/// `final_context` is never read from scan_runs.
pub(crate) fn rebuild_envelope_from_metadata(
    connection: &Connection,
    scan_run_id: i64,
    metadata_json: &str,
) -> Result<ContextEnvelope, StoreError> {
    let metadata: serde_json::Value = serde_json::from_str(metadata_json)
        .map_err(|_| StoreError::RunCorrupt("envelope metadata JSON is invalid".to_string()))?;
    let summary: ContextSummary =
        serde_json::from_value(metadata.get("summary").cloned().ok_or_else(|| {
            StoreError::RunCorrupt("envelope metadata missing summary".to_string())
        })?)
        .map_err(|_| StoreError::RunCorrupt("envelope metadata summary is invalid".to_string()))?;
    let artifact = load_artifact_for_replay(connection, scan_run_id)?;
    let envelope = crate::artifact::rebuild_envelope(&metadata, &summary, artifact.as_ref())
        .map_err(|message| {
            StoreError::RunCorrupt(format!("rebuilt envelope is invalid: {message}"))
        })?;
    envelope.validate().map_err(|_| {
        StoreError::RunCorrupt("rebuilt envelope violates the contract".to_string())
    })?;
    Ok(envelope)
}

pub(super) fn envelope_status_matches(
    run_status: PersistedRunStatus,
    engine_status: EngineStatus,
) -> bool {
    matches!(
        (run_status, engine_status),
        (PersistedRunStatus::Success, EngineStatus::Ok)
            | (PersistedRunStatus::Partial, EngineStatus::Partial)
            | (PersistedRunStatus::Error, EngineStatus::Error)
    )
}

pub(super) fn validate_finalization(
    active: &ActiveRun,
    batch: &FinalizationBatch,
) -> Result<ContextEnvelope, StoreError> {
    validate_finalization_with_check(active, batch, &|| Ok(()))
}

pub(super) fn validate_finalization_with_check(
    active: &ActiveRun,
    batch: &FinalizationBatch,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<ContextEnvelope, StoreError> {
    terminal_status_text(batch.status)?;
    if batch.inventory.len() > 1_000_000
        || batch.file_results.len() > 1_000_000
        || batch.diagnostics.len() > 100_001
    {
        return Err(StoreError::InvalidRequest(
            "finalization batch exceeds the v1 row limits".to_string(),
        ));
    }
    let mut inventory_by_identity = std::collections::HashMap::new();
    for (index, record) in batch.inventory.iter().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        record.validate().map_err(StoreError::InvalidRequest)?;
        if inventory_by_identity
            .insert(record.file_identity.as_str(), record)
            .is_some()
        {
            return Err(StoreError::InvalidRequest(
                "inventory identities must be unique".to_string(),
            ));
        }
    }
    let mut file_results_by_identity = std::collections::HashMap::new();
    for (index, record) in batch.file_results.iter().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        record
            .validate_for_persistence(batch.snapshot_hit.is_some())
            .map_err(StoreError::InvalidRequest)?;
        let inventory = inventory_by_identity
            .get(record.file_identity.as_str())
            .ok_or_else(|| {
                StoreError::InvalidRequest(
                    "file result does not reference final inventory".to_string(),
                )
            })?;
        if inventory.source_version != record.source_version
            || file_results_by_identity
                .insert(record.file_identity.as_str(), record)
                .is_some()
        {
            return Err(StoreError::InvalidRequest(
                "file results must uniquely match inventory source versions".to_string(),
            ));
        }
    }
    for (index, record) in batch.diagnostics.iter().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        record
            .diagnostic
            .validate()
            .map_err(StoreError::InvalidRequest)?;
    }
    crate::metrics::validate_metrics(&batch.stage_metrics, &batch.extension_metrics)
        .map_err(StoreError::InvalidRequest)?;

    ensure_deadline()?;
    let envelope: ContextEnvelope = serde_json::from_str(&batch.envelope_json)
        .map_err(|_| StoreError::InvalidRequest("final envelope JSON is invalid".to_string()))?;
    envelope.validate().map_err(StoreError::InvalidRequest)?;
    if canonical_envelope_json(&envelope).as_deref() != Ok(batch.envelope_json.as_str()) {
        return Err(StoreError::InvalidRequest(
            "final envelope JSON must use canonical serialization".to_string(),
        ));
    }
    if envelope.request_id != active.request_id
        || envelope.scan_run_id.0 != Some(active.scan_run_id())
        || !final_status_matches(batch.status, envelope.status)
    {
        return Err(StoreError::InvalidRequest(
            "final envelope does not match the active run".to_string(),
        ));
    }

    let mut warning_diagnostics: Vec<Diagnostic> = Vec::new();
    let mut error_diagnostics: Vec<Diagnostic> = Vec::new();
    for records in batch.diagnostics.chunks(TERMINAL_WRITE_BATCH_SIZE) {
        ensure_deadline()?;
        for record in records {
            match record.severity {
                DiagnosticSeverity::Warning => warning_diagnostics.push(record.diagnostic.clone()),
                DiagnosticSeverity::Error => error_diagnostics.push(record.diagnostic.clone()),
            }
        }
    }
    if warning_diagnostics != envelope.warnings
        || error_diagnostics != envelope.error.0.iter().cloned().collect::<Vec<_>>()
    {
        return Err(StoreError::InvalidRequest(
            "final envelope diagnostics disagree with run diagnostics".to_string(),
        ));
    }

    match (&batch.context, envelope.context_run_id.0) {
        (Some(context), Some(context_run_id)) if context_run_id == active.context_run_id() => {
            validate_context(context, ensure_deadline)?;
            let mut decision_identities: std::collections::HashSet<&str> =
                std::collections::HashSet::new();
            for records in context.decisions.chunks(TERMINAL_WRITE_BATCH_SIZE) {
                ensure_deadline()?;
                decision_identities
                    .extend(records.iter().map(|record| record.file_identity.as_str()));
            }
            let inventory_identities: std::collections::HashSet<&str> =
                inventory_by_identity.keys().copied().collect();
            if context.status != batch.status
                || context.final_context != envelope.file_context
                || context.summary != envelope.summary
                || context.summary.source_file_count != batch.inventory.len() as u64
                || decision_identities != inventory_identities
                || file_results_by_identity.len() != inventory_by_identity.len()
            {
                return Err(StoreError::InvalidRequest(
                    "context rows disagree with the final envelope".to_string(),
                ));
            }
            validate_context_relations(
                context,
                &inventory_by_identity,
                &file_results_by_identity,
                &batch.stage_metrics,
                &batch.extension_metrics,
                ensure_deadline,
            )?;
        }
        (None, None) => {}
        _ => {
            return Err(StoreError::InvalidRequest(
                "context run identity is inconsistent".to_string(),
            ));
        }
    }
    Ok(envelope)
}

/// Rechecks that the independently committed inventory receipt still matches
/// this run's immutable discovery snapshot. This is an indexed invariant read,
/// never an inventory mutation inside the terminal transaction.
pub(super) fn verify_inventory_snapshot(
    transaction: &rusqlite::Transaction<'_>,
    scan_run_id: i64,
    records: &[InventoryRecord],
) -> Result<(), StoreError> {
    let mut statement = transaction
        .prepare_cached(
            "SELECT absolute_path, relative_path, file_type, source_version,
                    size_bytes, mtime_ns, last_seen_run_id,
                    source_guard_kind, source_guard_sha256
             FROM file_inventory WHERE file_identity=?1",
        )
        .map_err(cache_write)?;
    for record in records {
        let persisted = statement
            .query_row([&record.file_identity], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            })
            .optional()
            .map_err(cache_write)?;
        let Some(persisted) = persisted else {
            return Err(StoreError::RunCorrupt(
                "terminal inventory receipt is incomplete".to_string(),
            ));
        };
        let size_bytes = i64::try_from(record.size_bytes).map_err(|_| {
            StoreError::InvalidRequest("inventory size exceeds SQLite integer range".to_string())
        })?;
        let mtime_ns = i64::try_from(record.mtime_ns).map_err(|_| {
            StoreError::InvalidRequest("inventory mtime exceeds SQLite integer range".to_string())
        })?;
        if persisted
            != (
                record.absolute_path.clone(),
                record.relative_path.clone(),
                record.file_type.clone(),
                record.source_version.clone(),
                size_bytes,
                mtime_ns,
                Some(scan_run_id),
                record.source_guard_kind.clone(),
                record.source_guard_sha256.clone(),
            )
        {
            return Err(StoreError::RunCorrupt(
                "terminal inventory receipt disagrees with discovery".to_string(),
            ));
        }
    }
    Ok(())
}

pub(super) fn validate_context_relations(
    context: &ContextRunRecord,
    inventory: &std::collections::HashMap<&str, &InventoryRecord>,
    file_results: &std::collections::HashMap<&str, &FileResultRecord>,
    stage_metrics: &[StageMetric],
    extension_metrics: &[ExtensionMetric],
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let summary = &context.summary;
    let mut success_count = 0_u64;
    let mut timeout_count = 0_u64;
    let mut error_count = 0_u64;
    for (index, result) in file_results.values().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        match result.parse_status {
            ParseStatus::Success => success_count += 1,
            ParseStatus::Timeout => timeout_count += 1,
            ParseStatus::Error => error_count += 1,
            ParseStatus::NotParsed => {}
        }
    }
    // spec Part 2.2: `not_parsed_count` is DERIVED, not a stored counter.
    let not_parsed_count = (file_results.len() as u64)
        .checked_sub(success_count)
        .and_then(|value| value.checked_sub(timeout_count))
        .and_then(|value| value.checked_sub(error_count))
        .ok_or_else(|| StoreError::InvalidRequest("file status counts overflow".to_string()))?;
    let mut included_count = 0_u64;
    let mut omitted_count = 0_u64;
    let mut decision_error_count = 0_u64;
    let mut input_chars = 0_u64;
    for (index, record) in context.decisions.iter().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        match record.decision.action {
            ContextAction::Keep | ContextAction::Compress | ContextAction::MetadataOnly => {
                included_count += 1
            }
            ContextAction::Omit => omitted_count += 1,
            ContextAction::Error => decision_error_count += 1,
        }
        input_chars = input_chars
            .checked_add(record.decision.input_chars)
            .ok_or_else(|| {
                StoreError::InvalidRequest("decision input count overflows".to_string())
            })?;
        let item = inventory
            .get(record.file_identity.as_str())
            .ok_or_else(|| {
                StoreError::InvalidRequest("decision inventory is missing".to_string())
            })?;
        if item.relative_path != record.decision.relative_path {
            return Err(StoreError::InvalidRequest(
                "decision path disagrees with inventory".to_string(),
            ));
        }
    }
    if summary.success_count != success_count
        || summary.timeout_count != timeout_count
        || summary.error_file_count != error_count
        || summary.included_file_count != included_count
        || summary.omitted_file_count != omitted_count
        // spec Part 2.2 count equations: included = success, omitted = derived
        // not_parsed, decision_error = error + timeout.
        || included_count != success_count
        || omitted_count != not_parsed_count
        || decision_error_count != error_count + timeout_count
        || included_count + omitted_count + decision_error_count != context.decisions.len() as u64
        || summary.input_chars != input_chars
        || summary.output_chars != context.final_context.chars().count() as u64
    {
        return Err(StoreError::InvalidRequest(
            "context summary disagrees with file or decision rows".to_string(),
        ));
    }

    let stage_by_name: std::collections::HashMap<StageName, &StageMetric> = stage_metrics
        .iter()
        .map(|metric| (metric.stage, metric))
        .collect();
    if stage_metrics.len() != 4
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
            metric.item_count != context.decisions.len() as u64
                || metric.duration_ms != summary.compression_duration_ms
        })
    {
        return Err(StoreError::InvalidRequest(
            "context summary disagrees with stage metrics".to_string(),
        ));
    }
    let stage_duration = stage_metrics.iter().try_fold(0_u64, |total, metric| {
        total
            .checked_add(metric.duration_ms)
            .ok_or_else(|| StoreError::InvalidRequest("stage durations overflow".to_string()))
    })?;
    if summary.total_duration_ms < stage_duration {
        return Err(StoreError::InvalidRequest(
            "total duration is shorter than its stages".to_string(),
        ));
    }

    let mut expected_extensions: std::collections::BTreeMap<&str, (u64, u64, u64, u64, u64)> =
        std::collections::BTreeMap::new();
    for (index, (identity, item)) in inventory.iter().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        let result = file_results.get(identity).ok_or_else(|| {
            StoreError::InvalidRequest("extension metric file result is missing".to_string())
        })?;
        let values = expected_extensions
            .entry(item.file_type.as_str())
            .or_insert((0, 0, 0, 0, 0));
        values.0 = values.0.checked_add(1).ok_or_else(|| {
            StoreError::InvalidRequest("extension file count overflows".to_string())
        })?;
        values.1 = values
            .1
            .checked_add(result.parse_duration_ms)
            .ok_or_else(|| {
                StoreError::InvalidRequest("extension duration overflows".to_string())
            })?;
        match result.parse_status {
            ParseStatus::Success => values.2 += 1,
            // spec Part 2.2: extension `error_count` counts ONLY Error; NotParsed
            // is derived as file_count - success - error - timeout.
            ParseStatus::Error => values.3 += 1,
            ParseStatus::Timeout => values.4 += 1,
            ParseStatus::NotParsed => {}
        }
    }
    if extension_metrics.len() != expected_extensions.len()
        || extension_metrics.iter().any(|metric| {
            expected_extensions
                .get(metric.extension.as_str())
                .is_none_or(|expected| {
                    *expected
                        != (
                            metric.file_count,
                            metric.parse_duration_ms,
                            metric.success_count,
                            metric.error_count,
                            metric.timeout_count,
                        )
                })
        })
    {
        return Err(StoreError::InvalidRequest(
            "context summary disagrees with extension metrics".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn validate_context(
    context: &ContextRunRecord,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    terminal_status_text(context.status)?;
    if context.decisions.len() > 1_000_000 {
        return Err(StoreError::InvalidRequest(
            "context decision count exceeds the v1 limit".to_string(),
        ));
    }
    context
        .summary
        .validate()
        .map_err(StoreError::InvalidRequest)?;
    if !inventory::is_sha256(&context.context_profile_hash)
        || !inventory::is_sha256(&context.context_sha256)
        || cache::sha256_hex(context.final_context.as_bytes()) != context.context_sha256
        || !summary_fits_sqlite(&context.summary)
    {
        return Err(StoreError::InvalidRequest(
            "context fingerprint is invalid".to_string(),
        ));
    }
    let mut identities = std::collections::HashSet::new();
    for (index, record) in context.decisions.iter().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        record
            .decision
            .validate()
            .map_err(StoreError::InvalidRequest)?;
        if record.file_identity.is_empty() || !identities.insert(&record.file_identity) {
            return Err(StoreError::InvalidRequest(
                "context decision identity is invalid or duplicate".to_string(),
            ));
        }
        if [
            record.decision.priority,
            record.decision.input_chars,
            record.decision.output_chars,
        ]
        .into_iter()
        .any(|value| value > i64::MAX as u64)
        {
            return Err(StoreError::InvalidRequest(
                "context decision exceeds SQLite integer range".to_string(),
            ));
        }
    }
    Ok(())
}

pub(super) fn summary_fits_sqlite(summary: &ContextSummary) -> bool {
    [
        summary.source_file_count,
        summary.success_count,
        summary.timeout_count,
        summary.included_file_count,
        summary.omitted_file_count,
        summary.error_file_count,
        summary.input_chars,
        summary.output_chars,
        summary.total_duration_ms,
        summary.discovery_duration_ms,
        summary.parse_duration_ms,
        summary.compression_duration_ms,
    ]
    .into_iter()
    .all(|value| value <= i64::MAX as u64)
}

pub(super) fn final_status_matches(status: RunStatus, engine_status: EngineStatus) -> bool {
    matches!(
        (status, engine_status),
        (RunStatus::Success, EngineStatus::Ok)
            | (RunStatus::Partial, EngineStatus::Partial)
            | (RunStatus::Error, EngineStatus::Error)
    )
}

pub(super) fn terminal_status_text(status: RunStatus) -> Result<&'static str, StoreError> {
    match status {
        RunStatus::Success => Ok("success"),
        RunStatus::Partial => Ok("partial"),
        RunStatus::Error => Ok("error"),
        RunStatus::Running | RunStatus::Abandoned => Err(StoreError::InvalidRequest(
            "final status must be success, partial, or error".to_string(),
        )),
    }
}

pub(super) fn insert_diagnostics(
    transaction: &rusqlite::Transaction<'_>,
    scan_run_id: i64,
    sequence_offset: usize,
    diagnostics: &[RunDiagnosticRecord],
) -> rusqlite::Result<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO run_diagnostics(
            scan_run_id, sequence, severity, error_code, message, retryable,
            stage, file_path, backend
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for (sequence, record) in diagnostics.iter().enumerate() {
        statement.execute(params![
            scan_run_id,
            sequence_offset.saturating_add(sequence) as i64,
            record.severity.as_str(),
            inventory::enum_text(&record.diagnostic.error_code),
            record.diagnostic.message,
            i64::from(record.diagnostic.retryable),
            inventory::enum_text(&record.diagnostic.stage),
            record.diagnostic.file_path.0,
            record.diagnostic.backend.0,
        ])?;
    }
    Ok(())
}

/// Persists the authoritative `execution_metrics` row for a terminal run.
pub(super) fn insert_execution_metrics(
    transaction: &rusqlite::Transaction<'_>,
    scan_run_id: i64,
    metrics: &ExecutionMetricsV2,
) -> Result<(), StoreError> {
    let all_hit = |value: &Option<bool>| value.map(i64::from);
    let c = |value: u64, field: &str| -> Result<i64, StoreError> { checked_i64(value, field) };
    transaction
        .execute(
            "INSERT INTO scan_execution_metrics(
                scan_run_id,
                discovery_observed_file_count, source_guard_content_hash_file_count,
                source_guard_unavailable_count, source_guard_bytes_read,
                candidate_file_count, admitted_file_count, classification_slot_count,
                confirmed_run_inspected_pages_total, unobserved_classification_attempt_count,
                nominal_charged_pages_total, extraction_slot_count, pdfplumber_invocations,
                snapshot_hit, parse_cache_lookup_count, classification_cache_lookup_count,
                parse_cache_all_hit, classification_cache_all_hit,
                stage_deadline_exhausted_count, session_restart_count, session_fallback_count,
                classify_attempt_count, parse_attempt_count, reserved_chars, rendered_chars,
                worker_handshake_ms, discovery_ms, snapshot_lookup_ms,
                current_run_audit_write_ms, terminal_precommit_ms,
                deadline_precommit_elapsed_ms, envelope_rebuild_ms,
                terminal_rows_written, peak_worker_rss_bytes
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26,
                       ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34)",
            params![
                scan_run_id,
                c(
                    metrics.discovery_observed_file_count,
                    "discovery_observed_file_count"
                )?,
                c(
                    metrics.source_guard_content_hash_file_count,
                    "source_guard_content_hash_file_count"
                )?,
                c(
                    metrics.source_guard_unavailable_count,
                    "source_guard_unavailable_count"
                )?,
                c(metrics.source_guard_bytes_read, "source_guard_bytes_read")?,
                c(metrics.candidate_file_count, "candidate_file_count")?,
                c(metrics.admitted_file_count, "admitted_file_count")?,
                c(
                    metrics.classification_slot_count,
                    "classification_slot_count"
                )?,
                c(
                    metrics.confirmed_run_inspected_pages_total,
                    "confirmed_run_inspected_pages_total"
                )?,
                c(
                    metrics.unobserved_classification_attempt_count,
                    "unobserved_classification_attempt_count"
                )?,
                c(
                    metrics.nominal_charged_pages_total,
                    "nominal_charged_pages_total"
                )?,
                c(metrics.extraction_slot_count, "extraction_slot_count")?,
                c(metrics.pdfplumber_invocations, "pdfplumber_invocations")?,
                i64::from(metrics.snapshot_hit),
                c(metrics.parse_cache_lookup_count, "parse_cache_lookup_count")?,
                c(
                    metrics.classification_cache_lookup_count,
                    "classification_cache_lookup_count"
                )?,
                all_hit(&metrics.parse_cache_all_hit.0),
                all_hit(&metrics.classification_cache_all_hit.0),
                c(
                    metrics.stage_deadline_exhausted_count,
                    "stage_deadline_exhausted_count"
                )?,
                c(metrics.session_restart_count, "session_restart_count")?,
                c(metrics.session_fallback_count, "session_fallback_count")?,
                c(metrics.classify_attempt_count, "classify_attempt_count")?,
                c(metrics.parse_attempt_count, "parse_attempt_count")?,
                c(metrics.reserved_chars, "reserved_chars")?,
                c(metrics.rendered_chars, "rendered_chars")?,
                c(metrics.worker_handshake_ms, "worker_handshake_ms")?,
                c(metrics.discovery_ms, "discovery_ms")?,
                c(metrics.snapshot_lookup_ms, "snapshot_lookup_ms")?,
                c(
                    metrics.current_run_audit_write_ms,
                    "current_run_audit_write_ms"
                )?,
                c(metrics.terminal_precommit_ms, "terminal_precommit_ms")?,
                c(
                    metrics.deadline_precommit_elapsed_ms,
                    "deadline_precommit_elapsed_ms"
                )?,
                c(metrics.envelope_rebuild_ms, "envelope_rebuild_ms")?,
                c(metrics.terminal_rows_written, "terminal_rows_written")?,
                metrics.peak_worker_rss_bytes.0.map(|value| value as i64),
            ],
        )
        .map_err(cache_write)?;
    Ok(())
}

// Arguments mirror one atomic context write plus its deadline hook.
#[allow(clippy::too_many_arguments)]
pub(super) fn insert_context(
    transaction: &rusqlite::Transaction<'_>,
    scan_run_id: i64,
    context: &ContextRunRecord,
    artifact_id: Option<i64>,
    snapshot_hit: bool,
    reused_from_context_run_id: Option<i64>,
    now_ms: i64,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    ensure_deadline()?;
    let summary = &context.summary;
    transaction
        .execute(
            "INSERT INTO context_runs(
            context_run_id, scan_run_id, context_profile_hash, status,
            final_context, context_sha256, source_file_count, success_count,
            timeout_count, included_file_count, omitted_file_count,
            error_file_count, input_chars, output_chars, total_duration_ms,
            discovery_duration_ms, parse_duration_ms, compression_duration_ms,
            created_at_ms, artifact_id, reused_from_context_run_id, snapshot_hit
         ) VALUES (
            ?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21
         )",
            params![
                scan_run_id,
                context.context_profile_hash,
                terminal_status_text(context.status)?,
                context.final_context,
                context.context_sha256,
                summary.source_file_count as i64,
                summary.success_count as i64,
                summary.timeout_count as i64,
                summary.included_file_count as i64,
                summary.omitted_file_count as i64,
                summary.error_file_count as i64,
                summary.input_chars as i64,
                summary.output_chars as i64,
                summary.total_duration_ms as i64,
                summary.discovery_duration_ms as i64,
                summary.parse_duration_ms as i64,
                summary.compression_duration_ms as i64,
                now_ms,
                artifact_id,
                reused_from_context_run_id,
                i64::from(snapshot_hit),
            ],
        )
        .map_err(cache_write)?;
    let mut statement = transaction
        .prepare_cached(
            "INSERT INTO context_decisions(
            context_run_id, file_identity, relative_path, action, reason,
            priority, input_chars, output_chars, truncated, error_code
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .map_err(cache_write)?;
    for records in context.decisions.chunks(TERMINAL_WRITE_BATCH_SIZE) {
        ensure_deadline()?;
        for record in records {
            statement
                .execute(params![
                    scan_run_id,
                    record.file_identity,
                    record.decision.relative_path,
                    inventory::enum_text(&record.decision.action),
                    record.decision.reason,
                    record.decision.priority as i64,
                    record.decision.input_chars as i64,
                    record.decision.output_chars as i64,
                    i64::from(record.decision.truncated),
                    record.decision.error_code,
                ])
                .map_err(cache_write)?;
        }
    }
    Ok(())
}
