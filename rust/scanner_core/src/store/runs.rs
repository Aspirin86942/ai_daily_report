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

mod finalization;
mod records;
#[cfg(test)]
pub(super) use records::compute_audit_size;
pub(super) use records::{
    compute_audit_size_with_check, compute_terminal_rows_written, insert_context,
    insert_diagnostics, insert_execution_metrics,
};
mod history;
pub(crate) use history::rebuild_envelope_from_metadata;
pub(super) use history::{
    ensure_request_hash, load_stored_envelope, load_stored_envelope_ref, query_existing_run,
};
mod validation;
pub(super) use validation::{
    terminal_status_text, validate_finalization, validate_finalization_with_check,
    verify_inventory_snapshot,
};
