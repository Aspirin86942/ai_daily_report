use super::*;

impl ScannerStore {
    /// Spec Part 5.2/5.4 snapshot lookup. The SQL must select an eligible
    /// artifact AND at least one committed Success `context_runs` row
    /// referencing it; an orphan artifact (no source run) is NOT a hit. The
    /// `snapshot_key_json` is compared byte-for-byte (never trusts the hash
    /// alone). The source run is chosen by `(finished_at_ms DESC,
    /// context_run_id DESC)` from BEFORE the current transaction.
    pub fn snapshot_lookup(
        &self,
        key: &SnapshotKeyParts,
    ) -> Result<Option<SnapshotHit>, StoreError> {
        let row: Option<(i64, i64)> = self
            .connection
            .query_row(
                "SELECT a.artifact_id, r.context_run_id
                 FROM context_artifacts a
                 JOIN context_runs r ON r.artifact_id = a.artifact_id
                 JOIN scan_runs s ON s.scan_run_id = r.scan_run_id
                 WHERE a.snapshot_eligible = 1
                   AND a.snapshot_key_sha256 = ?1
                   AND a.snapshot_key_json = ?2
                   AND r.status = 'success'
                   AND s.status = 'success'
                 ORDER BY s.finished_at_ms DESC, r.context_run_id DESC
                 LIMIT 1",
                params![key.sha256, key.canonical_json],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(cache_open)?;
        Ok(row.map(|(artifact_id, source_context_run_id)| SnapshotHit {
            artifact_id,
            source_context_run_id,
        }))
    }

    /// Loads an artifact (parent + owned file/decision rows) for snapshot
    /// current-row rebuild and dedup comparison (spec Part 5.1 replay check).
    pub fn load_artifact(&self, artifact_id: i64) -> Result<ArtifactDraft, StoreError> {
        load_artifact_from_connection(&self.connection, artifact_id)
    }
}

pub fn canonical_envelope_json(envelope: &ContextEnvelope) -> Result<String, StoreError> {
    envelope.validate().map_err(StoreError::InvalidRequest)?;
    serde_json::to_string(envelope).map_err(|error| StoreError::InvalidRequest(error.to_string()))
}

/// Loads the artifact referenced by a run's `context_runs` row for replay
/// (spec Part 5.1). Error runs and runs without an artifact return `None`.
pub(super) fn load_artifact_for_replay(
    connection: &Connection,
    scan_run_id: i64,
) -> Result<Option<ArtifactDraft>, StoreError> {
    let artifact_id: Option<Option<i64>> = connection
        .query_row(
            "SELECT artifact_id FROM context_runs WHERE scan_run_id=?1",
            [scan_run_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(cache_open)?;
    match artifact_id {
        Some(Some(id)) => Ok(Some(load_artifact_from_connection(connection, id)?)),
        _ => Ok(None),
    }
}

pub(super) fn parse_contract_enum<T>(value: &str) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(serde_json::Value::String(value.to_string()))
        .map_err(|_| StoreError::RunCorrupt("persisted contract enum is invalid".to_string()))
}

/// `final_envelope_metadata_json`（spec Part 5.1）：只保存 request/engine/status/
/// warnings/error 等小字段 + summary（供幂等重放从 metadata+summary+artifact 重建
/// 完整 Envelope），`warnings` 恒为数组（绝不 null），`error` 可为 null；
/// `file_context` 不进入 metadata（正文在 artifact 只存一次）。形状必须与
/// `rebuild_envelope` 期望完全一致。
pub(super) fn envelope_metadata_json(
    envelope: &ContextEnvelope,
    refill: Option<&ai_daily_scanner_contract::RefillAudit>,
    batch: &FinalizationBatch,
) -> Result<String, StoreError> {
    let mut value = serde_json::json!({
        "contract": envelope.contract,
        "protocol_version": envelope.protocol_version,
        "request_id": envelope.request_id,
        "engine_version": envelope.engine_version,
        "engine_build": envelope.engine_build,
        "status": serde_json::to_value(envelope.status)
            .map_err(|error| StoreError::InvalidRequest(error.to_string()))?,
        "scan_run_id": envelope.scan_run_id.0,
        "context_run_id": envelope.context_run_id.0,
        "warnings": envelope.warnings,
        "error": envelope.error.0,
        "summary": envelope.summary,
    });
    if let Some(refill) = refill {
        refill.validate().map_err(StoreError::RunCorrupt)?;
        value["refill_audit"] = serde_json::to_value(refill)
            .map_err(|error| StoreError::InvalidRequest(error.to_string()))?;
    }
    let mut pages = std::collections::BTreeMap::new();
    for record in &batch.file_results {
        if let Some(audit) = record
            .pdf_classification
            .as_ref()
            .and_then(|item| item.page_audit.as_ref())
        {
            audit.validate().map_err(StoreError::RunCorrupt)?;
            pages.insert(&record.file_identity, audit);
        }
    }
    if !pages.is_empty() {
        value["pdf_page_audits"] = serde_json::to_value(pages)
            .map_err(|error| StoreError::InvalidRequest(error.to_string()))?;
    }
    serde_json::to_string(&value).map_err(|error| StoreError::InvalidRequest(error.to_string()))
}

/// spec Part 4: artifact 的 exact logical bytes —— 一次精确覆盖 parent
/// `context_artifacts` 的全部 UTF-8/canonical JSON/BLOB 列（final_context +
/// semantic_summary_json + context_sha256 + snapshot_eligible + created_at_ms +
/// last_accessed_bucket + 可选 snapshot key 两字段）与全部 owned file/decision row
/// 的每个 text 列（含 enum text）与 i64 列。child 不重复计费。
pub(super) fn artifact_size_bytes_with_check(
    final_context: &str,
    context_sha256: &str,
    semantic_summary_json: &str,
    snapshot_key: Option<&SnapshotKeyParts>,
    file_rows: &[ArtifactFileRow],
    decision_rows: &[ArtifactDecisionRow],
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<i64, StoreError> {
    // parent context_artifacts columns
    let mut size = 8  // snapshot_eligible (i64)
        + final_context.len() as i64
        + context_sha256.len() as i64
        + semantic_summary_json.len() as i64
        + 8  // created_at_ms (i64)
        + 10; // last_accessed_bucket TEXT（YYYY-MM-DD，固定 10 字节）
    if let Some(key) = snapshot_key {
        size += key.sha256.len() as i64 + key.canonical_json.len() as i64;
    }
    for (index, row) in file_rows.iter().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        size += 8; // artifact_id (i64)
        size += row.file_identity.len() as i64;
        size += row.relative_path.len() as i64;
        size += row.legacy_source_version.len() as i64;
        size += row
            .source_guard_kind
            .as_ref()
            .map_or(0, |value| value.len() as i64);
        size += row
            .source_guard_sha256
            .as_ref()
            .map_or(0, |value| value.len() as i64);
        size += row.parse_profile_hash.len() as i64;
        size += inventory::enum_text(&row.parse_status).len() as i64; // parse_status TEXT
        size += row.parser_backend.len() as i64;
        size += row.worker_lane.len() as i64;
        size += 8; // truncated (i64)
        size += row.content_sha256.len() as i64;
        if let Some(classifier) = &row.classifier {
            size += inventory::enum_text(&classifier.status).len() as i64; // classifier_status TEXT
            size += classifier.page_count.map_or(0, |_| 8); // nullable i64
            size += classifier.result_examined_pages.map_or(0, |_| 8); // nullable i64
            size += 8; // classifier_nominal_charged_pages (i64, NOT NULL)
            size += classifier.classifier_build.len() as i64;
            size += classifier.classifier_profile_hash.len() as i64;
        }
    }
    for (index, row) in decision_rows.iter().enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        size += 8; // artifact_id (i64)
        size += row.file_identity.len() as i64;
        size += row.relative_path.len() as i64;
        size += inventory::enum_text(&row.action).len() as i64; // action TEXT
        size += row.reason.len() as i64;
        size += 8; // priority (i64)
        size += 8; // input_chars (i64)
        size += 8; // output_chars (i64)
        size += 8; // truncated (i64)
        size += row.error_code.len() as i64;
    }
    Ok(size)
}

pub(super) fn semantic_summary_json_for(draft: &ArtifactDraft) -> Result<String, StoreError> {
    serde_json::to_string(&draft.semantic_summary)
        .map_err(|error| StoreError::InvalidRequest(error.to_string()))
}

/// Persists an `ArtifactDraft` (parent + owned file/decision rows) and returns
/// the new `artifact_id` (spec Part 5.1). Eligible artifacts carry the snapshot
/// key fields plus per-source-file rows; ineligible payload artifacts carry
/// neither.
pub(super) fn insert_artifact(
    transaction: &rusqlite::Transaction<'_>,
    draft: &ArtifactDraft,
    snapshot_key: Option<&SnapshotKeyParts>,
    semantic_summary_json: &str,
    artifact_size: i64,
    now_ms: i64,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<i64, StoreError> {
    ensure_deadline()?;
    let (key_sha256, key_json) = match snapshot_key {
        Some(key) => (Some(key.sha256.as_str()), Some(key.canonical_json.as_str())),
        None => (None, None),
    };
    let bucket = cache::date_bucket_for_ms(now_ms);
    transaction
        .execute(
            "INSERT INTO context_artifacts(
                snapshot_eligible, snapshot_key_sha256, snapshot_key_json,
                final_context, context_sha256, semantic_summary_json,
                artifact_size_bytes, created_at_ms, last_accessed_bucket
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                i64::from(snapshot_key.is_some()),
                key_sha256,
                key_json,
                draft.final_context,
                draft.context_sha256,
                semantic_summary_json,
                artifact_size,
                now_ms,
                bucket,
            ],
        )
        .map_err(cache_write)?;
    let artifact_id = transaction.last_insert_rowid();
    if draft.snapshot_eligible {
        let mut file_stmt = transaction.prepare_cached(
            "INSERT INTO context_artifact_files(
                artifact_id, file_identity, relative_path, source_version,
                source_guard_kind, source_guard_sha256, parse_profile_hash, parse_status,
                parser_backend, worker_lane, truncated, content_sha256,
                classifier_status, classifier_page_count, classifier_result_examined_pages,
                classifier_nominal_charged_pages, classifier_build, classifier_profile_hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        )
        .map_err(cache_write)?;
        for rows in draft.file_rows.chunks(TERMINAL_WRITE_BATCH_SIZE) {
            ensure_deadline()?;
            for row in rows {
                file_stmt
                    .execute(params![
                        artifact_id,
                        row.file_identity,
                        row.relative_path,
                        row.legacy_source_version,
                        row.source_guard_kind,
                        row.source_guard_sha256,
                        row.parse_profile_hash,
                        inventory::parse_status_text(row.parse_status),
                        row.parser_backend,
                        row.worker_lane,
                        i64::from(row.truncated),
                        row.content_sha256,
                        row.classifier
                            .as_ref()
                            .map(|classifier| inventory::enum_text(&classifier.status)),
                        row.classifier
                            .as_ref()
                            .and_then(|classifier| classifier.page_count.map(|value| value as i64)),
                        row.classifier.as_ref().and_then(|classifier| classifier
                            .result_examined_pages
                            .map(|value| value as i64)),
                        row.classifier
                            .as_ref()
                            .map(|classifier| classifier.nominal_charged_pages as i64),
                        row.classifier
                            .as_ref()
                            .map(|classifier| classifier.classifier_build.clone()),
                        row.classifier
                            .as_ref()
                            .map(|classifier| classifier.classifier_profile_hash.clone()),
                    ])
                    .map_err(cache_write)?;
            }
        }
        let mut decision_stmt = transaction
            .prepare_cached(
                "INSERT INTO context_artifact_decisions(
                artifact_id, file_identity, relative_path, action, reason,
                priority, input_chars, output_chars, truncated, error_code
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )
            .map_err(cache_write)?;
        for rows in draft.decision_rows.chunks(TERMINAL_WRITE_BATCH_SIZE) {
            ensure_deadline()?;
            for row in rows {
                decision_stmt
                    .execute(params![
                        artifact_id,
                        row.file_identity,
                        row.relative_path,
                        inventory::enum_text(&row.action),
                        row.reason,
                        row.priority as i64,
                        row.input_chars as i64,
                        row.output_chars as i64,
                        i64::from(row.truncated),
                        row.error_code,
                    ])
                    .map_err(cache_write)?;
            }
        }
    }
    Ok(artifact_id)
}

/// Reconstructs an `ArtifactDraft` from the persisted artifact rows
/// (spec Part 5.1 replay direction). The context hash is re-verified.
pub(super) fn load_artifact_from_connection(
    connection: &Connection,
    artifact_id: i64,
) -> Result<ArtifactDraft, StoreError> {
    load_artifact_from_connection_with_check(connection, artifact_id, &|| Ok(()))
}

pub(super) fn load_artifact_from_connection_with_check(
    connection: &Connection,
    artifact_id: i64,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<ArtifactDraft, StoreError> {
    ensure_deadline()?;
    let row: Option<(i64, String, String, String)> = connection
        .query_row(
            "SELECT snapshot_eligible, final_context, context_sha256, semantic_summary_json
             FROM context_artifacts WHERE artifact_id=?1",
            [artifact_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(cache_open)?;
    let Some((snapshot_eligible, final_context, context_sha256, semantic_summary_json)) = row
    else {
        return Err(StoreError::RunNotFound);
    };
    let semantic_summary: crate::artifact::SemanticSummary =
        serde_json::from_str(&semantic_summary_json).map_err(|error| {
            StoreError::RunCorrupt(format!("artifact semantic summary is invalid: {error}"))
        })?;

    let mut file_stmt = connection
        .prepare(
            "SELECT file_identity, relative_path, source_version,
                    source_guard_kind, source_guard_sha256, parse_profile_hash, parse_status,
                    parser_backend, worker_lane, truncated, content_sha256,
                    classifier_status, classifier_page_count, classifier_result_examined_pages,
                    classifier_nominal_charged_pages, classifier_build, classifier_profile_hash
             FROM context_artifact_files WHERE artifact_id=?1 ORDER BY file_identity",
        )
        .map_err(cache_open)?;
    let file_query = file_stmt
        .query_map([artifact_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)? != 0,
                row.get::<_, String>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<i64>>(12)?,
                row.get::<_, Option<i64>>(13)?,
                row.get::<_, Option<i64>>(14)?,
                row.get::<_, Option<String>>(15)?,
                row.get::<_, Option<String>>(16)?,
            ))
        })
        .map_err(cache_open)?;
    let mut file_rows = Vec::new();
    for (index, row) in file_query.enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        let (
            file_identity,
            relative_path,
            source_version,
            source_guard_kind,
            source_guard_sha256,
            parse_profile_hash,
            parse_status_text,
            parser_backend,
            worker_lane,
            truncated,
            content_sha256,
            classifier_status,
            classifier_page_count,
            classifier_examined,
            classifier_nominal,
            classifier_build,
            classifier_profile_hash,
        ) = row.map_err(cache_open)?;
        let parse_status: ParseStatus = parse_contract_enum(&parse_status_text)?;
        let classifier = match (classifier_status, classifier_build, classifier_profile_hash) {
            (Some(status_text), Some(build), Some(profile_hash)) => {
                let status = parse_contract_enum(&status_text)?;
                Some(crate::artifact::PdfClassificationProvenanceV1 {
                    status,
                    page_count: classifier_page_count.map(|value| value as u64),
                    result_examined_pages: classifier_examined.map(|value| value as u64),
                    nominal_charged_pages: classifier_nominal.unwrap_or(0) as u64,
                    classifier_build: build,
                    classifier_profile_hash: profile_hash,
                })
            }
            _ => None,
        };
        file_rows.push(ArtifactFileRow {
            file_identity,
            relative_path,
            legacy_source_version: source_version,
            source_guard_kind,
            source_guard_sha256,
            parse_profile_hash,
            parse_status,
            parser_backend,
            worker_lane,
            truncated,
            content_sha256,
            classifier,
        });
    }

    let mut decision_stmt = connection
        .prepare(
            "SELECT file_identity, relative_path, action, reason, priority,
                    input_chars, output_chars, truncated, error_code
             FROM context_artifact_decisions WHERE artifact_id=?1 ORDER BY file_identity",
        )
        .map_err(cache_open)?;
    let decision_query = decision_stmt
        .query_map([artifact_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)? != 0,
                row.get::<_, String>(8)?,
            ))
        })
        .map_err(cache_open)?;
    let mut decision_rows = Vec::new();
    for (index, row) in decision_query.enumerate() {
        if index % TERMINAL_WRITE_BATCH_SIZE == 0 {
            ensure_deadline()?;
        }
        let (
            file_identity,
            relative_path,
            action_text,
            reason,
            priority,
            input_chars,
            output_chars,
            truncated,
            error_code,
        ) = row.map_err(cache_open)?;
        let action: ContextAction = parse_contract_enum(&action_text)?;
        decision_rows.push(crate::artifact::ArtifactDecisionRow {
            file_identity,
            relative_path,
            action,
            reason,
            priority: priority as u64,
            input_chars: input_chars as u64,
            output_chars: output_chars as u64,
            truncated,
            error_code,
        });
    }

    if crate::artifact::sha256_hex(final_context.as_bytes()) != context_sha256 {
        return Err(StoreError::RunCorrupt(
            "artifact context hash is invalid".to_string(),
        ));
    }
    Ok(ArtifactDraft {
        snapshot_eligible: snapshot_eligible != 0,
        final_context,
        context_sha256,
        semantic_summary,
        file_rows,
        decision_rows,
    })
}

/// spec Part 5.1 eligible dedup: a recompute whose snapshot key already exists
/// reuses the stored artifact ONLY when `snapshot_key_json` is byte-exact and
/// the artifact is field-for-field identical; any difference is a non-retryable
/// `BUDGET_MODEL_MISMATCH`/store invariant (never overwrites the old artifact).
pub(super) fn dedup_artifact(
    transaction: &Connection,
    draft: &ArtifactDraft,
    key: &SnapshotKeyParts,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<Option<i64>, StoreError> {
    ensure_deadline()?;
    let existing_id: Option<i64> = transaction
        .query_row(
            "SELECT artifact_id FROM context_artifacts
             WHERE snapshot_eligible = 1 AND snapshot_key_sha256 = ?1
             LIMIT 1",
            [&key.sha256],
            |row| row.get(0),
        )
        .optional()
        .map_err(cache_write)?;
    let Some(existing_id) = existing_id else {
        return Ok(None);
    };
    let existing =
        load_artifact_from_connection_with_check(transaction, existing_id, ensure_deadline)?;
    ensure_deadline()?;
    let existing_key_json: Option<String> = transaction
        .query_row(
            "SELECT snapshot_key_json FROM context_artifacts WHERE artifact_id=?1",
            [existing_id],
            |row| row.get(0),
        )
        .map_err(cache_write)?;
    if existing_key_json.as_deref() == Some(key.canonical_json.as_str()) && existing == *draft {
        Ok(Some(existing_id))
    } else {
        Err(StoreError::ArtifactMismatch(format!(
            "snapshot key {} already exists with different artifact semantics",
            key.sha256
        )))
    }
}
