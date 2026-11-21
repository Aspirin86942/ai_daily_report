use super::*;

/// `scan_runs.audit_size_bytes` 的逻辑 payload 近似（spec Part 4）：覆盖 run 的
/// envelope/attempt/diagnostic/current file/decision/stage/extension/context-run
/// 元数据；不含其引用的 artifact、全局 inventory 或 cache。Store 在 insert 时
/// 计算，不接受 wire 输入。
/// Logical INSERT/UPDATE row count of the terminal transaction (spec Part 5.3
/// `terminal_rows_written`): current run/artifact/context/metric rows only;
/// retention deletions and cache-transaction rows are excluded.
pub(in crate::store) fn compute_terminal_rows_written(
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
pub(in crate::store) fn compute_audit_size(batch: &FinalizationBatch) -> i64 {
    compute_audit_size_with_check(batch, &|| Ok(())).unwrap_or(i64::MAX)
}

pub(in crate::store) fn compute_audit_size_with_check(
    batch: &FinalizationBatch,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<i64, StoreError> {
    let mut total = batch.envelope_json.len() as i64;
    if let Some(reasons) = batch
        .execution_metrics
        .as_ref()
        .and_then(|metrics| metrics.session_restart_reasons)
    {
        let bytes = serde_json::to_vec(&reasons)
            .map_err(|error| StoreError::InvalidRequest(error.to_string()))?;
        total = total.saturating_add(2 * (bytes.len() as i64 + 28));
    }
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
        if let Some(key) = record
            .pdf_classification
            .as_ref()
            .and_then(|item| item.pdf_open_failure_key.as_ref())
        {
            let bytes = serde_json::to_vec(&(&record.file_identity, key))
                .map_err(|error| StoreError::InvalidRequest(error.to_string()))?;
            total = total.saturating_add(bytes.len() as i64 * 2 + 64);
        }
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

pub(in crate::store) fn file_execution_audit_size(
    record: &FileResultRecord,
    snapshot_rows: bool,
) -> i64 {
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

pub(in crate::store) fn insert_diagnostics(
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
pub(in crate::store) fn insert_execution_metrics(
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
pub(in crate::store) fn insert_context(
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
