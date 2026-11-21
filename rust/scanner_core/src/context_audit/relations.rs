use super::*;
use crate::store::inventory::relations::RelationCounts;

fn corrupt(message: &str) -> EvidenceAuditError {
    EvidenceAuditError::RunCorrupt(message.to_owned())
}

fn load_rows<T>(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    sql: &str,
    decode: impl FnMut(&rusqlite::Row<'_>) -> Result<T, EvidenceAuditError>,
) -> Result<Vec<T>, EvidenceAuditError> {
    let mut statement = transaction.prepare(sql)?;
    let rows = statement.query([scan_run_id])?;
    rows.and_then(decode).collect()
}

fn validated<T: Validate>(value: T, message: &str) -> Result<T, EvidenceAuditError> {
    value.validate().map_err(|_| corrupt(message))?;
    Ok(value)
}

pub(super) fn load_context_row(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Option<PersistedContext>, EvidenceAuditError> {
    let mut statement = transaction.prepare(
        "SELECT context_run_id, context_profile_hash, status, final_context,
                    context_sha256, source_file_count, success_count, timeout_count,
                    included_file_count, omitted_file_count, error_file_count,
                    input_chars, output_chars, total_duration_ms,
                    discovery_duration_ms, parse_duration_ms, compression_duration_ms
             FROM context_runs WHERE scan_run_id=?1",
    )?;
    let mut rows = statement.query([scan_run_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let id: i64 = row.get(0)?;
    let final_context: String = row.get(3)?;
    let count = |column: &str| to_u64(row.get(column)?, column);
    let status =
        parse_run_status(&row.get::<_, String>(2)?).map_err(EvidenceAuditError::RunCorrupt)?;
    if id != scan_run_id
        || !is_sha256(&row.get::<_, String>(1)?)
        || !matches!(
            status,
            RunStatus::Success | RunStatus::Partial | RunStatus::Error
        )
        || sha256_hex(final_context.as_bytes()) != row.get::<_, String>(4)?
    {
        return Err(corrupt("context row identity or hash is invalid"));
    }
    let summary = ContextSummary {
        source_file_count: count("source_file_count")?,
        success_count: count("success_count")?,
        timeout_count: count("timeout_count")?,
        included_file_count: count("included_file_count")?,
        omitted_file_count: count("omitted_file_count")?,
        error_file_count: count("error_file_count")?,
        input_chars: count("input_chars")?,
        output_chars: count("output_chars")?,
        total_duration_ms: count("total_duration_ms")?,
        discovery_duration_ms: count("discovery_duration_ms")?,
        parse_duration_ms: count("parse_duration_ms")?,
        compression_duration_ms: count("compression_duration_ms")?,
    };
    let summary = validated(summary, "context summary is invalid")?;
    Ok(Some(PersistedContext {
        context_run_id: id as u64,
        status,
        final_context,
        summary,
    }))
}

pub(super) fn load_stage_metrics(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Vec<StageMetric>, EvidenceAuditError> {
    load_rows(
        transaction,
        scan_run_id,
        "SELECT stage, item_count, duration_ms FROM scan_stage_metrics
             WHERE scan_run_id=?1
             ORDER BY CASE stage WHEN 'discovery' THEN 1 WHEN 'cache' THEN 2
                                  WHEN 'parse' THEN 3 WHEN 'context' THEN 4 END",
        |row| {
            let metric = StageMetric {
                stage: parse_enum(&row.get::<_, String>(0)?, "stage metric")?,
                item_count: to_u64(row.get::<_, i64>(1)?, "stage item_count")?,
                duration_ms: to_u64(row.get::<_, i64>(2)?, "stage duration_ms")?,
            };
            validated(metric, "stage metric is invalid")
        },
    )
}

pub(super) fn load_extension_metrics(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Vec<ExtensionMetric>, EvidenceAuditError> {
    load_rows(
        transaction,
        scan_run_id,
        "SELECT extension, file_count, parse_duration_ms, success_count,
                    error_count, timeout_count
             FROM scan_extension_metrics WHERE scan_run_id=?1 ORDER BY extension",
        |row| {
            let metric = ExtensionMetric {
                extension: row.get::<_, String>(0)?,
                file_count: to_u64(row.get::<_, i64>(1)?, "extension file_count")?,
                parse_duration_ms: to_u64(row.get::<_, i64>(2)?, "extension parse_duration_ms")?,
                success_count: to_u64(row.get::<_, i64>(3)?, "extension success_count")?,
                error_count: to_u64(row.get::<_, i64>(4)?, "extension error_count")?,
                timeout_count: to_u64(row.get::<_, i64>(5)?, "extension timeout_count")?,
            };
            validated(metric, "extension metric is invalid")
        },
    )
}

pub(super) fn load_file_audits(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Vec<FileAudit>, EvidenceAuditError> {
    load_rows(
        transaction,
        scan_run_id,
        "SELECT relative_path, file_identity, source_version, parse_status,
                    parser_backend, worker_lane, cache_status, cache_miss_reason,
                    truncated, content_sha256, parse_duration_ms, failure_class,
                    fallback_backend, fallback_reason_code
             FROM scan_file_results WHERE scan_run_id=?1
             ORDER BY lower(relative_path), relative_path, file_identity",
        |row| {
            // spec Part 5.3 v1 lossy projection: v2-only miss reasons are
            // remapped to the frozen v1 values (`parser_identity_changed` is a
            // lossless projection to `parser_profile_changed`;
            // `entry_absent_or_evicted` projects as `new_file` and the caller
            // adds the `CACHE_MISS_REASON_PROJECTED_AS_NEW_FILE` warning).
            let stored_miss: String = row.get(7)?;
            let miss_reason = match stored_miss.as_str() {
                "parser_identity_changed" => "parser_profile_changed",
                "entry_absent_or_evicted" => "new_file",
                other => other,
            };
            let file = FileAudit {
                relative_path: row.get::<_, String>(0)?,
                file_identity: row.get::<_, String>(1)?,
                source_version: row.get::<_, String>(2)?,
                parse_status: parse_enum(&row.get::<_, String>(3)?, "file parse_status")?,
                parser_backend: row.get::<_, String>(4)?,
                worker_lane: parse_enum(&row.get::<_, String>(5)?, "file worker_lane")?,
                cache_status: parse_enum(&row.get::<_, String>(6)?, "file cache_status")?,
                cache_miss_reason: parse_enum(miss_reason, "file cache_miss_reason")?,
                truncated: parse_bool(row.get::<_, i64>(8)?, "file truncated")?,
                content_sha256: row.get::<_, String>(9)?,
                parse_duration_ms: to_u64(row.get::<_, i64>(10)?, "file parse_duration_ms")?,
                failure_class: row.get::<_, String>(11)?,
                fallback_backend: row.get::<_, String>(12)?,
                fallback_reason_code: row.get::<_, String>(13)?,
            };
            validated(file, "file audit is invalid")
        },
    )
}

pub(super) fn load_context_decisions(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<Vec<PersistedDecision>, EvidenceAuditError> {
    load_rows(
        transaction,
        scan_run_id,
        "SELECT d.file_identity, d.relative_path, d.action, d.reason, d.priority,
                    d.input_chars, d.output_chars, d.truncated, d.error_code
             FROM context_decisions d
             JOIN context_runs c ON c.context_run_id=d.context_run_id
             WHERE c.scan_run_id=?1
             ORDER BY d.priority, lower(d.relative_path), d.relative_path, d.file_identity",
        |row| {
            let decision = ContextDecision {
                relative_path: row.get::<_, String>(1)?,
                action: parse_enum::<ContextAction>(&row.get::<_, String>(2)?, "context action")?,
                reason: row.get::<_, String>(3)?,
                priority: to_u64(row.get::<_, i64>(4)?, "context priority")?,
                input_chars: to_u64(row.get::<_, i64>(5)?, "context input_chars")?,
                output_chars: to_u64(row.get::<_, i64>(6)?, "context output_chars")?,
                truncated: parse_bool(row.get::<_, i64>(7)?, "context truncated")?,
                error_code: row.get::<_, String>(8)?,
            };
            let decision = validated(decision, "context decision is invalid")?;
            Ok(PersistedDecision {
                file_identity: row.get::<_, String>(0)?,
                decision,
            })
        },
    )
}

pub(super) fn load_run_diagnostics(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
) -> Result<(Vec<Diagnostic>, Option<Diagnostic>), EvidenceAuditError> {
    let mut statement = transaction.prepare(
        "SELECT severity, error_code, message, retryable, stage, file_path, backend
             FROM run_diagnostics WHERE scan_run_id=?1 ORDER BY sequence",
    )?;
    type DiagnosticColumns = (
        String,
        String,
        String,
        i64,
        String,
        Option<String>,
        Option<String>,
    );
    let raw = statement
        .query_map([scan_run_id], |row| DiagnosticColumns::try_from(row))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut warnings = Vec::new();
    let mut persisted_error = None;
    for (severity, code, message, retryable, stage, file_path, backend) in raw {
        if !matches!(severity.as_str(), "warning" | "error") {
            return Err(corrupt("run diagnostic severity is invalid"));
        }
        let diagnostic = Diagnostic {
            error_code: parse_enum(&code, "diagnostic error_code")?,
            message,
            retryable: parse_bool(retryable, "diagnostic retryable")?,
            stage: parse_enum(&stage, "diagnostic stage")?,
            file_path: Nullable(file_path),
            backend: Nullable(backend),
        };
        let diagnostic = validated(diagnostic, "run diagnostic is invalid")?;
        if severity == "warning" {
            warnings.push(diagnostic);
        } else if persisted_error.replace(diagnostic).is_some() {
            return Err(corrupt("run contains multiple terminal errors"));
        }
    }
    Ok((warnings, persisted_error))
}

pub(super) fn validate_relational_summary(
    context: &PersistedContext,
    stages: &[StageMetric],
    extensions: &[ExtensionMetric],
    files: &[FileAudit],
    decisions: &[PersistedDecision],
) -> Result<(), EvidenceAuditError> {
    let summary = &context.summary;
    let mut counts = RelationCounts::default();
    for file in files {
        counts.file(file.parse_status);
    }
    for record in decisions {
        counts.decision(&record.decision).map_err(corrupt)?;
    }
    let files_by_identity: HashMap<&str, &str> = files
        .iter()
        .map(|file| (file.file_identity.as_str(), file.relative_path.as_str()))
        .collect();
    if files_by_identity.len() != files.len()
        || decisions.iter().any(|record| {
            files_by_identity
                .get(record.file_identity.as_str())
                .is_none_or(|path| *path != record.decision.relative_path)
        })
    {
        return Err(corrupt(
            "context decisions do not match file identities and paths",
        ));
    }
    counts
        .validate(
            summary,
            &context.final_context,
            files.len(),
            decisions.len(),
            stages,
            "stage duration sum overflows",
        )
        .map_err(corrupt)?;
    let add = |total, value, field| {
        checked_add(total, value, field).map_err(EvidenceAuditError::RunCorrupt)
    };
    let mut extension_file_count = 0;
    let mut extension_success_count = 0;
    let mut extension_error_count = 0;
    let mut extension_timeout_count = 0;
    for metric in extensions {
        extension_file_count = add(
            extension_file_count,
            metric.file_count,
            "extension file count",
        )?;
        extension_success_count = add(
            extension_success_count,
            metric.success_count,
            "extension success count",
        )?;
        extension_error_count = add(
            extension_error_count,
            metric.error_count,
            "extension error count",
        )?;
        extension_timeout_count = add(
            extension_timeout_count,
            metric.timeout_count,
            "extension timeout count",
        )?;
    }
    if extension_file_count != summary.source_file_count
        || extension_success_count != summary.success_count
        || extension_error_count != summary.error_file_count
        || extension_timeout_count != summary.timeout_count
    {
        return Err(corrupt("context summary disagrees with extension metrics"));
    }
    Ok(())
}
