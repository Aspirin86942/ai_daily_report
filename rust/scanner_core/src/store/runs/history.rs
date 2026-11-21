use super::*;

pub(in crate::store) fn query_existing_run(
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

pub(in crate::store) fn ensure_request_hash(
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

pub(in crate::store) fn load_stored_envelope(
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
pub(in crate::store) fn load_stored_envelope_ref(
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

pub(in crate::store) fn envelope_status_matches(
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
