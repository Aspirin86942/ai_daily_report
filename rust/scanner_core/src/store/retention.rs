use super::*;

const RETENTION_DELETE_BATCH_SIZE: usize = 64;

#[derive(Debug)]
pub(super) enum PreparedRetentionDelete {
    Artifacts(Vec<i64>),
    Runs(Vec<i64>),
}

#[derive(Debug, Default)]
pub(super) struct FinalizationRetentionPlan {
    pub(super) artifact_room: Vec<PreparedRetentionDelete>,
    terminal_run_batches: Vec<Vec<i64>>,
    new_artifact_size: Option<i64>,
}

#[derive(Debug)]
struct ArtifactRetentionState {
    artifact_id: i64,
    artifact_size_bytes: i64,
    created_at_ms: i64,
    remaining_references: usize,
    removed: bool,
}

#[derive(Debug)]
struct TerminalRetentionState {
    scan_run_id: i64,
    finished_at_ms: i64,
    audit_size_bytes: i64,
    removed: bool,
}

/// Prepares every retention victim before the terminal transaction (spec Part
/// 2.3). The simulation uses the frozen ordering and accounts for references
/// that become orphaned after a planned terminal-run delete.
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_finalization_retention_plan(
    connection: &Connection,
    current_run_id: i64,
    protected_runs: &HashSet<i64>,
    protected_artifacts: &HashSet<i64>,
    new_artifact_size: Option<i64>,
    now_ms: i64,
    audit_size: i64,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<FinalizationRetentionPlan, StoreError> {
    if audit_size > cache::TERMINAL_AUDIT_MAX_BYTES {
        return Err(StoreError::RunCorrupt(
            "current terminal audit exceeds the 2 GiB retention cap".to_string(),
        ));
    }
    if new_artifact_size.is_some_and(|size| size > cache::CONTEXT_ARTIFACTS_MAX_BYTES) {
        return Err(StoreError::RunCorrupt(
            "current artifact exceeds the 512 MiB context artifact cap".to_string(),
        ));
    }

    let mut artifacts = Vec::new();
    let mut artifact_indexes = HashMap::new();
    let mut artifact_bytes = 0_i64;
    {
        ensure_deadline()?;
        let mut statement = connection
            .prepare(
                "SELECT artifact_id, artifact_size_bytes, created_at_ms
                 FROM context_artifacts ORDER BY artifact_id ASC",
            )
            .map_err(cache_write)?;
        let mut rows = statement.query([]).map_err(cache_write)?;
        let mut index = 0_usize;
        loop {
            if index.is_multiple_of(TERMINAL_WRITE_BATCH_SIZE) {
                ensure_deadline()?;
            }
            let Some(row) = rows.next().map_err(cache_write)? else {
                break;
            };
            let artifact_id: i64 = row.get(0).map_err(cache_write)?;
            let artifact_size_bytes: i64 = row.get(1).map_err(cache_write)?;
            let created_at_ms: i64 = row.get(2).map_err(cache_write)?;
            artifact_bytes = artifact_bytes
                .checked_add(artifact_size_bytes)
                .ok_or_else(|| {
                    StoreError::RunCorrupt("artifact retention byte total overflowed".to_string())
                })?;
            artifact_indexes.insert(artifact_id, artifacts.len());
            artifacts.push(ArtifactRetentionState {
                artifact_id,
                artifact_size_bytes,
                created_at_ms,
                remaining_references: 0,
                removed: false,
            });
            index += 1;
        }
    }

    let mut run_artifacts: HashMap<i64, Vec<usize>> = HashMap::new();
    {
        ensure_deadline()?;
        let mut statement = connection
            .prepare(
                "SELECT scan_run_id, artifact_id FROM context_runs
                 WHERE artifact_id IS NOT NULL ORDER BY scan_run_id ASC, artifact_id ASC",
            )
            .map_err(cache_write)?;
        let mut rows = statement.query([]).map_err(cache_write)?;
        let mut index = 0_usize;
        loop {
            if index.is_multiple_of(TERMINAL_WRITE_BATCH_SIZE) {
                ensure_deadline()?;
            }
            let Some(row) = rows.next().map_err(cache_write)? else {
                break;
            };
            let scan_run_id: i64 = row.get(0).map_err(cache_write)?;
            let artifact_id: i64 = row.get(1).map_err(cache_write)?;
            let artifact_index = *artifact_indexes.get(&artifact_id).ok_or_else(|| {
                StoreError::RunCorrupt(
                    "context run references a missing artifact during retention planning"
                        .to_string(),
                )
            })?;
            artifacts[artifact_index].remaining_references = artifacts[artifact_index]
                .remaining_references
                .checked_add(1)
                .ok_or_else(|| {
                    StoreError::RunCorrupt("artifact reference count overflowed".to_string())
                })?;
            run_artifacts
                .entry(scan_run_id)
                .or_default()
                .push(artifact_index);
            index += 1;
        }
    }

    let mut terminal_runs = Vec::new();
    {
        ensure_deadline()?;
        let mut statement = connection
            .prepare(
                // Abandoned runs never committed a terminal payload, so their
                // nullable audit size is legitimately zero. They still count
                // toward the terminal-row cap. Other terminal states must
                // retain the committed size; do not hide corrupt NULL values.
                "SELECT scan_run_id, finished_at_ms,
                        CASE WHEN status='abandoned' THEN COALESCE(audit_size_bytes,0)
                             ELSE audit_size_bytes END
                 FROM scan_runs
                 WHERE status IN ('success', 'partial', 'error', 'abandoned')
                 ORDER BY finished_at_ms ASC, scan_run_id ASC",
            )
            .map_err(cache_write)?;
        let mut rows = statement.query([]).map_err(cache_write)?;
        let mut index = 0_usize;
        loop {
            if index.is_multiple_of(TERMINAL_WRITE_BATCH_SIZE) {
                ensure_deadline()?;
            }
            let Some(row) = rows.next().map_err(cache_write)? else {
                break;
            };
            let scan_run_id: i64 = row.get(0).map_err(cache_write)?;
            if scan_run_id == current_run_id {
                return Err(StoreError::RunCorrupt(
                    "active run became terminal before finalization".to_string(),
                ));
            }
            let finished_at_ms: Option<i64> = row.get(1).map_err(cache_write)?;
            terminal_runs.push(TerminalRetentionState {
                scan_run_id,
                finished_at_ms: finished_at_ms.ok_or_else(|| {
                    StoreError::RunCorrupt(
                        "terminal retention row has no finished timestamp".to_string(),
                    )
                })?,
                audit_size_bytes: row.get(2).map_err(cache_write)?,
                removed: false,
            });
            index += 1;
        }
    }

    let mut plan = FinalizationRetentionPlan {
        new_artifact_size,
        ..FinalizationRetentionPlan::default()
    };
    if let Some(new_size) = new_artifact_size {
        let mut orphan_heap = BinaryHeap::new();
        for (index, artifact) in artifacts.iter().enumerate() {
            if artifact.remaining_references == 0
                && !protected_artifacts.contains(&artifact.artifact_id)
            {
                orphan_heap.push(Reverse((
                    artifact.created_at_ms,
                    artifact.artifact_id,
                    index,
                )));
            }
        }
        let mut run_cursor = 0_usize;
        while artifact_bytes.saturating_add(new_size) > cache::CONTEXT_ARTIFACTS_MAX_BYTES {
            ensure_deadline()?;
            let orphan_batch = select_orphan_artifact_batch(
                &mut artifacts,
                &mut orphan_heap,
                protected_artifacts,
                &mut artifact_bytes,
            );
            if !orphan_batch.is_empty() {
                plan.artifact_room
                    .push(PreparedRetentionDelete::Artifacts(orphan_batch));
                continue;
            }

            let mut run_batch = Vec::new();
            while run_batch.len() < RETENTION_DELETE_BATCH_SIZE && run_cursor < terminal_runs.len()
            {
                let run_index = run_cursor;
                run_cursor += 1;
                if terminal_runs[run_index].removed
                    || protected_runs.contains(&terminal_runs[run_index].scan_run_id)
                {
                    continue;
                }
                let run_id = terminal_runs[run_index].scan_run_id;
                terminal_runs[run_index].removed = true;
                run_batch.push(run_id);
                if let Some(indexes) = run_artifacts.get(&run_id) {
                    for &artifact_index in indexes {
                        let artifact = &mut artifacts[artifact_index];
                        artifact.remaining_references = artifact
                            .remaining_references
                            .checked_sub(1)
                            .ok_or_else(|| {
                                StoreError::RunCorrupt(
                                    "artifact reference simulation underflowed".to_string(),
                                )
                            })?;
                        if artifact.remaining_references == 0
                            && !artifact.removed
                            && !protected_artifacts.contains(&artifact.artifact_id)
                        {
                            orphan_heap.push(Reverse((
                                artifact.created_at_ms,
                                artifact.artifact_id,
                                artifact_index,
                            )));
                        }
                    }
                }
            }
            if run_batch.is_empty() {
                return Err(StoreError::RunCorrupt(
                    "context artifact retention could not make room for the current artifact"
                        .to_string(),
                ));
            }
            plan.artifact_room
                .push(PreparedRetentionDelete::Runs(run_batch));
        }
    }

    let cutoff_ms = now_ms.saturating_sub(cache::TERMINAL_RUN_MAX_AGE_DAYS * 86_400_000);
    let mut terminal_batch = Vec::new();
    for run in &mut terminal_runs {
        if !run.removed
            && !protected_runs.contains(&run.scan_run_id)
            && run.finished_at_ms < cutoff_ms
        {
            run.removed = true;
            terminal_batch.push(run.scan_run_id);
            if terminal_batch.len() == RETENTION_DELETE_BATCH_SIZE {
                plan.terminal_run_batches
                    .push(std::mem::take(&mut terminal_batch));
            }
        }
    }
    if !terminal_batch.is_empty() {
        plan.terminal_run_batches.push(terminal_batch);
    }

    let mut terminal_count = 1_i64;
    let mut terminal_bytes = audit_size;
    for run in terminal_runs.iter().filter(|run| !run.removed) {
        terminal_count = terminal_count.checked_add(1).ok_or_else(|| {
            StoreError::RunCorrupt("terminal retention row count overflowed".to_string())
        })?;
        terminal_bytes = terminal_bytes
            .checked_add(run.audit_size_bytes)
            .ok_or_else(|| {
                StoreError::RunCorrupt("terminal retention byte total overflowed".to_string())
            })?;
    }

    let mut cap_batch = Vec::new();
    for run in &mut terminal_runs {
        if terminal_count <= cache::TERMINAL_RUN_MAX_COUNT
            && terminal_bytes <= cache::TERMINAL_AUDIT_MAX_BYTES
        {
            break;
        }
        if run.removed || protected_runs.contains(&run.scan_run_id) {
            continue;
        }
        run.removed = true;
        terminal_count = terminal_count.saturating_sub(1);
        terminal_bytes = terminal_bytes.saturating_sub(run.audit_size_bytes);
        cap_batch.push(run.scan_run_id);
        if cap_batch.len() == RETENTION_DELETE_BATCH_SIZE {
            plan.terminal_run_batches
                .push(std::mem::take(&mut cap_batch));
        }
    }
    if !cap_batch.is_empty() {
        plan.terminal_run_batches.push(cap_batch);
    }
    if terminal_count > cache::TERMINAL_RUN_MAX_COUNT
        || terminal_bytes > cache::TERMINAL_AUDIT_MAX_BYTES
    {
        return Err(StoreError::RunCorrupt(
            "terminal audit retention could not make room for the current record".to_string(),
        ));
    }
    Ok(plan)
}

fn select_orphan_artifact_batch(
    artifacts: &mut [ArtifactRetentionState],
    orphan_heap: &mut BinaryHeap<Reverse<(i64, i64, usize)>>,
    protected_artifacts: &HashSet<i64>,
    artifact_bytes: &mut i64,
) -> Vec<i64> {
    let mut batch = Vec::new();
    while batch.len() < RETENTION_DELETE_BATCH_SIZE {
        let Some(Reverse((_, _, index))) = orphan_heap.pop() else {
            break;
        };
        let artifact = &mut artifacts[index];
        if artifact.removed
            || artifact.remaining_references != 0
            || protected_artifacts.contains(&artifact.artifact_id)
        {
            continue;
        }
        artifact.removed = true;
        *artifact_bytes = artifact_bytes.saturating_sub(artifact.artifact_size_bytes);
        batch.push(artifact.artifact_id);
    }
    batch
}

pub(super) fn apply_artifact_retention_plan(
    transaction: &rusqlite::Transaction<'_>,
    plan: &FinalizationRetentionPlan,
    protected_runs: &HashSet<i64>,
    protected_artifacts: &HashSet<i64>,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for deletion in &plan.artifact_room {
        ensure_deadline()?;
        match deletion {
            PreparedRetentionDelete::Artifacts(ids) => {
                delete_prepared_artifacts(transaction, ids, protected_artifacts)?
            }
            PreparedRetentionDelete::Runs(ids) => {
                delete_prepared_terminal_runs(transaction, ids, protected_runs)?
            }
        }
    }
    if let Some(new_size) = plan.new_artifact_size {
        ensure_deadline()?;
        let current: i64 = transaction
            .query_row(
                "SELECT COALESCE(SUM(artifact_size_bytes), 0) FROM context_artifacts",
                [],
                |row| row.get(0),
            )
            .map_err(cache_write)?;
        if current.saturating_add(new_size) > cache::CONTEXT_ARTIFACTS_MAX_BYTES {
            return Err(StoreError::RunCorrupt(
                "prepared artifact retention plan no longer makes room".to_string(),
            ));
        }
    }
    Ok(())
}

pub(super) fn apply_terminal_retention_plan(
    transaction: &rusqlite::Transaction<'_>,
    plan: &FinalizationRetentionPlan,
    protected_runs: &HashSet<i64>,
    ensure_deadline: &dyn Fn() -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for ids in &plan.terminal_run_batches {
        ensure_deadline()?;
        delete_prepared_terminal_runs(transaction, ids, protected_runs)?;
    }
    ensure_deadline()?;
    let (count, total): (i64, i64) = transaction
        .query_row(
            "SELECT count(*), COALESCE(SUM(audit_size_bytes), 0) FROM scan_runs
             WHERE status IN ('success', 'partial', 'error', 'abandoned')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(cache_write)?;
    if count > cache::TERMINAL_RUN_MAX_COUNT || total > cache::TERMINAL_AUDIT_MAX_BYTES {
        return Err(StoreError::RunCorrupt(
            "prepared terminal retention plan no longer satisfies the hard caps".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn delete_prepared_artifacts(
    transaction: &rusqlite::Transaction<'_>,
    ids: &[i64],
    protected_artifacts: &HashSet<i64>,
) -> Result<(), StoreError> {
    if ids.is_empty() {
        return Ok(());
    }
    let id_placeholders = vec!["?"; ids.len()].join(",");
    let mut protected: Vec<i64> = protected_artifacts.iter().copied().collect();
    protected.sort_unstable();
    let protected_clause = if protected.is_empty() {
        String::new()
    } else {
        format!(
            " AND context_artifacts.artifact_id NOT IN ({})",
            vec!["?"; protected.len()].join(",")
        )
    };
    let sql = format!(
        "DELETE FROM context_artifacts
         WHERE context_artifacts.artifact_id IN ({id_placeholders})
           AND NOT EXISTS(
               SELECT 1 FROM context_runs
               WHERE context_runs.artifact_id = context_artifacts.artifact_id
           ){protected_clause}"
    );
    let parameters: Vec<i64> = ids.iter().copied().chain(protected).collect();
    let deleted = transaction
        .execute(&sql, rusqlite::params_from_iter(parameters))
        .map_err(cache_write)?;
    if deleted != ids.len() {
        return Err(StoreError::RunCorrupt(
            "prepared artifact retention victims changed before terminal apply".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn delete_prepared_terminal_runs(
    transaction: &rusqlite::Transaction<'_>,
    ids: &[i64],
    protected_runs: &HashSet<i64>,
) -> Result<(), StoreError> {
    if ids.is_empty() {
        return Ok(());
    }
    let id_placeholders = vec!["?"; ids.len()].join(",");
    let mut protected: Vec<i64> = protected_runs.iter().copied().collect();
    protected.sort_unstable();
    let protected_clause = if protected.is_empty() {
        String::new()
    } else {
        format!(
            " AND scan_runs.scan_run_id NOT IN ({})",
            vec!["?"; protected.len()].join(",")
        )
    };
    let sql = format!(
        "DELETE FROM scan_runs
         WHERE scan_runs.scan_run_id IN ({id_placeholders})
           AND status IN ('success', 'partial', 'error', 'abandoned')
           {protected_clause}"
    );
    let parameters: Vec<i64> = ids.iter().copied().chain(protected).collect();
    let deleted = transaction
        .execute(&sql, rusqlite::params_from_iter(parameters))
        .map_err(cache_write)?;
    if deleted != ids.len() {
        return Err(StoreError::RunCorrupt(
            "prepared terminal retention victims changed before terminal apply".to_string(),
        ));
    }
    Ok(())
}
