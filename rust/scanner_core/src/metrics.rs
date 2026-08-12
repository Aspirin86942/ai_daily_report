//! Stable metric rows persisted for scanner evidence.

use ai_daily_scanner_contract::{ExtensionMetric, StageMetric, StageName, Validate};
use rusqlite::{params, Transaction};

pub(crate) fn validate_metrics(
    stages: &[StageMetric],
    extensions: &[ExtensionMetric],
) -> Result<(), String> {
    if stages.len() > 4 || extensions.len() > 256 {
        return Err("too many scanner metric rows".to_string());
    }
    let mut stage_names = std::collections::HashSet::new();
    for metric in stages {
        metric.validate()?;
        if metric.item_count > i64::MAX as u64
            || metric.duration_ms > i64::MAX as u64
            || !stage_names.insert(stage_text(metric.stage))
        {
            return Err("invalid or duplicate stage metric".to_string());
        }
    }
    let mut extension_names = std::collections::HashSet::new();
    for metric in extensions {
        metric.validate()?;
        let counts_fit = [
            metric.file_count,
            metric.parse_duration_ms,
            metric.success_count,
            metric.error_count,
            metric.timeout_count,
        ]
        .into_iter()
        .all(|value| value <= i64::MAX as u64);
        if !counts_fit || !extension_names.insert(metric.extension.as_str()) {
            return Err("invalid or duplicate extension metric".to_string());
        }
    }
    Ok(())
}

pub(crate) fn insert_stage_metrics(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    stages: &[StageMetric],
) -> rusqlite::Result<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO scan_stage_metrics(scan_run_id, stage, item_count, duration_ms)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for metric in stages {
        statement.execute(params![
            scan_run_id,
            stage_text(metric.stage),
            metric.item_count as i64,
            metric.duration_ms as i64,
        ])?;
    }
    Ok(())
}

pub(crate) fn insert_extension_metrics(
    transaction: &Transaction<'_>,
    scan_run_id: i64,
    extensions: &[ExtensionMetric],
) -> rusqlite::Result<()> {
    let mut statement = transaction.prepare_cached(
        "INSERT INTO scan_extension_metrics(
            scan_run_id, extension, file_count, parse_duration_ms,
            success_count, error_count, timeout_count
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for metric in extensions {
        statement.execute(params![
            scan_run_id,
            metric.extension,
            metric.file_count as i64,
            metric.parse_duration_ms as i64,
            metric.success_count as i64,
            metric.error_count as i64,
            metric.timeout_count as i64,
        ])?;
    }
    Ok(())
}

pub(crate) fn stage_text(stage: StageName) -> &'static str {
    match stage {
        StageName::Discovery => "discovery",
        StageName::Cache => "cache",
        StageName::Parse => "parse",
        StageName::Context => "context",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_stage_metrics_are_rejected_before_the_transaction() {
        let metric = StageMetric {
            stage: StageName::Parse,
            item_count: 1,
            duration_ms: 2,
        };
        assert!(validate_metrics(&[metric.clone(), metric], &[]).is_err());
    }
}
