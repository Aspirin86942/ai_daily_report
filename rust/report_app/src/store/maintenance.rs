use super::*;
use crate::snapshots::fingerprint;
use std::{fs, io::Read};

impl Store {
    /// SQLite creates a consistent compact copy, including committed WAL pages.
    pub fn backup(&self, destination: &Path) -> Result<String, ReportError> {
        reject_redirects(destination, "BACKUP_PATH_REDIRECTED")?;
        if destination.exists() {
            return Err(ReportError::new("BACKUP_ALREADY_EXISTS", "备份目标已存在"));
        }
        fs::create_dir_all(
            destination
                .parent()
                .ok_or_else(|| ReportError::new("BACKUP_PATH_INVALID", "备份路径缺少目录"))?,
        )?;
        let conn = self.connection()?;
        conn.execute(
            "VACUUM INTO ?",
            [destination
                .to_str()
                .ok_or_else(|| ReportError::new("BACKUP_PATH_INVALID", "备份路径无效"))?],
        )?;
        let backup =
            Connection::open_with_flags(destination, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let check: String = backup.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if check != "ok" || schema_shape(&backup)? != schema_shape(&conn)? {
            return Err(ReportError::new(
                "BACKUP_VERIFY_FAILED",
                "数据库备份验证失败",
            ));
        }
        drop(backup);
        file_sha(destination)
    }
    pub fn register_backfill_budget(
        &self,
        job: &str,
        baseline: i64,
        limit: u64,
    ) -> Result<(), ReportError> {
        if baseline < 0 || limit > 100000 || job.is_empty() {
            return Err(ReportError::new("BACKFILL_BUDGET_INVALID", "补跑预算无效"));
        }
        let key = format!("backfill_budget:{job}");
        let value = json!({"baseline_call_id":baseline,"calls_limit":limit});
        let mut conn = self.connection()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<String> = tx
            .query_row(
                "SELECT value FROM report_metadata WHERE key=?",
                [&key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(prior) = prior {
            if fingerprint(&serde_json::from_str(&prior)?)? != fingerprint(&value)? {
                return Err(ReportError::new(
                    "BACKFILL_BUDGET_CHANGED",
                    "补跑必须保留原始预算与消耗起点",
                ));
            }
        } else {
            tx.execute(
                "INSERT INTO report_metadata(key,value) VALUES(?,?)",
                params![key, value.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn calls_after(&self, baseline: i64) -> Result<u64, ReportError> {
        Ok(self.connection()?.query_row(
            "SELECT COUNT(*) FROM report_model_calls WHERE call_id>?",
            [baseline],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }
    pub fn last_call_id(&self) -> Result<i64, ReportError> {
        Ok(self.connection()?.query_row(
            "SELECT COALESCE(MAX(call_id),0) FROM report_model_calls",
            [],
            |r| r.get(0),
        )?)
    }
}
pub(crate) fn file_sha(path: &Path) -> Result<String, ReportError> {
    use sha2::{Digest, Sha256};
    reject_redirects(path, "MAINTENANCE_PATH_REDIRECTED")?;
    let mut file = fs::File::open(path)?;
    let mut buffer = [0u8; 65536];
    let mut hash = Sha256::new();
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
