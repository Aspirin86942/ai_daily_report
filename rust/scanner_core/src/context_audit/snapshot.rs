use super::*;

pub(crate) fn load_evidence_snapshot(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<EvidenceSnapshot, EvidenceLoadError> {
    type EvidenceRunRow = (String, String, String, String, Option<String>);
    let run_row: Option<EvidenceRunRow> = transaction
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
        .map_err(|error| EvidenceLoadError::before_status(EvidenceAuditError::Sql(error)))?;
    let (
        stored_request_id,
        canonical_request_json,
        request_hash,
        run_status_text,
        envelope_metadata_json,
    ) = run_row.ok_or_else(|| EvidenceLoadError::before_status(EvidenceAuditError::RunNotFound))?;
    if crate::store::cache::domain_hash(b"request-v1\0", canonical_request_json.as_bytes())
        != request_hash
    {
        return Err(EvidenceLoadError::before_status(
            EvidenceAuditError::RunCorrupt("stored logical request hash is invalid".to_string()),
        ));
    }
    let run_status = parse_run_status(&run_status_text).map_err(|message| {
        EvidenceLoadError::before_status(EvidenceAuditError::RunCorrupt(message))
    })?;

    load_run_evidence(
        transaction,
        scan_run_id,
        &stored_request_id,
        run_status,
        envelope_metadata_json.as_deref(),
    )
    .map_err(|error| EvidenceLoadError::after_status(error, run_status))
}

fn load_run_evidence(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    stored_request_id: &str,
    run_status: RunStatus,
    envelope_metadata_json: Option<&str>,
) -> Result<EvidenceSnapshot, EvidenceAuditError> {
    // spec Part 5.1: the full ContextEnvelope is REBUILT from the body-free
    // final_envelope_metadata_json + summary + artifact (never from a
    // body-carrying scan_runs JSON).
    let envelope = match (run_status, envelope_metadata_json) {
        (RunStatus::Running | RunStatus::Abandoned, None) => None,
        (RunStatus::Running | RunStatus::Abandoned, Some(_)) => {
            return Err(EvidenceAuditError::RunCorrupt(
                "nonterminal run has a final envelope".to_string(),
            ));
        }
        (_, None) => {
            return Err(EvidenceAuditError::RunCorrupt(
                "terminal run has no final envelope metadata".to_string(),
            ));
        }
        (_, Some(metadata_json)) => {
            let rebuilt = crate::store::rebuild_envelope_from_metadata(
                transaction,
                scan_run_id,
                metadata_json,
            )
            .map_err(|error| EvidenceAuditError::RunCorrupt(error.to_string()))?;
            let status_matches = matches!(
                (run_status, rebuilt.status),
                (RunStatus::Success, EngineStatus::Ok)
                    | (RunStatus::Partial, EngineStatus::Partial)
                    | (RunStatus::Error, EngineStatus::Error)
            );
            if !status_matches
                || rebuilt.request_id != stored_request_id
                || rebuilt.scan_run_id.0 != Some(scan_run_id as u64)
            {
                return Err(EvidenceAuditError::RunCorrupt(
                    "rebuilt envelope does not match its run".to_string(),
                ));
            }
            Some(rebuilt)
        }
    };
    let context = load_context_row(transaction, scan_run_id)?;
    let stage_metrics = load_stage_metrics(transaction, scan_run_id)?;
    let extension_metrics = load_extension_metrics(transaction, scan_run_id)?;
    let files = load_file_audits(transaction, scan_run_id)?;
    let persisted_decisions = load_context_decisions(transaction, scan_run_id)?;
    let (warnings, persisted_error) = load_run_diagnostics(transaction, scan_run_id)?;

    let (context_run_id, summary) = match (&context, envelope) {
        (Some(context), Some(envelope)) => {
            if context.status != run_status
                || envelope.context_run_id.0 != Some(context.context_run_id)
                || envelope.file_context != context.final_context
                || envelope.summary != context.summary
                || warnings != envelope.warnings
                // spec Part 2.3: an Error run legitimately carries an error
                // diagnostic (envelope.error == persisted Error row); any other
                // mismatch is corrupt.
                || persisted_error != envelope.error.0
            {
                return Err(EvidenceAuditError::RunCorrupt(
                    "context row and final envelope disagree".to_string(),
                ));
            }
            validate_relational_summary(
                context,
                &stage_metrics,
                &extension_metrics,
                &files,
                &persisted_decisions,
            )?;
            (Some(context.context_run_id), envelope.summary)
        }
        (None, Some(envelope)) if run_status == RunStatus::Error => {
            if envelope.context_run_id.0.is_some()
                || !files.is_empty()
                || !persisted_decisions.is_empty()
            {
                return Err(EvidenceAuditError::RunCorrupt(
                    "error envelope has unexpected context rows".to_string(),
                ));
            }
            if warnings != envelope.warnings || persisted_error != envelope.error.0 {
                return Err(EvidenceAuditError::RunCorrupt(
                    "run warnings and error envelope disagree".to_string(),
                ));
            }
            (None, envelope.summary)
        }
        (None, None) if matches!(run_status, RunStatus::Running | RunStatus::Abandoned) => {
            if !stage_metrics.is_empty()
                || !extension_metrics.is_empty()
                || !files.is_empty()
                || !persisted_decisions.is_empty()
                || !warnings.is_empty()
                || persisted_error.is_some()
            {
                return Err(EvidenceAuditError::RunCorrupt(
                    "nonterminal run exposes committed final rows".to_string(),
                ));
            }
            (None, ContextSummary::default())
        }
        _ => {
            return Err(EvidenceAuditError::RunCorrupt(
                "run status, envelope, and context rows disagree".to_string(),
            ));
        }
    };
    let decisions = persisted_decisions
        .into_iter()
        .map(|record| record.decision)
        .collect();

    let (artifact_id, reused_from_context_run_id, snapshot_hit) = match context.as_ref() {
        Some(_) => {
            let (artifact_id, reused_from, snapshot_hit) =
                load_context_run_provenance(transaction, scan_run_id)?;
            if artifact_id.is_none() && !matches!(run_status, RunStatus::Error) {
                return Err(EvidenceAuditError::RunCorrupt(
                    "success/partial context run has no artifact".to_string(),
                ));
            }
            (artifact_id, reused_from, snapshot_hit)
        }
        None => (None, None, false),
    };
    let sqlite_artifact_id = artifact_id
        .map(i64::try_from)
        .transpose()
        .map_err(|_| EvidenceAuditError::RunCorrupt("artifact_id exceeds i64".into()))?;
    let (reserved_chars, rendered_chars) = match sqlite_artifact_id {
        Some(id) => load_artifact_semantic_chars(transaction, id)?,
        None => (0, 0),
    };
    let files_v2 = if matches!(
        run_status,
        RunStatus::Success | RunStatus::Partial | RunStatus::Error
    ) {
        load_file_audits_v2(
            transaction,
            scan_run_id,
            sqlite_artifact_id,
            envelope_metadata_json,
        )?
    } else {
        Vec::new()
    };
    let stage_deadline_exhausted = load_stage_deadline_exhausted(transaction, scan_run_id)?;
    let execution_metrics =
        load_execution_metrics(transaction, scan_run_id, envelope_metadata_json)?;

    Ok(EvidenceSnapshot {
        context_run_id,
        run_status,
        summary,
        stage_metrics,
        extension_metrics,
        decisions,
        warnings,
        artifact_id,
        reused_from_context_run_id,
        snapshot_hit,
        reserved_chars,
        rendered_chars,
        stage_deadline_exhausted,
        files_v2,
        execution_metrics,
    })
}

/// Loads the authoritative `execution_metrics` row persisted at finalize.
/// Nonterminal and engine-error runs may have none.
fn load_execution_metrics(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    metadata_json: Option<&str>,
) -> Result<Option<ExecutionMetricsV2>, EvidenceAuditError> {
    let mut statement = transaction.prepare(
        "SELECT discovery_observed_file_count,
                    source_guard_content_hash_file_count, source_guard_unavailable_count,
                    source_guard_bytes_read, candidate_file_count, admitted_file_count,
                    classification_slot_count, confirmed_run_inspected_pages_total,
                    unobserved_classification_attempt_count, nominal_charged_pages_total,
                    extraction_slot_count, pdfplumber_invocations, snapshot_hit,
                    parse_cache_lookup_count, classification_cache_lookup_count,
                    parse_cache_all_hit, classification_cache_all_hit,
                    stage_deadline_exhausted_count, session_restart_count,
                    session_fallback_count, classify_attempt_count, parse_attempt_count,
                    reserved_chars, rendered_chars, worker_handshake_ms, discovery_ms,
                    snapshot_lookup_ms, current_run_audit_write_ms, terminal_precommit_ms,
                    deadline_precommit_elapsed_ms, envelope_rebuild_ms,
                    terminal_rows_written, peak_worker_rss_bytes
             FROM scan_execution_metrics WHERE scan_run_id=?1",
    )?;
    let mut rows = statement.query([scan_run_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let count = |column: &str| to_u64(row.get(column)?, column);
    let parse_bool = |value: i64| match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(EvidenceAuditError::RunCorrupt(
            "persisted metric bool is invalid".to_string(),
        )),
    };
    let refill_audit = match metadata_json {
        Some(text) => {
            let value: serde_json::Value = serde_json::from_str(text)
                .map_err(|_| EvidenceAuditError::RunCorrupt("invalid run metadata JSON".into()))?;
            value
                .get("refill_audit")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| EvidenceAuditError::RunCorrupt("invalid refill audit".into()))?
        }
        None => None,
    };
    let metrics = ExecutionMetricsV2 {
        discovery_observed_file_count: count("discovery_observed_file_count")?,
        refill_audit,
        source_guard_content_hash_file_count: count("source_guard_content_hash_file_count")?,
        source_guard_unavailable_count: count("source_guard_unavailable_count")?,
        source_guard_bytes_read: count("source_guard_bytes_read")?,
        candidate_file_count: count("candidate_file_count")?,
        admitted_file_count: count("admitted_file_count")?,
        classification_slot_count: count("classification_slot_count")?,
        confirmed_run_inspected_pages_total: count("confirmed_run_inspected_pages_total")?,
        unobserved_classification_attempt_count: count("unobserved_classification_attempt_count")?,
        nominal_charged_pages_total: count("nominal_charged_pages_total")?,
        extraction_slot_count: count("extraction_slot_count")?,
        pdfplumber_invocations: count("pdfplumber_invocations")?,
        snapshot_hit: parse_bool(row.get(12)?)?,
        parse_cache_lookup_count: count("parse_cache_lookup_count")?,
        classification_cache_lookup_count: count("classification_cache_lookup_count")?,
        parse_cache_all_hit: Nullable(row.get::<_, Option<i64>>(15)?.map(parse_bool).transpose()?),
        classification_cache_all_hit: Nullable(
            row.get::<_, Option<i64>>(16)?.map(parse_bool).transpose()?,
        ),
        stage_deadline_exhausted_count: count("stage_deadline_exhausted_count")?,
        session_restart_count: count("session_restart_count")?,
        session_fallback_count: count("session_fallback_count")?,
        classify_attempt_count: count("classify_attempt_count")?,
        parse_attempt_count: count("parse_attempt_count")?,
        reserved_chars: count("reserved_chars")?,
        rendered_chars: count("rendered_chars")?,
        worker_handshake_ms: count("worker_handshake_ms")?,
        discovery_ms: count("discovery_ms")?,
        snapshot_lookup_ms: count("snapshot_lookup_ms")?,
        current_run_audit_write_ms: count("current_run_audit_write_ms")?,
        terminal_precommit_ms: count("terminal_precommit_ms")?,
        deadline_precommit_elapsed_ms: count("deadline_precommit_elapsed_ms")?,
        envelope_rebuild_ms: count("envelope_rebuild_ms")?,
        terminal_rows_written: count("terminal_rows_written")?,
        peak_worker_rss_bytes: Nullable(
            row.get::<_, Option<i64>>(32)?
                .map(|value| to_u64(value, "peak_worker_rss_bytes"))
                .transpose()?,
        ),
    };
    metrics
        .validate()
        .map_err(|message| EvidenceAuditError::RunCorrupt(message.to_string()))?;
    Ok(Some(metrics))
}

/// `stage_deadline_exhausted_count` is 0 or 1 (spec Part 5.3): true exactly
/// when a `STAGE_DEADLINE_EXHAUSTED` diagnostic was persisted for this run.
fn load_stage_deadline_exhausted(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<bool, EvidenceAuditError> {
    let count: i64 = transaction.query_row(
        "SELECT count(*) FROM run_diagnostics
             WHERE scan_run_id=?1 AND error_code='STAGE_DEADLINE_EXHAUSTED'",
        [scan_run_id],
        |row| row.get(0),
    )?;
    if count > 1 {
        return Err(EvidenceAuditError::RunCorrupt(
            "run persisted multiple stage deadline triggers".to_string(),
        ));
    }
    Ok(count == 1)
}

/// Loads the current `context_runs` snapshot relationship columns:
/// `artifact_id`, `reused_from_context_run_id`, `snapshot_hit`.
fn load_context_run_provenance(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<(Option<u64>, Option<u64>, bool), EvidenceAuditError> {
    let row: Option<(Option<i64>, Option<i64>, i64)> = transaction
        .query_row(
            "SELECT artifact_id, reused_from_context_run_id, snapshot_hit
             FROM context_runs WHERE scan_run_id=?1",
            [scan_run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)?)),
        )
        .optional()?;
    let Some((artifact_id, reused_from, snapshot_hit)) = row else {
        return Ok((None, None, false));
    };
    let snapshot_hit = match snapshot_hit {
        0 => false,
        1 => true,
        _ => {
            return Err(EvidenceAuditError::RunCorrupt(
                "context run snapshot_hit is invalid".to_string(),
            ));
        }
    };
    if snapshot_hit && reused_from.is_none() {
        return Err(EvidenceAuditError::RunCorrupt(
            "snapshot hit context run has no reused_from source".to_string(),
        ));
    }
    if !snapshot_hit && reused_from.is_some() {
        return Err(EvidenceAuditError::RunCorrupt(
            "non-snapshot context run has a reused_from source".to_string(),
        ));
    }
    Ok((
        artifact_id.map(|value| value as u64),
        reused_from.map(|value| value as u64),
        snapshot_hit,
    ))
}

/// Loads `reserved_chars`/`rendered_chars` from the artifact's immutable
/// semantic summary JSON (spec Part 5.3 `execution_metrics`).
fn load_artifact_semantic_chars(
    transaction: &Transaction<'_>,
    artifact_id: i64,
) -> Result<(u64, u64), EvidenceAuditError> {
    let summary_json: String = transaction.query_row(
        "SELECT semantic_summary_json FROM context_artifacts WHERE artifact_id=?1",
        [artifact_id],
        |row| row.get(0),
    )?;
    let value: serde_json::Value = serde_json::from_str(&summary_json)
        .map_err(|error| EvidenceAuditError::RunCorrupt(error.to_string()))?;
    let reserved = value
        .get("reserved_chars")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            EvidenceAuditError::RunCorrupt("artifact summary is missing reserved_chars".to_string())
        })?;
    let rendered = value
        .get("rendered_chars")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            EvidenceAuditError::RunCorrupt("artifact summary is missing rendered_chars".to_string())
        })?;
    Ok((reserved, rendered))
}
