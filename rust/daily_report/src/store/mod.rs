mod calls;
pub mod locking;
mod personal;
pub use calls::{CallCompletion, CallReservation};
mod maintenance;
mod periods;
pub(crate) use maintenance::file_sha;
mod reports;
pub use reports::StagedReport;

use crate::{
    snapshots::{canonical_json, reject_redirects, SnapshotRef},
    types::{RunContext, RunPhase},
    ReportError,
};
use chrono::{DateTime, Utc};
use locking::ReportLock;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

pub const SCHEMA: &str = include_str!("schema-v1.sql");

// A random revision also distinguishes divergent writes to two copies of the same backup.
pub(crate) fn search_revision_triggers() -> impl Iterator<Item = (String, String)> {
    ["daily_reports", "weekly_reports", "monthly_reports"]
        .into_iter()
        .flat_map(|table| {
            ["INSERT", "UPDATE", "DELETE"].into_iter().map(move |event| {
                let name = format!("search_revision_{table}_{}", event.to_ascii_lowercase());
                let sql = format!("CREATE TRIGGER {name} AFTER {event} ON {table} BEGIN UPDATE report_metadata SET value=lower(hex(randomblob(16))) WHERE key='search_revision_v1'; END");
                (name, sql)
            })
        })
}

impl From<rusqlite::Error> for ReportError {
    fn from(_error: rusqlite::Error) -> Self {
        Self::new("REPORT_DATABASE_FAILED", "报告数据库操作失败")
    }
}

pub fn timestamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub scope_id: String,
    pub target_date: String,
    pub timezone: String,
    pub run_kind: String,
    pub context_json: String,
    pub config_fingerprint: String,
    pub input_fingerprint: Option<String>,
    pub publication_fingerprint: Option<String>,
    pub generation_id: String,
    pub regeneration_reason: Option<String>,
    pub phase: RunPhase,
    pub status: RunStatus,
    pub quality: Option<String>,
    pub snapshot_path: Option<String>,
    pub snapshot_sha256: Option<String>,
    pub report_json: Option<String>,
    pub markdown: Option<String>,
    pub output_path: Option<String>,
    pub expected_sha256: Option<String>,
    pub previous_sha256: Option<String>,
    pub previous_run_id: Option<String>,
    pub error_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

pub(crate) fn query(
    conn: &Connection,
    sql: &str,
    values: impl rusqlite::Params,
) -> Result<Vec<Value>, ReportError> {
    let mut statement = conn.prepare(sql)?;
    let columns: Vec<_> = statement
        .column_names()
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let rows = statement.query_map(values, |row| {
        let mut object = serde_json::Map::new();
        for (index, name) in columns.iter().enumerate() {
            use rusqlite::types::ValueRef;
            let value = match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => json!(value),
                ValueRef::Real(value) => json!(value),
                ValueRef::Text(_) => Value::String(row.get(index)?),
                ValueRef::Blob(_) => {
                    return Err(rusqlite::Error::InvalidColumnType(
                        index,
                        name.clone(),
                        rusqlite::types::Type::Blob,
                    ))
                }
            };
            object.insert(name.clone(), value);
        }
        Ok(Value::Object(object))
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn schema_shape(conn: &Connection) -> Result<BTreeMap<String, (String, String)>, ReportError> {
    let mut statement =
        conn.prepare("SELECT name,type,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'")?;
    let rows = statement.query_map([], |row| row.try_into())?;
    let mut shape = BTreeMap::new();
    for row in rows {
        let (name, kind, sql): (String, String, String) = row?;
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(ReportError::new(
                "REPORT_SCHEMA_UNSUPPORTED",
                "数据库对象名称不在支持范围",
            ));
        }
        if !(matches!(kind.as_str(), "table" | "index")
            || kind == "trigger" && search_revision_triggers().any(|(_, expected)| sql == expected))
        {
            return Err(ReportError::new(
                "REPORT_SCHEMA_UNSUPPORTED",
                "数据库包含不支持的视图或触发器",
            ));
        }
        // A definition covers column order, constraints, foreign keys and index expressions.
        let definition = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        shape.insert(name, (kind, definition));
    }
    Ok(shape)
}

#[derive(Debug, Clone)]
pub struct Store {
    pub path: PathBuf,
}

impl Store {
    pub fn open(path: &Path, now: DateTime<Utc>) -> Result<Self, ReportError> {
        let _lock = ReportLock::acquire(path, "personal_work", Duration::from_secs(5))?;
        Self::open_locked(path, now, &_lock)
    }

    pub fn open_locked(
        path: &Path,
        now: DateTime<Utc>,
        _lock: &ReportLock,
    ) -> Result<Self, ReportError> {
        let path = std::path::absolute(path)?;
        reject_redirects(&path, "REPORT_PATH_REDIRECTED")?;
        let store = Self { path };
        let mut conn = store.connection()?;
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if count == 0 {
            transaction.execute_batch(SCHEMA)?;
            transaction.execute(
                "INSERT INTO report_schema_migrations(version,applied_at) VALUES (1,?)",
                [timestamp(now)],
            )?;
            transaction.execute("INSERT INTO report_metadata(key,value) VALUES ('scope_id','personal_work'),('timezone','Asia/Shanghai')", [])?;
        }
        let expected = Connection::open_in_memory()?;
        expected.execute_batch(SCHEMA)?;
        let shape = schema_shape(&transaction)?;
        let mut tracks_search = count == 0;
        let mut missing_triggers = Vec::new();
        for (name, sql) in search_revision_triggers() {
            if shape.contains_key(&name) {
                tracks_search = true;
                expected.execute_batch(&sql)?;
            } else {
                missing_triggers.push(sql);
            }
        }
        if shape != schema_shape(&expected)? {
            return Err(ReportError::new(
                "REPORT_SCHEMA_UNSUPPORTED",
                "报告库结构不符合当前版本，未自动改表",
            ));
        }
        let current_version: bool = transaction.query_row(
            "SELECT count(*)=1 AND min(version)=1 FROM report_schema_migrations",
            [],
            |row| row.get(0),
        )?;
        if !current_version {
            return Err(ReportError::new(
                "REPORT_SCHEMA_UNSUPPORTED",
                "报告库版本不支持",
            ));
        }
        for (key, expected) in [("scope_id", "personal_work"), ("timezone", "Asia/Shanghai")] {
            if metadata(&transaction, key)?.as_deref() != Some(expected) {
                return Err(ReportError::new(
                    "REPORT_SCOPE_MISMATCH",
                    "报告库业务范围不一致",
                ));
            }
        }
        // Retained releases require the exact old schema. Opening an unmarked historical
        // database must not install triggers that would prevent those releases reopening it.
        if tracks_search || transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM report_metadata WHERE key IN ('search_database_id_v1','search_revision_v1'))",
            [], |r| r.get::<_, bool>(0),
        )? {
            if !missing_triggers.is_empty() {
                for sql in missing_triggers {
                    transaction.execute_batch(&sql)?;
                }
                // Rows may have changed while one or more triggers were absent.
                transaction.execute("DELETE FROM report_metadata WHERE key='search_revision_v1'", [])?;
            }
            let existing_keys: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM report_metadata WHERE key IN ('search_database_id_v1','search_revision_v1')",
                [],
                |r| r.get(0),
            )?;
            if existing_keys < 2 {
                transaction.execute_batch(
                    "INSERT OR IGNORE INTO report_metadata(key,value) VALUES
                     ('search_database_id_v1',lower(hex(randomblob(16)))),
                     ('search_revision_v1',lower(hex(randomblob(16))));",
                )?;
            }
        }
        transaction.commit()?;
        Ok(store)
    }

    pub fn connection(&self) -> Result<Connection, ReportError> {
        reject_redirects(&self.path, "REPORT_PATH_REDIRECTED")?;
        let conn = Connection::open(&self.path)?;
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(conn)
    }

    pub fn create(
        &self,
        context: &RunContext,
        kind: &str,
        generation_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<RunRecord, ReportError> {
        self.create_run(context, kind, generation_id, reason, now, false)
    }

    pub fn create_personal(
        &self,
        context: &RunContext,
        kind: &str,
        generation_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<RunRecord, ReportError> {
        if !["report", "preview", "model_check"].contains(&kind) {
            return Err(ReportError::new("RUN_KIND_INVALID", "个人运行类型无效"));
        }
        self.create_run(context, kind, generation_id, reason, now, true)
    }

    fn create_run(
        &self,
        context: &RunContext,
        kind: &str,
        generation_id: &str,
        reason: Option<&str>,
        now: DateTime<Utc>,
        personal: bool,
    ) -> Result<RunRecord, ReportError> {
        context.validate()?;
        if !["report", "preview", "model_check", "weekly", "monthly"].contains(&kind) {
            return Err(ReportError::new("RUN_KIND_INVALID", "不支持的运行类型"));
        }
        let mut conn = self.connection()?;
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("INSERT INTO report_runs(run_id,scope_id,target_date,timezone,run_kind,context_json,config_fingerprint,generation_id,regeneration_reason,created_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            params![context.run_id,context.scope_id,context.target_date.to_string(),context.timezone,kind,
                String::from_utf8(canonical_json(&serde_json::to_value(context)?)?).unwrap(),context.config_fingerprint,generation_id,reason,timestamp(now),timestamp(now)])?;
        if personal {
            transaction.execute(
                "INSERT INTO report_metadata(key,value) VALUES(?,'5')",
                [format!("personal_run:{}", context.run_id)],
            )?;
        }
        transaction.commit()?;
        self.get(&context.run_id)
    }

    pub fn get(&self, id: &str) -> Result<RunRecord, ReportError> {
        reports::read_run(&self.connection()?, id)
    }

    pub fn list_runs(&self, date: Option<&str>, limit: u32) -> Result<Vec<Value>, ReportError> {
        query(&self.connection()?, "SELECT run_id,target_date,run_kind,phase,status,quality,error_code,created_at,updated_at FROM report_runs WHERE (? IS NULL OR target_date=?) ORDER BY created_at DESC,run_id LIMIT ?", params![date,date,limit.clamp(1,500)])
    }

    pub fn pending(&self, date: Option<&str>) -> Result<Vec<RunRecord>, ReportError> {
        query(&self.connection()?, "SELECT * FROM report_runs WHERE run_kind='report' AND phase IN ('prepared','generating','db_committed') AND status!='succeeded' AND COALESCE(error_code,'')!='RUN_SUPERSEDED' AND COALESCE(regeneration_reason,'') NOT LIKE 'coverage_publish:%' AND (? IS NULL OR target_date=?) ORDER BY CASE WHEN phase='db_committed' THEN 0 ELSE 1 END,created_at,run_id", params![date,date])?
            .into_iter().map(|value| serde_json::from_value(value).map_err(Into::into)).collect()
    }

    pub fn set_scanning(&self, id: &str, now: DateTime<Utc>) -> Result<(), ReportError> {
        let changed = self.connection()?.execute("UPDATE report_runs SET phase='scanning',updated_at=? WHERE run_id=? AND phase='created' AND status='running'", params![timestamp(now),id])?;
        changed_one(changed, "RUN_PHASE_INVALID")
    }

    pub fn prepare(
        &self,
        id: &str,
        snapshot: &SnapshotRef,
        fingerprint: &str,
        sources: &[Value],
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute("UPDATE report_runs SET phase='prepared',snapshot_path=?,snapshot_sha256=?,input_fingerprint=?,updated_at=? WHERE run_id=? AND phase IN ('created','scanning') AND status='running'", params![snapshot.path,snapshot.sha256,fingerprint,timestamp(now),id])?;
        changed_one(changed, "RUN_INPUT_ALREADY_FROZEN")?;
        reports::insert_sources(&tx, id, sources)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn freeze_personal_plan(
        &self,
        materials: &RunRecord,
        planned: &SnapshotRef,
        fingerprint: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        let changed = self.connection()?.execute(
            "UPDATE report_runs SET snapshot_path=?1,snapshot_sha256=?2,input_fingerprint=?3,updated_at=?4
             WHERE run_id=?5 AND phase='prepared' AND status='running'
               AND snapshot_path=run_id||'/personal-materials-'||?6||'.json'
               AND snapshot_sha256=?6 AND input_fingerprint=?7
               AND generation_id=?8 AND config_fingerprint=?9
               AND NOT EXISTS (SELECT 1 FROM report_model_calls WHERE generation_id=report_runs.generation_id)",
            params![planned.path, planned.sha256, fingerprint, timestamp(now), materials.run_id,
                    materials.snapshot_sha256, materials.input_fingerprint,
                    materials.generation_id, materials.config_fingerprint],
        )?;
        changed_one(changed, "RUN_INPUT_ALREADY_FROZEN")
    }

    pub fn resume(&self, id: &str, now: DateTime<Utc>) -> Result<RunRecord, ReportError> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute("UPDATE report_runs SET status='running',error_code=CASE WHEN error_code='OUTPUT_CONFLICT_CAPTURED' THEN error_code ELSE NULL END,finished_at=NULL,updated_at=? WHERE run_id=? AND phase IN ('prepared','generating','db_committed') AND status!='succeeded' AND COALESCE(error_code,'')!='RUN_SUPERSEDED'", params![timestamp(now),id])?;
        changed_one(changed, "RUN_NOT_RECOVERABLE")?;
        tx.execute("UPDATE report_model_calls SET status='unknown',error_code='CALL_RESULT_UNKNOWN',finished_at=? WHERE run_id=? AND status='reserved'", params![timestamp(now),id])?;
        tx.commit()?;
        self.get(id)
    }

    pub fn fail(
        &self,
        id: &str,
        code: &str,
        cancelled: bool,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        self.connection()?.execute("UPDATE report_runs SET status=?,error_code=?,updated_at=?,finished_at=? WHERE run_id=? AND status!='succeeded'", params![if cancelled {"cancelled"} else {"failed"},code,timestamp(now),timestamp(now),id])?;
        Ok(())
    }

    pub fn recover_interrupted(
        &self,
        now: DateTime<Utc>,
        _lock: &ReportLock,
    ) -> Result<(), ReportError> {
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE report_model_calls SET status='unknown',error_code='CALL_RESULT_UNKNOWN',finished_at=? WHERE status='reserved' AND EXISTS(SELECT 1 FROM report_metadata m WHERE m.key='personal_run:'||report_model_calls.run_id AND m.value='5')", [timestamp(now)])?;
        tx.execute("UPDATE report_runs SET status='failed',error_code='RUN_INTERRUPTED',updated_at=?,finished_at=? WHERE status='running' AND EXISTS(SELECT 1 FROM report_metadata m WHERE m.key='personal_run:'||report_runs.run_id AND m.value='5') AND (phase IN ('created','scanning') OR run_kind!='report')", params![timestamp(now),timestamp(now)])?;
        tx.commit()?;
        Ok(())
    }

    pub fn calls(&self, id: &str) -> Result<Vec<Value>, ReportError> {
        query(
            &self.connection()?,
            "SELECT * FROM report_model_calls WHERE run_id=? ORDER BY call_id",
            [id],
        )
    }

    pub fn successful_call(
        &self,
        generation_id: &str,
        request_fingerprint: &str,
    ) -> Result<Option<Value>, ReportError> {
        Ok(query(&self.connection()?, "SELECT * FROM report_model_calls WHERE generation_id=? AND request_fingerprint=? AND status='succeeded' ORDER BY call_id DESC LIMIT 1", params![generation_id,request_fingerprint])?.into_iter().next())
    }
}

fn metadata(conn: &Connection, key: &str) -> Result<Option<String>, ReportError> {
    Ok(conn
        .query_row(
            "SELECT value FROM report_metadata WHERE key=?",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}

fn changed_one(changed: usize, code: &'static str) -> Result<(), ReportError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(ReportError::new(code, "运行状态不允许此操作"))
    }
}
