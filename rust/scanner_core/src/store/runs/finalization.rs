use super::*;

impl ScannerStore {
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
}
