use super::*;
use std::fs;

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
        let backup = crate::infra::sqlite::open_read_only(
            destination,
            Duration::from_secs(5),
            "BACKUP_PATH_REDIRECTED",
            "BACKUP_VERIFY_FAILED",
        )?;
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
    pub fn job_calls(&self, job: &str) -> Result<Vec<Value>, ReportError> {
        query(&self.connection()?,
            "SELECT call_id,run_id,status,error_code,usage_json,request_sha256,response_sha256 FROM report_model_calls WHERE COALESCE(json_extract(usage_json,'$.job_id'),json_extract(usage_json,'$.reserved_budget.job_id'))=? ORDER BY call_id", [job])
    }
}
pub(crate) fn file_sha(path: &Path) -> Result<String, ReportError> {
    crate::infra::fs::file_sha256(path, "MAINTENANCE_PATH_REDIRECTED")
}
