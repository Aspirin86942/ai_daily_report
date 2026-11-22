use crate::{
    config::AppConfig,
    processing_cache::MAX_INPUT_BYTES,
    snapshots::{digest, reject_redirects, valid_run_id, SnapshotRef, SnapshotStore},
    store::locking::ReportLock,
    ReportError,
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, BufRead, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// Keep both daily-run locks until stdin closes or receives `release` on its own line.
pub fn guard(
    config: &AppConfig,
    previous_config_dir: Option<&Path>,
    previous_program_dir: Option<&Path>,
) -> Result<(), ReportError> {
    let locked = checked_lock(config, previous_config_dir, previous_program_dir)?;
    hold(
        io::stdin().lock(),
        io::stdout().lock(),
        &locked.target_sha256,
    )?;
    Ok(())
}

/// Identify the shared rollback target without locking it or reading the call ledger.
pub fn target_identity(
    config: &AppConfig,
    previous_config_dir: Option<&Path>,
    previous_program_dir: Option<&Path>,
) -> Result<String, ReportError> {
    verified_target(config, previous_config_dir, previous_program_dir)?.identity()
}

#[derive(PartialEq, Eq, Serialize)]
struct Target {
    scope_id: String,
    database: PathBuf,
    snapshots: PathBuf,
    reports: PathBuf,
}

impl Target {
    fn resolve(config: &AppConfig) -> Result<Self, ReportError> {
        Ok(Self {
            scope_id: config.report.scope_id.clone(),
            database: database_path(config)?,
            snapshots: directory_path(
                &config.paths.data_dir.join("daily_cache").join("snapshots"),
                "SNAPSHOT_PATH_REDIRECTED",
            )?,
            reports: directory_path(&config.paths.reports_dir, "OUTPUT_PATH_REDIRECTED")?,
        })
    }

    fn identity(&self) -> Result<String, ReportError> {
        Ok(digest(&serde_json::to_vec(&("rollback-target-v1", self))?))
    }

    fn processing_lock_path(&self) -> PathBuf {
        self.snapshots
            .parent()
            .expect("snapshot parent")
            .join("processing")
    }
}

struct HeldLocks {
    // Rust drops fields in declaration order, the reverse of acquisition here.
    _report: ReportLock,
    _processing: ReportLock,
    target_sha256: String,
}

fn checked_lock(
    config: &AppConfig,
    previous_config_dir: Option<&Path>,
    previous_program_dir: Option<&Path>,
) -> Result<HeldLocks, ReportError> {
    let target = verified_target(config, previous_config_dir, previous_program_dir)?;
    let target_sha256 = target.identity()?;
    let wait = Duration::from_secs(5);
    let processing = ReportLock::acquire(&target.processing_lock_path(), "personal", wait)?;
    let report = ReportLock::acquire(&target.database, &target.scope_id, wait)?;
    let connection =
        Connection::open_with_flags(&target.database, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| database_error())?;
    connection
        .busy_timeout(wait)
        .map_err(|_| database_error())?;
    check_pending(&connection, &target)?;
    Ok(HeldLocks {
        _report: report,
        _processing: processing,
        target_sha256,
    })
}

fn verified_target(
    config: &AppConfig,
    previous_config_dir: Option<&Path>,
    previous_program_dir: Option<&Path>,
) -> Result<Target, ReportError> {
    let target = Target::resolve(config)?;
    let previous = previous_config(config, previous_config_dir, previous_program_dir)?;
    if let Some(previous) = previous {
        if Target::resolve(&previous)? != target {
            return Err(ReportError::new(
                "ROLLBACK_CONFIG_MISMATCH",
                "回退前后的报告范围、报告库、快照根和发布根必须一致",
            ));
        }
    }
    Ok(target)
}

fn previous_config(
    config: &AppConfig,
    dir: Option<&Path>,
    program_dir: Option<&Path>,
) -> Result<Option<AppConfig>, ReportError> {
    let invalid = || ReportError::new("ROLLBACK_PREVIOUS_CONFIG_INVALID", "无法读取回退配置");
    let dir = dir
        .map(fs::canonicalize)
        .transpose()
        .map_err(|_| invalid())?;
    let program_dir = program_dir
        .map(fs::canonicalize)
        .transpose()
        .map_err(|_| invalid())?;
    let dir = dir.as_deref().unwrap_or(&config.config_dir);
    let program_dir = program_dir.as_deref().unwrap_or(&config.program_dir);
    if dir == config.config_dir && program_dir == config.program_dir {
        return Ok(None);
    }
    AppConfig::load(program_dir, Some(dir))
        .map(Some)
        .map_err(|_| invalid())
}

fn directory_path(path: &Path, redirect_code: &'static str) -> Result<PathBuf, ReportError> {
    let invalid = || ReportError::new("ROLLBACK_TARGET_PATH_INVALID", "无法确定回退目标目录");
    let check = |path: &Path| {
        reject_redirects(path, redirect_code).map_err(|error| {
            if error.code == redirect_code {
                error
            } else {
                invalid()
            }
        })
    };
    // Inspect the original spelling before removing dots so it cannot hide a link.
    check(path)?;
    let absolute = std::path::absolute(path).map_err(|_| invalid())?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    check(&normalized)?;
    for ancestor in normalized.ancestors() {
        match fs::metadata(ancestor) {
            Ok(info) if info.is_dir() => {
                let suffix = normalized.strip_prefix(ancestor).expect("directory suffix");
                let mut actual = fs::canonicalize(ancestor).map_err(|_| invalid())?;
                if !suffix.as_os_str().is_empty() {
                    actual.push(suffix);
                }
                return Ok(actual);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            _ => return Err(invalid()),
        }
    }
    Err(invalid())
}

fn database_path(config: &AppConfig) -> Result<PathBuf, ReportError> {
    let database = config.paths.db_dir.join("reports.sqlite3");
    reject_redirects(&database, "REPORT_PATH_REDIRECTED").map_err(|error| {
        if error.code == "REPORT_PATH_REDIRECTED" {
            error
        } else {
            database_error()
        }
    })?;
    let database = fs::canonicalize(database).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            ReportError::new("ROLLBACK_DB_MISSING", "回退检查需要已有的报告库")
        } else {
            database_error()
        }
    })?;
    if !database.is_file() {
        return Err(database_error());
    }
    Ok(database)
}

fn database_error() -> ReportError {
    ReportError::new("ROLLBACK_DB_READ_FAILED", "无法只读检查报告库")
}

// Startup diagnostic cleanup can remove an earlier pending run before recovery.
// Only a published current report excludes the date from model recovery.
const PENDING_WITH_UNRESOLVED_CALL: &str = "
SELECT r.run_id, r.snapshot_path, r.snapshot_sha256
FROM report_runs r
WHERE r.scope_id=?1 AND r.run_kind='report'
  AND r.phase IN ('prepared','generating') AND r.status!='succeeded'
  AND coalesce(r.error_code,'')!='RUN_SUPERSEDED'
  AND coalesce(r.regeneration_reason,'') NOT LIKE 'coverage_publish:%'
  AND EXISTS (SELECT 1 FROM report_metadata m WHERE m.key='personal_run:'||r.run_id AND m.value='5')
  AND NOT EXISTS (
      SELECT 1 FROM daily_reports d JOIN report_runs published ON published.run_id=d.current_run_id
      WHERE d.date=r.target_date AND published.target_date=r.target_date
        AND published.scope_id=r.scope_id AND published.phase='published' AND published.status='succeeded'
  )
  AND EXISTS (
      SELECT 1 FROM report_model_calls c
      WHERE c.generation_id=r.generation_id AND c.status IN ('unknown','reserved')
        AND NOT EXISTS (
            SELECT 1 FROM report_model_calls s
            WHERE s.generation_id=c.generation_id AND s.request_fingerprint=c.request_fingerprint
              AND s.status='succeeded'
        )
  )
ORDER BY r.target_date, r.created_at, r.run_id";

fn check_pending(connection: &Connection, target: &Target) -> Result<(), ReportError> {
    let mut query = connection
        .prepare(PENDING_WITH_UNRESOLVED_CALL)
        .map_err(|_| database_error())?;
    let mut rows = query
        .query([&target.scope_id])
        .map_err(|_| database_error())?;
    while let Some(row) = rows.next().map_err(|_| database_error())? {
        let id: String = row.get(0).map_err(|_| database_error())?;
        let path: Option<String> = row.get(1).map_err(|_| database_error())?;
        let sha256: Option<String> = row.get(2).map_err(|_| database_error())?;
        if let (Some(path), Some(sha256)) = (path, sha256) {
            let reference = SnapshotRef { path, sha256 };
            if v2_only(&target.snapshots, &id, &reference) {
                continue;
            }
        }
        let message = if valid_run_id(&id) {
            format!("未发布运行 {id} 有结果未知的模型调用")
        } else {
            "存在运行身份无效且模型调用结果未知的未发布记录".into()
        };
        return Err(ReportError::new("ROLLBACK_MODEL_OUTCOME_UNKNOWN", message));
    }
    Ok(())
}

fn v2_only(root: &Path, run_id: &str, reference: &SnapshotRef) -> bool {
    #[derive(Deserialize)]
    struct FrozenVersion {
        version: u32,
    }

    if !["personal-input", "personal-materials"]
        .iter()
        .any(|kind| reference.path == format!("{run_id}/{kind}-{}.json", reference.sha256))
    {
        return false;
    }
    let Ok(bytes) = SnapshotStore::retained(root.to_owned(), MAX_INPUT_BYTES)
        .and_then(|store| store.read_bytes(reference))
    else {
        return false;
    };
    // Phase 3 accepts frozen inputs 5 and 6. Its decoder rejects input 7 before model work.
    serde_json::from_slice::<FrozenVersion>(&bytes).is_ok_and(|input| input.version == 7)
}

fn hold(mut input: impl BufRead, mut output: impl Write, target_sha256: &str) -> io::Result<()> {
    writeln!(
        output,
        "{{\"ready\":true,\"target_sha256\":\"{target_sha256}\"}}"
    )?;
    output.flush()?;
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0
            || matches!(line.as_slice(), b"release\n" | b"release\r\n")
        {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use serde_json::json;
    use std::{collections::BTreeMap, io::Cursor};

    struct Fixture {
        root: tempfile::TempDir,
        config: AppConfig,
        database: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            for name in ["config", "work", "db"] {
                fs::create_dir(root.path().join(name)).unwrap();
            }
            let config_dir = root.path().join("config");
            fs::write(
                config_dir.join("settings.yaml"),
                r#"{"paths":{"work_dir":"work","db_dir":"db","data_dir":"data","reports_dir":"reports"}}"#,
            )
            .unwrap();
            let config =
                AppConfig::load_with_env(root.path(), Some(&config_dir), &BTreeMap::new()).unwrap();
            let database = root.path().join("db/reports.sqlite3");
            Self {
                root,
                config,
                database,
            }
        }

        fn initialize(&self) -> Connection {
            let connection = Connection::open(&self.database).unwrap();
            connection
                .execute_batch(include_str!("../store/schema-v1.sql"))
                .unwrap();
            connection
        }

        fn check(&self) -> Result<HeldLocks, ReportError> {
            checked_lock(&self.config, None, None)
        }

        fn identity(&self) -> String {
            target_identity(&self.config, None, None).unwrap()
        }
    }

    fn run(connection: &Connection, number: u32, date: &str) -> String {
        let id = format!("{number:032x}");
        connection.execute(
            "INSERT INTO report_runs(run_id,scope_id,target_date,timezone,context_json,config_fingerprint,
                 generation_id,phase,status,created_at,updated_at)
             VALUES(?1,'personal_work',?2,'Asia/Shanghai','{}','test',?1,'generating','failed',?3,?3)",
            params![id, date, format!("2026-11-21T00:{number:02}:00Z")],
        ).unwrap();
        connection
            .execute(
                "INSERT INTO report_metadata(key,value) VALUES('personal_run:'||?1,'5')",
                [&id],
            )
            .unwrap();
        id
    }

    fn call(connection: &Connection, id: &str, fingerprint: &str, status: &str) {
        connection.execute(
            "INSERT INTO report_model_calls(run_id,generation_id,request_fingerprint,logical_step,
                 attempt,call_date,status,started_at)
             VALUES(?1,?1,?2,'summary',1,'2026-11-21',?3,'2026-11-21T00:00:00Z')",
            params![id, fingerprint, status],
        ).unwrap();
    }

    fn blocked(fixture: &Fixture, id: &str) {
        let error = fixture.check().err().expect("rollback should be blocked");
        assert_eq!(error.code, "ROLLBACK_MODEL_OUTCOME_UNKNOWN");
        assert!(error.message.contains(id));
    }

    #[test]
    fn checks_later_unknown_runs_until_the_date_has_a_published_current_report() {
        let fixture = Fixture::new();
        let connection = fixture.initialize();
        let date = "2026-11-20";
        let risky = run(&connection, 2, date);
        call(&connection, &risky, "summary", "unknown");
        blocked(&fixture, &risky);

        run(&connection, 1, date);
        blocked(&fixture, &risky);

        let committed = run(&connection, 3, date);
        connection
            .execute(
                "UPDATE report_runs SET phase='db_committed' WHERE run_id=?",
                [&committed],
            )
            .unwrap();
        blocked(&fixture, &risky);

        connection.execute(
            "INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json,current_run_id)
             VALUES(?1,'','','','{}',?2)",
            params![date, committed],
        ).unwrap();
        blocked(&fixture, &risky);
        connection
            .execute(
                "UPDATE report_runs SET phase='published',status='succeeded' WHERE run_id=?",
                [&committed],
            )
            .unwrap();
        assert!(fixture.check().is_ok());
    }

    #[test]
    fn filters_historical_and_resolved_calls_without_masking_a_live_reservation() {
        let fixture = Fixture::new();
        let connection = fixture.initialize();
        let historical = run(&connection, 1, "2026-11-17");
        connection
            .execute_batch("UPDATE report_runs SET status='succeeded',phase='published';")
            .unwrap();
        connection.execute(
            "WITH RECURSIVE history(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM history WHERE n<1668)
             INSERT INTO report_model_calls(run_id,generation_id,request_fingerprint,logical_step,
                 attempt,call_date,status,started_at)
             SELECT ?1,?1,'history-'||n,'summary',1,'2026-11-17','unknown','2026-11-17T00:00:00Z'
             FROM history",
            [&historical],
        ).unwrap();
        let coverage = run(&connection, 2, "2026-11-18");
        connection
            .execute(
                "UPDATE report_runs SET regeneration_reason='coverage_publish:test' WHERE run_id=?",
                [&coverage],
            )
            .unwrap();
        call(&connection, &coverage, "coverage", "unknown");
        let unmarked = run(&connection, 3, "2026-11-19");
        connection
            .execute(
                "DELETE FROM report_metadata WHERE key='personal_run:'||?1",
                [&unmarked],
            )
            .unwrap();
        call(&connection, &unmarked, "unmarked", "unknown");
        let superseded = run(&connection, 5, "2026-11-16");
        connection.execute(
            "UPDATE report_runs SET status='cancelled',error_code='RUN_SUPERSEDED' WHERE run_id=?",
            [&superseded],
        ).unwrap();
        call(&connection, &superseded, "superseded", "unknown");
        assert!(fixture.check().is_ok());

        let active = run(&connection, 4, "2026-11-20");
        connection
            .execute(
                "UPDATE report_runs SET phase='prepared' WHERE run_id=?",
                [&active],
            )
            .unwrap();
        call(&connection, &active, "pending", "reserved");
        call(&connection, &active, "unrelated", "succeeded");
        call(&connection, &historical, "pending", "succeeded");
        blocked(&fixture, &active);
        call(&connection, &active, "pending", "succeeded");
        assert!(fixture.check().is_ok());
    }

    #[test]
    fn excludes_v2_only_input_using_verified_frozen_bytes() {
        let fixture = Fixture::new();
        let connection = fixture.initialize();
        let active = run(&connection, 1, "2026-11-20");
        call(&connection, &active, "pending", "unknown");
        let snapshots = SnapshotStore::new(
            fixture.config.paths.data_dir.join("daily_cache/snapshots"),
            MAX_INPUT_BYTES,
        )
        .unwrap();
        for version in [6, 7] {
            let reference = snapshots
                .put(
                    &active,
                    "personal-input",
                    &json!({"version":version,"plan":{"version":version+1}}),
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE report_runs SET snapshot_path=?1,snapshot_sha256=?2 WHERE run_id=?3",
                    params![reference.path, reference.sha256, active],
                )
                .unwrap();
            if version == 6 {
                blocked(&fixture, &active);
            } else {
                assert!(fixture.check().is_ok());
                fs::write(snapshots.root.join(reference.path), br#"{"version":7}"#).unwrap();
                blocked(&fixture, &active);
            }
        }
    }

    #[test]
    fn requires_database_and_resolves_relative_database_from_each_program() {
        let fixture = Fixture::new();
        assert_eq!(fixture.check().err().unwrap().code, "ROLLBACK_DB_MISSING");
        assert_eq!(
            fs::read_dir(fixture.database.parent().unwrap())
                .unwrap()
                .count(),
            0
        );
        let _connection = fixture.initialize();
        let previous_root = fixture.root.path().join("old-package");
        fs::create_dir_all(previous_root.join("db")).unwrap();
        let _old = Connection::open(previous_root.join("db/reports.sqlite3")).unwrap();
        let error = checked_lock(
            &fixture.config,
            Some(&fixture.config.config_dir),
            Some(&previous_root),
        )
        .err()
        .unwrap();
        assert_eq!(error.code, "ROLLBACK_CONFIG_MISMATCH");

        fs::write(
            fixture.config.config_dir.join("settings.yaml"),
            b"invalid: [",
        )
        .unwrap();
        assert!(checked_lock(
            &fixture.config,
            Some(&fixture.config.config_dir),
            Some(&fixture.config.program_dir),
        )
        .is_ok());
    }

    #[test]
    fn compares_missing_snapshot_and_publication_roots_for_the_same_database() {
        let fixture = Fixture::new();
        let _connection = fixture.initialize();
        let previous = fixture.root.path().join("previous-config");
        fs::create_dir(&previous).unwrap();
        let settings = previous.join("settings.yaml");
        for (data_dir, reports_dir) in [("other-data", "reports"), ("data", "other-reports")] {
            fs::write(
                &settings,
                json!({"paths": {
                    "work_dir": "work", "db_dir": "db", "data_dir": data_dir,
                    "reports_dir": reports_dir,
                }})
                .to_string(),
            )
            .unwrap();
            let identity = target_identity(&fixture.config, Some(&previous), None);
            assert_eq!(identity.unwrap_err().code, "ROLLBACK_CONFIG_MISMATCH");
        }

        fs::write(&settings, r#"{"paths":{"work_dir":"work","db_dir":"./db","data_dir":"./data","reports_dir":"./reports"}}"#).unwrap();
        let identity = fixture.identity();
        assert_eq!(
            target_identity(&fixture.config, Some(&previous), None).unwrap(),
            identity
        );
        let snapshots = fixture.config.paths.data_dir.join("daily_cache/snapshots");
        assert!(!snapshots.exists() && !fixture.config.paths.reports_dir.exists());
        fs::create_dir_all(snapshots).unwrap();
        fs::create_dir_all(&fixture.config.paths.reports_dir).unwrap();
        assert_eq!(fixture.identity(), identity);

        let mut detached = fixture.config.clone();
        detached.program_dir = fixture.root.path().join("missing-package");
        detached.config_dir = detached.program_dir.join("config");
        assert_eq!(target_identity(&detached, None, None).unwrap(), identity);
    }

    #[test]
    fn identity_query_ignores_unresolved_calls_and_held_locks() {
        let fixture = Fixture::new();
        let connection = fixture.initialize();
        let identity = fixture.identity();
        assert!(crate::snapshots::valid_hash(&identity));
        let active = run(&connection, 1, "2026-11-20");
        call(&connection, &active, "pending", "unknown");
        blocked(&fixture, &active);
        let target = Target::resolve(&fixture.config).unwrap();
        let _processing =
            ReportLock::acquire(&target.processing_lock_path(), "personal", Duration::ZERO)
                .unwrap();
        let _report =
            ReportLock::acquire(&target.database, &target.scope_id, Duration::ZERO).unwrap();
        assert_eq!(fixture.identity(), identity);
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn rejects_linked_ancestors_even_when_the_target_directory_is_missing() {
        let mut fixture = Fixture::new();
        let _connection = fixture.initialize();
        let real = fixture.root.path().join("real");
        let link = fixture.root.path().join("link");
        fs::create_dir(&real).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let result = std::process::Command::new("cmd")
                .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
                .args(["/D", "/C", "mklink", "/J"])
                .arg(&link)
                .arg(&real)
                .output()
                .unwrap();
            assert!(result.status.success(), "isolated junction creation failed");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();

        fixture.config.paths.reports_dir = link.join("absent");
        assert_eq!(
            target_identity(&fixture.config, None, None)
                .unwrap_err()
                .code,
            "OUTPUT_PATH_REDIRECTED"
        );
        fixture.config.paths.data_dir = link;
        assert_eq!(
            target_identity(&fixture.config, None, None)
                .unwrap_err()
                .code,
            "SNAPSHOT_PATH_REDIRECTED"
        );
        assert!(fs::read_dir(real).unwrap().next().is_none());
    }

    #[test]
    fn flushes_identity_while_holding_both_locks_then_releases_on_command_or_eof() {
        struct Probe<'a> {
            database: &'a Path,
            processing: &'a Path,
            identity: &'a str,
            bytes: Vec<u8>,
            flushed: bool,
        }

        impl Write for Probe<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.write(bytes)
            }

            fn flush(&mut self) -> io::Result<()> {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&self.bytes).unwrap(),
                    json!({"ready": true, "target_sha256": self.identity}),
                );
                assert_eq!(self.bytes.iter().filter(|&&byte| byte == b'\n').count(), 1);
                for (path, scope) in [
                    (self.database, "personal_work"),
                    (self.processing, "personal"),
                ] {
                    let error = ReportLock::acquire(path, scope, Duration::ZERO)
                        .err()
                        .expect("guard must still own both locks");
                    assert_eq!(error.code, "REPORT_ALREADY_RUNNING");
                }
                self.flushed = true;
                Ok(())
            }
        }

        let fixture = Fixture::new();
        let _connection = fixture.initialize();
        let before = fs::read(&fixture.database).unwrap();
        let processing = fixture.config.paths.data_dir.join("daily_cache/processing");
        let identity = fixture.identity();
        for (bytes, consumed) in [
            (b"ignored\r\nrelease\r\nafter".as_slice(), 18),
            (b"partial line".as_slice(), 12),
        ] {
            let mut cursor = Cursor::new(bytes);
            let mut output = Probe {
                database: &fixture.database,
                processing: &processing,
                identity: &identity,
                bytes: vec![],
                flushed: false,
            };
            {
                let locked = fixture.check().unwrap();
                hold(&mut cursor, &mut output, &locked.target_sha256).unwrap();
            }
            assert!(output.flushed);
            assert_eq!(cursor.position(), consumed);
            assert!(
                ReportLock::acquire(&fixture.database, "personal_work", Duration::ZERO).is_ok()
            );
            assert!(ReportLock::acquire(&processing, "personal", Duration::ZERO).is_ok());
        }
        assert_eq!(fs::read(&fixture.database).unwrap(), before);
    }
}
