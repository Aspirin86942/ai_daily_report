use super::{
    metadata,
    reports::{daily_pointer, read_run},
    timestamp, RunRecord, Store,
};
use crate::{infra::json::read_frozen_report_json, reports::period, types::RunPhase, ReportError};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

fn table(kind: &str) -> Result<(&'static str, &'static str), ReportError> {
    match kind {
        "weekly" => Ok(("weekly_reports", "week_label")),
        "monthly" => Ok(("monthly_reports", "year_month")),
        _ => Err(ReportError::new("RUN_KIND_INVALID", "周月报类型无效")),
    }
}
fn key(kind: &str, label: &str) -> String {
    format!("current_period:{kind}:{label}")
}
fn current(conn: &Connection, kind: &str, label: &str) -> Result<Option<RunRecord>, ReportError> {
    let (table, column) = table(kind)?;
    let Some(id) = metadata(conn, &key(kind, label))? else {
        return Ok(None);
    };
    let run = read_run(conn, &id)?;
    let raw: Option<String> = conn
        .query_row(
            &format!("SELECT raw_json FROM {table} WHERE {column}=?"),
            [label],
            |row| row.get(0),
        )
        .optional()?;
    if run.run_kind != kind || run.phase != RunPhase::Published || run.report_json != raw {
        return Err(ReportError::new(
            "PUBLICATION_VERSION_CHANGED",
            "周月报数据库正文与正式版本不一致",
        ));
    }
    Ok(Some(run))
}

impl Store {
    pub fn current_period(
        &self,
        kind: &str,
        label: &str,
    ) -> Result<Option<RunRecord>, ReportError> {
        current(&self.connection()?, kind, label)
    }
    pub(crate) fn publication_pointer(
        &self,
        run: &RunRecord,
        period_label: Option<&str>,
    ) -> Result<Option<String>, ReportError> {
        let connection = self.connection()?;
        match period_label {
            Some(label) => Ok(current(&connection, &run.run_kind, label)?.map(|run| run.run_id)),
            None => daily_pointer(&connection, &run.target_date),
        }
    }
}

fn period_report(run: &RunRecord) -> Result<Value, ReportError> {
    read_frozen_report_json(
        run.report_json
            .as_deref()
            .ok_or_else(|| ReportError::new("REPORT_NOT_STAGED", "周月报正文缺失"))?,
    )
}

pub(super) fn mark_published(
    tx: &Connection,
    run: &RunRecord,
    now: DateTime<Utc>,
) -> Result<(), ReportError> {
    let report = period_report(run)?;
    let id = &run.run_id;
    let label = period::validate_identity(&run.run_kind, &report)?;
    if current(tx, &run.run_kind, &label)?.map(|r| r.run_id) != run.previous_run_id {
        return Err(ReportError::new(
            "PUBLICATION_VERSION_CHANGED",
            "正式周月报版本已经改变",
        ));
    }
    let (table, column) = table(&run.run_kind)?;
    let fields = period::fields(&run.run_kind)?;
    let mut columns = fields.join(",");
    columns.push_str(",raw_json,updated_at");
    let marks = vec!["?"; fields.len() + 2].join(",");
    let updates = columns
        .split(',')
        .filter(|name| *name != column)
        .map(|name| format!("{name}=excluded.{name}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut values: Vec<String> = fields
        .iter()
        .map(|name| report[*name].as_str().unwrap().to_owned())
        .collect();
    values.push(run.report_json.as_ref().unwrap().clone());
    values.push(timestamp(now));
    tx.execute(&format!("INSERT INTO {table}({columns}) VALUES ({marks}) ON CONFLICT({column}) DO UPDATE SET {updates}"),rusqlite::params_from_iter(values))?;
    tx.execute("INSERT INTO report_metadata(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key(&run.run_kind,&label),id])?;
    Ok(())
}
