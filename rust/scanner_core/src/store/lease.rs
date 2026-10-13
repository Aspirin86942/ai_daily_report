use super::*;

pub(super) fn query_lease(connection: &Connection) -> Result<Option<ExistingLease>, StoreError> {
    connection
        .query_row(
            "SELECT owner_id, heartbeat_at_ms, expires_at_ms FROM engine_lease WHERE lease_key=1",
            [],
            |row| {
                Ok(ExistingLease {
                    owner_id: row.get(0)?,
                    heartbeat_at_ms: row.get(1)?,
                    expires_at_ms: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(cache_write)
}

pub(super) fn lease_is_live(lease: &ExistingLease, now_ms: i64) -> bool {
    let heartbeat_cutoff = now_ms.saturating_sub(LEASE_GRACE_MS as i64);
    lease.expires_at_ms > now_ms || lease.heartbeat_at_ms > heartbeat_cutoff
}

pub(super) fn reclaim_expired_lease(
    transaction: &rusqlite::Transaction<'_>,
    lease: &ExistingLease,
    now_ms: i64,
) -> Result<(), StoreError> {
    if lease_is_live(lease, now_ms) {
        return Err(StoreError::ScanAlreadyRunning);
    }
    transaction
        .execute(
            "UPDATE scan_runs
             SET status='abandoned', updated_at_ms=?1, finished_at_ms=?1
             WHERE owner_id=?2 AND status='running'",
            params![now_ms, lease.owner_id],
        )
        .map_err(cache_write)?;
    transaction
        .execute(
            "UPDATE scan_run_attempts
             SET status='abandoned', finished_at_ms=?1
             WHERE owner_id=?2 AND status='running'",
            params![now_ms, lease.owner_id],
        )
        .map_err(cache_write)?;
    let deleted = transaction
        .execute(
            "DELETE FROM engine_lease WHERE lease_key=1 AND owner_id=?1",
            [&lease.owner_id],
        )
        .map_err(cache_write)?;
    if deleted != 1 {
        return Err(StoreError::LeaseLost);
    }
    Ok(())
}

pub(super) fn random_owner_id(connection: &Connection) -> Result<String, StoreError> {
    let mut hex: Vec<char> = connection
        .query_row("SELECT lower(hex(randomblob(16)))", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(cache_write)?
        .chars()
        .collect();
    if hex.len() != 32 {
        return Err(StoreError::CacheWrite {
            detail: "SQLite random owner id had invalid length".to_string(),
        });
    }
    hex[12] = '4';
    hex[16] = '8';
    let compact: String = hex.into_iter().collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &compact[0..8],
        &compact[8..12],
        &compact[12..16],
        &compact[16..20],
        &compact[20..32]
    ))
}

pub(super) fn clear_staging_rows(
    transaction: &rusqlite::Transaction<'_>,
    scan_run_id: i64,
) -> Result<(), StoreError> {
    for sql in [
        "DELETE FROM context_runs WHERE scan_run_id=?1",
        "DELETE FROM scan_extension_metrics WHERE scan_run_id=?1",
        "DELETE FROM scan_stage_metrics WHERE scan_run_id=?1",
        "DELETE FROM scan_file_results WHERE scan_run_id=?1",
        "DELETE FROM run_diagnostics WHERE scan_run_id=?1",
    ] {
        transaction
            .execute(sql, [scan_run_id])
            .map_err(cache_write)?;
    }
    Ok(())
}

pub(super) fn ensure_owner(
    transaction: &rusqlite::Transaction<'_>,
    active: &ActiveRun,
) -> Result<(), StoreError> {
    let owned: bool = transaction
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM scan_runs r JOIN engine_lease l ON l.lease_key=1
                WHERE r.scan_run_id=?1 AND r.owner_id=?2 AND r.status='running'
                  AND l.owner_id=?2
             )",
            params![active.scan_run_id, active.owner_id],
            |row| row.get(0),
        )
        .map_err(cache_write)?;
    if owned {
        Ok(())
    } else {
        Err(StoreError::LeaseLost)
    }
}

pub(super) fn ensure_worker_fingerprints(
    transaction: &rusqlite::Transaction<'_>,
    active: &ActiveRun,
) -> Result<(), StoreError> {
    let complete: bool = transaction
        .query_row(
            "SELECT
                office_worker_contract IS NOT NULL
                AND office_worker_version IS NOT NULL
                AND office_worker_build IS NOT NULL
                AND python_worker_contract IS NOT NULL
                AND python_worker_version IS NOT NULL
                AND python_worker_build IS NOT NULL
             FROM scan_run_attempts
             WHERE scan_run_id=?1 AND attempt_number=?2 AND owner_id=?3 AND status='running'",
            params![active.scan_run_id, active.attempt_number, active.owner_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(cache_write)?
        .ok_or(StoreError::LeaseLost)?;
    if complete {
        Ok(())
    } else {
        Err(StoreError::RunCorrupt(
            "terminal attempt is missing required worker handshakes".to_string(),
        ))
    }
}

pub(super) fn ensure_engine_fingerprint(
    transaction: &rusqlite::Transaction<'_>,
    active: &ActiveRun,
    envelope: &ContextEnvelope,
) -> Result<(), StoreError> {
    let persisted: String = transaction
        .query_row(
            "SELECT engine_fingerprint FROM scan_run_attempts
             WHERE scan_run_id=?1 AND attempt_number=?2 AND owner_id=?3 AND status='running'",
            params![active.scan_run_id, active.attempt_number, active.owner_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(cache_write)?
        .ok_or(StoreError::LeaseLost)?;
    let fingerprint: EngineFingerprint = serde_json::from_str(&persisted)
        .map_err(|_| StoreError::RunCorrupt("attempt engine fingerprint is invalid".to_string()))?;
    if fingerprint.contract != envelope.contract
        || fingerprint.protocol_version != envelope.protocol_version
        || fingerprint.engine_version != envelope.engine_version
        || fingerprint.engine_build != envelope.engine_build
    {
        return Err(StoreError::RunCorrupt(
            "final envelope engine identity changed during the run".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn heartbeat_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    active: &ActiveRun,
    now_ms: i64,
) -> Result<(), StoreError> {
    ensure_owner(transaction, active)?;
    let expires_at_ms = lease_expiry(now_ms)?;
    let heartbeat_cutoff = now_ms.saturating_sub(LEASE_GRACE_MS as i64);
    let updated = transaction
        .execute(
            "UPDATE engine_lease
             SET heartbeat_at_ms=?1, expires_at_ms=?2
             WHERE lease_key=1 AND owner_id=?3
               AND (expires_at_ms > ?1 OR heartbeat_at_ms > ?4)",
            params![now_ms, expires_at_ms, active.owner_id, heartbeat_cutoff],
        )
        .map_err(cache_write)?;
    if updated != 1 {
        return Err(StoreError::LeaseLost);
    }
    transaction
        .execute(
            "UPDATE scan_runs SET updated_at_ms=?1
             WHERE scan_run_id=?2 AND owner_id=?3 AND status='running'",
            params![now_ms, active.scan_run_id, active.owner_id],
        )
        .map_err(cache_write)?;
    Ok(())
}

pub(super) fn lease_expiry(now_ms: i64) -> Result<i64, StoreError> {
    now_ms
        .checked_add(LEASE_GRACE_MS as i64)
        .ok_or_else(|| StoreError::InvalidRequest("lease timestamp overflows".to_string()))
}
