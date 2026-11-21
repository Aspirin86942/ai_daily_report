use super::*;
use inventory::relations::RelationCounts;

fn invalid_request(message: &str) -> StoreError {
    StoreError::InvalidRequest(message.to_owned())
}

fn check_batch_start(
    index: usize,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    if index.is_multiple_of(TERMINAL_WRITE_BATCH_SIZE) {
        ensure_deadline()?;
    }
    Ok(())
}

pub(in crate::store) fn validate_finalization(
    active: &ActiveRun,
    batch: &FinalizationBatch,
) -> Result<ContextEnvelope, StoreError> {
    validate_finalization_with_check(active, batch, &|| Ok(()))
}

pub(in crate::store) fn validate_finalization_with_check(
    active: &ActiveRun,
    batch: &FinalizationBatch,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<ContextEnvelope, StoreError> {
    terminal_status_text(batch.status)?;
    if batch.inventory.len() > 1_000_000
        || batch.file_results.len() > 1_000_000
        || batch.diagnostics.len() > 100_001
    {
        return Err(invalid_request(
            "finalization batch exceeds the v1 row limits",
        ));
    }
    let mut inventory_by_identity = std::collections::HashMap::new();
    for (index, record) in batch.inventory.iter().enumerate() {
        check_batch_start(index, ensure_deadline)?;
        record.validate().map_err(StoreError::InvalidRequest)?;
        if inventory_by_identity
            .insert(record.file_identity.as_str(), record)
            .is_some()
        {
            return Err(invalid_request("inventory identities must be unique"));
        }
    }
    let mut file_results_by_identity = std::collections::HashMap::new();
    for (index, record) in batch.file_results.iter().enumerate() {
        check_batch_start(index, ensure_deadline)?;
        record
            .validate_for_persistence(batch.snapshot_hit.is_some())
            .map_err(StoreError::InvalidRequest)?;
        let inventory = inventory_by_identity
            .get(record.file_identity.as_str())
            .ok_or_else(|| invalid_request("file result does not reference final inventory"))?;
        if inventory.source_version != record.source_version
            || file_results_by_identity
                .insert(record.file_identity.as_str(), record)
                .is_some()
        {
            return Err(invalid_request(
                "file results must uniquely match inventory source versions",
            ));
        }
    }
    for (index, record) in batch.diagnostics.iter().enumerate() {
        check_batch_start(index, ensure_deadline)?;
        record
            .diagnostic
            .validate()
            .map_err(StoreError::InvalidRequest)?;
    }
    crate::metrics::validate_metrics(&batch.stage_metrics, &batch.extension_metrics)
        .map_err(StoreError::InvalidRequest)?;

    ensure_deadline()?;
    let envelope: ContextEnvelope = serde_json::from_str(&batch.envelope_json)
        .map_err(|_| invalid_request("final envelope JSON is invalid"))?;
    envelope.validate().map_err(StoreError::InvalidRequest)?;
    if canonical_envelope_json(&envelope).as_deref() != Ok(batch.envelope_json.as_str()) {
        return Err(invalid_request(
            "final envelope JSON must use canonical serialization",
        ));
    }
    if envelope.request_id != active.request_id
        || envelope.scan_run_id.0 != Some(active.scan_run_id())
        || !final_status_matches(batch.status, envelope.status)
    {
        return Err(invalid_request(
            "final envelope does not match the active run",
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
        return Err(invalid_request(
            "final envelope diagnostics disagree with run diagnostics",
        ));
    }

    match (&batch.context, envelope.context_run_id.0) {
        (Some(context), Some(context_run_id)) if context_run_id == active.context_run_id() => {
            validate_context(context, ensure_deadline)?;
            let mut identities_match = context.decisions.len() == inventory_by_identity.len();
            for records in context.decisions.chunks(TERMINAL_WRITE_BATCH_SIZE) {
                ensure_deadline()?;
                identities_match &= records.iter().all(|record| {
                    inventory_by_identity.contains_key(record.file_identity.as_str())
                });
            }
            if context.status != batch.status
                || context.final_context != envelope.file_context
                || context.summary != envelope.summary
                || context.summary.source_file_count != batch.inventory.len() as u64
                || !identities_match
                || file_results_by_identity.len() != inventory_by_identity.len()
            {
                return Err(invalid_request(
                    "context rows disagree with the final envelope",
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
            return Err(invalid_request("context run identity is inconsistent"));
        }
    }
    Ok(envelope)
}

/// Rechecks that the independently committed inventory receipt still matches
/// this run's immutable discovery snapshot. This is an indexed invariant read,
/// never an inventory mutation inside the terminal transaction.
pub(in crate::store) fn verify_inventory_snapshot(
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
                <(_, _, _, _, _, _, _, _, _)>::try_from(row)
            })
            .optional()
            .map_err(cache_write)?;
        let Some(persisted) = persisted else {
            return Err(StoreError::RunCorrupt(
                "terminal inventory receipt is incomplete".to_string(),
            ));
        };
        let size_bytes = checked_i64(record.size_bytes, "inventory size")?;
        let mtime_ns = checked_i64(record.mtime_ns, "inventory mtime")?;
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

fn validate_context_relations(
    context: &ContextRunRecord,
    inventory: &std::collections::HashMap<&str, &InventoryRecord>,
    file_results: &std::collections::HashMap<&str, &FileResultRecord>,
    stage_metrics: &[StageMetric],
    extension_metrics: &[ExtensionMetric],
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let summary = &context.summary;
    let mut counts = RelationCounts::default();
    for (index, result) in file_results.values().enumerate() {
        check_batch_start(index, ensure_deadline)?;
        counts.file(result.parse_status);
    }
    for (index, record) in context.decisions.iter().enumerate() {
        check_batch_start(index, ensure_deadline)?;
        counts.decision(&record.decision).map_err(invalid_request)?;
        let item = inventory
            .get(record.file_identity.as_str())
            .ok_or_else(|| invalid_request("decision inventory is missing"))?;
        if item.relative_path != record.decision.relative_path {
            return Err(invalid_request("decision path disagrees with inventory"));
        }
    }
    counts
        .validate(
            summary,
            &context.final_context,
            file_results.len(),
            context.decisions.len(),
            stage_metrics,
            "stage durations overflow",
        )
        .map_err(invalid_request)?;

    let mut expected_extensions = std::collections::BTreeMap::new();
    for (index, (identity, item)) in inventory.iter().enumerate() {
        check_batch_start(index, ensure_deadline)?;
        let result = file_results
            .get(identity)
            .ok_or_else(|| invalid_request("extension metric file result is missing"))?;
        let metric = expected_extensions
            .entry(item.file_type.as_str())
            .or_insert_with(|| ExtensionMetric {
                extension: item.file_type.clone(),
                file_count: 0,
                parse_duration_ms: 0,
                success_count: 0,
                error_count: 0,
                timeout_count: 0,
            });
        metric.file_count = metric
            .file_count
            .checked_add(1)
            .ok_or_else(|| invalid_request("extension file count overflows"))?;
        metric.parse_duration_ms = metric
            .parse_duration_ms
            .checked_add(result.parse_duration_ms)
            .ok_or_else(|| invalid_request("extension duration overflows"))?;
        match result.parse_status {
            ParseStatus::Success => metric.success_count += 1,
            ParseStatus::Error => metric.error_count += 1,
            ParseStatus::Timeout => metric.timeout_count += 1,
            ParseStatus::NotParsed => {}
        }
    }
    if extension_metrics.len() != expected_extensions.len()
        || extension_metrics
            .iter()
            .any(|metric| expected_extensions.get(metric.extension.as_str()) != Some(metric))
    {
        return Err(invalid_request(
            "context summary disagrees with extension metrics",
        ));
    }
    Ok(())
}

fn validate_context(
    context: &ContextRunRecord,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    terminal_status_text(context.status)?;
    if context.decisions.len() > 1_000_000 {
        return Err(invalid_request(
            "context decision count exceeds the v1 limit",
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
        return Err(invalid_request("context fingerprint is invalid"));
    }
    let mut identities = std::collections::HashSet::new();
    for (index, record) in context.decisions.iter().enumerate() {
        check_batch_start(index, ensure_deadline)?;
        record
            .decision
            .validate()
            .map_err(StoreError::InvalidRequest)?;
        if record.file_identity.is_empty() || !identities.insert(&record.file_identity) {
            return Err(invalid_request(
                "context decision identity is invalid or duplicate",
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
            return Err(invalid_request(
                "context decision exceeds SQLite integer range",
            ));
        }
    }
    Ok(())
}

fn summary_fits_sqlite(summary: &ContextSummary) -> bool {
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

fn final_status_matches(status: RunStatus, engine_status: EngineStatus) -> bool {
    matches!(
        (status, engine_status),
        (RunStatus::Success, EngineStatus::Ok)
            | (RunStatus::Partial, EngineStatus::Partial)
            | (RunStatus::Error, EngineStatus::Error)
    )
}

pub(in crate::store) fn terminal_status_text(
    status: RunStatus,
) -> Result<&'static str, StoreError> {
    match status {
        RunStatus::Success => Ok("success"),
        RunStatus::Partial => Ok("partial"),
        RunStatus::Error => Ok("error"),
        RunStatus::Running | RunStatus::Abandoned => Err(invalid_request(
            "final status must be success, partial, or error",
        )),
    }
}
