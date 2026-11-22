use super::*;
use rusqlite::{config::DbConfig, ffi, params_from_iter, ErrorCode, Row, Transaction};
use std::{ffi::c_void, io::ErrorKind, ops::Deref};

pub(super) const SCHEMA: &str = include_str!("schema.sql");
const APPLICATION_ID: i64 = 0x4144_5343;
const LOOKUP_COLUMNS: &str =
    "c.file_id, c.source_guard, c.profile_hash, c.payload, c.record_sha256, c.last_access_day";
pub(super) const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;

pub(super) struct Database {
    connection: Connection,
    // SQLite closes before the memory registered with its progress handler.
    deadline: Box<Instant>,
}

impl Deref for Database {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.connection
    }
}

impl Database {
    pub(super) fn transaction_with_behavior(
        &mut self,
        behavior: TransactionBehavior,
    ) -> rusqlite::Result<Transaction<'_>> {
        self.connection.transaction_with_behavior(behavior)
    }
}

pub(super) fn sql<T>(deadline: Instant, result: rusqlite::Result<T>) -> Result<T, ReportError> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            check_time(deadline)?;
            let code = match error.sqlite_error_code() {
                Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => "SCAN_CACHE_BUSY",
                _ => "SCAN_CACHE_DATABASE_FAILED",
            };
            Err(ReportError::new(code, error.to_string()))
        }
    }
}

pub(super) fn refresh_timeout(
    connection: &Connection,
    deadline: Instant,
) -> Result<(), ReportError> {
    check_time(deadline)?;
    let wait = deadline
        .saturating_duration_since(Instant::now())
        .min(MAX_BUSY_WAIT);
    sql(deadline, connection.busy_timeout(wait))
}

pub(super) fn open(
    path: &Path,
    flags: OpenFlags,
    deadline: Instant,
) -> Result<Database, ReportError> {
    check_time(deadline)?;
    let database = Database {
        connection: sql(deadline, Connection::open_with_flags(path, flags))?,
        deadline: Box::new(deadline),
    };
    refresh_timeout(&database, deadline)?;
    // A cache read never checkpoints on connection close.
    sql(
        deadline,
        database.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true),
    )?;
    unsafe extern "C" fn expired(context: *mut c_void) -> std::ffi::c_int {
        let deadline = unsafe { *context.cast::<Instant>() };
        i32::from(Instant::now() >= deadline)
    }
    // The pointer is stable across moves and its owner outlives the connection.
    unsafe {
        ffi::sqlite3_progress_handler(
            database.handle(),
            4_096,
            Some(expired),
            std::ptr::from_ref(database.deadline.as_ref())
                .cast_mut()
                .cast(),
        );
    }
    sql(
        deadline,
        database.execute_batch(
            "PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-32768;
         PRAGMA mmap_size=134217728;
         PRAGMA trusted_schema=OFF;",
        ),
    )?;
    Ok(database)
}

pub(super) fn check_paths(path: &Path) -> Result<(), ReportError> {
    const CODE: &str = "SCAN_CACHE_PATH_REDIRECTED";
    crate::snapshots::reject_redirects(path, CODE)?;
    // The database and sidecars share the already validated ancestor chain.
    for suffix in ["-wal", "-shm", "-journal"] {
        let info = match fs::symlink_metadata(sidecar(path, suffix)) {
            Ok(info) => info,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        #[cfg(windows)]
        let redirect = {
            use std::os::windows::fs::MetadataExt;
            info.file_attributes() & 0x400 != 0
        };
        #[cfg(not(windows))]
        let redirect = info.file_type().is_symlink();
        if redirect {
            return Err(ReportError::new(CODE, "路径包含链接或重定向"));
        }
    }
    Ok(())
}

pub(super) fn verify_schema(connection: &Connection) -> Result<(), ReportError> {
    fn pragma(connection: &Connection, name: &str) -> rusqlite::Result<i64> {
        connection.pragma_query_value(None, name, |row| row.get(0))
    }
    let inspect = || -> rusqlite::Result<bool> {
        let mode: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if pragma(connection, "application_id")? != APPLICATION_ID
            || pragma(connection, "user_version")? != 4
            || pragma(connection, "auto_vacuum")? != 1
            || mode != "wal"
        {
            return Ok(false);
        }
        let mut expected: HashSet<_> = SCHEMA
            .split(';')
            .map(str::trim)
            .filter(|statement| statement.starts_with("CREATE "))
            .collect();
        let mut stmt = connection
            .prepare("SELECT sql FROM main.sqlite_schema WHERE name NOT GLOB 'sqlite_*'")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if !expected.remove(row.get_ref(0)?.as_str()?) {
                return Ok(false);
            }
        }
        Ok(expected.is_empty())
    };
    match inspect() {
        Ok(true) => Ok(()),
        Ok(false) => Err(schema_error()),
        Err(error) => Err(ReportError::new("SCAN_CACHE_SCHEMA", error.to_string())),
    }
}

fn schema_error() -> ReportError {
    ReportError::new("SCAN_CACHE_SCHEMA", "数据库不符合扫描缓存 v4 的身份或结构")
}

pub(super) fn enable_wal(connection: &Connection, deadline: Instant) -> Result<(), ReportError> {
    refresh_timeout(connection, deadline)?;
    sql(
        deadline,
        connection.pragma_update(None, "auto_vacuum", "FULL"),
    )?;
    let mode = sql(
        deadline,
        connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0)),
    )?;
    if mode != "wal" {
        return Err(schema_error());
    }
    Ok(())
}

fn wal_len(path: &Path) -> Result<u64, ReportError> {
    optional_len(&sidecar(path, "-wal"))
}

fn log_len(path: &Path) -> Result<u64, ReportError> {
    Ok(wal_len(path)?.saturating_add(optional_len(&sidecar(path, "-shm"))?))
}

pub(super) fn disk_len(path: &Path) -> Result<u64, ReportError> {
    Ok(optional_len(path)?.saturating_add(log_len(path)?))
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn optional_len(path: &Path) -> Result<u64, ReportError> {
    match fs::metadata(path) {
        Ok(info) => Ok(info.len()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn checkpoint_if_needed(
    connection: &Connection,
    path: &Path,
    limit: u64,
    deadline: Instant,
) -> Result<(), ReportError> {
    let wal = wal_len(path)?;
    if wal == 0 || disk_len(path)? <= limit {
        return Ok(());
    }
    refresh_timeout(connection, deadline)?;
    let busy: i64 = sql(
        deadline,
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0)),
    )?;
    if busy != 0 {
        check_time(deadline)?;
        return Err(ReportError::new(
            "SCAN_CACHE_BUSY",
            "SQLite 读事务阻止了扫描缓存检查点",
        ));
    }
    Ok(())
}

pub(super) fn row_key(row: &Row<'_>) -> rusqlite::Result<CacheKey> {
    Ok(CacheKey {
        file_id: row.get(0)?,
        source_guard: row.get(1)?,
        profile_hash: row.get(2)?,
    })
}

fn key_params<'a>(keys: &'a [&CacheKey]) -> impl Iterator<Item = &'a str> {
    keys.iter().flat_map(|key| {
        [
            key.file_id.as_str(),
            key.source_guard.as_str(),
            key.profile_hash.as_str(),
        ]
    })
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("(?,?,?)", count)
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn read_batch(
    connection: &Connection,
    deadline: Instant,
    keys: &[&CacheKey],
    mut read: impl FnMut(&Row<'_>) -> rusqlite::Result<()>,
) -> Result<(), ReportError> {
    if keys.is_empty() {
        return Ok(());
    }
    let query = format!(
        "WITH wanted(file_id, source_guard, profile_hash) AS (VALUES {})
         SELECT {LOOKUP_COLUMNS}, c.parser_backend, c.content_sha256, c.source_sha256, c.content_text, 1
         FROM wanted CROSS JOIN parse_cache AS c USING (file_id, source_guard, profile_hash)
         WHERE length(c.payload) <= {record_bytes} AND octet_length(c.content_text) <= {content_bytes}
         UNION ALL
         SELECT {LOOKUP_COLUMNS}, NULL, NULL, NULL, NULL, 0
         FROM wanted CROSS JOIN pdf_negative_cache AS c USING (file_id, source_guard, profile_hash)
         WHERE length(c.payload) <= {record_bytes}",
        placeholders(keys.len()),
        record_bytes = records::MAX_RECORD_BYTES,
        content_bytes = records::MAX_CONTENT_BYTES,
    );
    refresh_timeout(connection, deadline)?;
    let mut statement = sql(deadline, connection.prepare_cached(&query))?;
    let mut rows = sql(
        deadline,
        statement.query(params_from_iter(key_params(keys))),
    )?;
    while let Some(row) = sql(deadline, rows.next())? {
        check_time(deadline)?;
        sql(deadline, read(row))?;
    }
    Ok(())
}

pub(super) fn write_entries(
    tx: &Transaction<'_>,
    deadline: Instant,
    hits: &CacheHits,
    entries: Vec<records::EncodedEntry>,
    day: i64,
    now_ms: i64,
) -> Result<(), ReportError> {
    for entry in entries {
        check_time(deadline)?;
        let key = entry.reference.key();
        refresh_timeout(tx, deadline)?;
        if let Some(parse) = entry.parse {
            let mut opposite = sql(deadline, tx.prepare_cached(
                "DELETE FROM pdf_negative_cache WHERE file_id=?1 AND source_guard=?2 AND profile_hash=?3",
            ))?;
            sql(
                deadline,
                opposite.execute(params![key.file_id, key.source_guard, key.profile_hash]),
            )?;
            let mut insert = sql(deadline, tx.prepare_cached(
                "INSERT INTO parse_cache
                   (file_id,source_guard,profile_hash,parser_backend,content_text,content_sha256,source_sha256,payload,record_sha256,last_access_day,written_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(file_id,source_guard,profile_hash) DO UPDATE SET
                   parser_backend=excluded.parser_backend, content_text=excluded.content_text,
                   content_sha256=excluded.content_sha256,
                   source_sha256=excluded.source_sha256, payload=excluded.payload, record_sha256=excluded.record_sha256,
                   last_access_day=max(last_access_day,excluded.last_access_day), written_at_ms=excluded.written_at_ms",
            ))?;
            sql(
                deadline,
                insert.execute(params![
                    key.file_id,
                    key.source_guard,
                    key.profile_hash,
                    parse.backend,
                    parse.content_text,
                    parse.content_sha256,
                    parse.source_sha256,
                    entry.payload,
                    entry.record_sha256,
                    day,
                    now_ms
                ]),
            )?;
        } else {
            let mut opposite = sql(deadline, tx.prepare_cached(
                "DELETE FROM parse_cache WHERE file_id=?1 AND source_guard=?2 AND profile_hash=?3",
            ))?;
            sql(
                deadline,
                opposite.execute(params![key.file_id, key.source_guard, key.profile_hash]),
            )?;
            let mut insert = sql(deadline, tx.prepare_cached(
                "INSERT INTO pdf_negative_cache
                   (file_id,source_guard,profile_hash,payload,record_sha256,last_access_day,written_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(file_id,source_guard,profile_hash) DO UPDATE SET
                   payload=excluded.payload, record_sha256=excluded.record_sha256,
                   last_access_day=max(last_access_day,excluded.last_access_day), written_at_ms=excluded.written_at_ms",
            ))?;
            sql(
                deadline,
                insert.execute(params![
                    key.file_id,
                    key.source_guard,
                    key.profile_hash,
                    entry.payload,
                    entry.record_sha256,
                    day,
                    now_ms
                ]),
            )?;
        }
    }
    for (table, parse) in [("parse_cache", true), ("pdf_negative_cache", false)] {
        let keys: Vec<_> = hits
            .entries
            .iter()
            .filter_map(|(key, hit)| {
                (hit.last_access_day < day && hit.parsed == parse).then_some(key)
            })
            .collect();
        for chunk in keys.chunks(256) {
            refresh_timeout(tx, deadline)?;
            let query = format!(
                "UPDATE {table} SET last_access_day=?1 WHERE last_access_day<?1
                   AND (file_id,source_guard,profile_hash) IN (VALUES {})",
                placeholders(chunk.len())
            );
            let mut statement = sql(deadline, tx.prepare_cached(&query))?;
            sql(deadline, statement.raw_bind_parameter(1, day))?;
            for (index, key) in key_params(chunk).enumerate() {
                sql(deadline, statement.raw_bind_parameter(index + 2, key))?;
            }
            sql(deadline, statement.raw_execute())?;
        }
    }
    Ok(())
}

pub(super) fn write_snapshot(
    tx: &Transaction<'_>,
    deadline: Instant,
    snapshot: records::EncodedSnapshot,
    day: i64,
    now_ms: i64,
) -> Result<(), ReportError> {
    refresh_timeout(tx, deadline)?;
    sql(deadline, tx.execute(
        "INSERT INTO scan_snapshots(snapshot_key,manifest,refs,record_sha256,last_access_day,written_at_ms)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(snapshot_key) DO UPDATE SET manifest=excluded.manifest,refs=excluded.refs,
            record_sha256=excluded.record_sha256,last_access_day=max(last_access_day,excluded.last_access_day),
            written_at_ms=excluded.written_at_ms",
        params![snapshot.key, snapshot.manifest, snapshot.refs, snapshot.fingerprint, day, now_ms],
    ))?;
    Ok(())
}

pub(super) fn prune_snapshots(tx: &Transaction<'_>, deadline: Instant) -> Result<(), ReportError> {
    refresh_timeout(tx, deadline)?;
    sql(
        deadline,
        tx.execute(
            "DELETE FROM scan_snapshots AS s WHERE CASE
            WHEN NOT json_valid(s.refs) OR json_type(s.refs) <> 'array' THEN 1
            ELSE EXISTS (
                SELECT 1 FROM json_each(s.refs) AS r WHERE CASE
                    WHEN r.type <> 'object' THEN 1
                    WHEN r.value ->> '$.reference.kind' = 'parse' THEN NOT EXISTS (
                        SELECT 1 FROM parse_cache AS c
                        WHERE c.file_id = r.value ->> '$.reference.key.file_id'
                          AND c.source_guard = r.value ->> '$.reference.key.source_guard'
                          AND c.profile_hash = r.value ->> '$.reference.key.profile_hash'
                          AND c.record_sha256 = r.value ->> '$.record_sha256')
                    WHEN r.value ->> '$.reference.kind' = 'negative' THEN NOT EXISTS (
                        SELECT 1 FROM pdf_negative_cache AS c
                        WHERE c.file_id = r.value ->> '$.reference.key.file_id'
                          AND c.source_guard = r.value ->> '$.reference.key.source_guard'
                          AND c.profile_hash = r.value ->> '$.reference.key.profile_hash'
                          AND c.record_sha256 = r.value ->> '$.record_sha256')
                    ELSE 1 END)
            END",
            [],
        ),
    )?;
    Ok(())
}

pub(super) fn evict(
    tx: &Transaction<'_>,
    path: &Path,
    max_bytes: u64,
    deadline: Instant,
) -> Result<(), ReportError> {
    let page_size: i64 = sql(
        deadline,
        tx.pragma_query_value(None, "page_size", |row| row.get(0)),
    )?;
    // SQLite may still append dirty cache pages at COMMIT after this check.
    let wal_headroom = (max_bytes / 16).min(32 * 1024 * 1024);
    let target = max_bytes - wal_headroom;
    let mut removed_reference = false;
    loop {
        refresh_timeout(tx, deadline)?;
        let pages: i64 = sql(
            deadline,
            tx.pragma_query_value(None, "page_count", |row| row.get(0)),
        )?;
        let free: i64 = sql(
            deadline,
            tx.pragma_query_value(None, "freelist_count", |row| row.get(0)),
        )?;
        if (pages.saturating_sub(free).saturating_mul(page_size).max(0) as u64)
            .saturating_add(log_len(path)?)
            <= target
        {
            break;
        }
        let victim = sql(
            deadline,
            tx.query_row(
                "WITH
             p AS (SELECT file_id,source_guard,profile_hash,last_access_day,written_at_ms
                 FROM parse_cache ORDER BY last_access_day,written_at_ms,file_id,source_guard,profile_hash LIMIT 1),
             n AS (SELECT file_id,source_guard,profile_hash,last_access_day,written_at_ms
                 FROM pdf_negative_cache ORDER BY last_access_day,written_at_ms,file_id,source_guard,profile_hash LIMIT 1),
             s AS (SELECT snapshot_key,last_access_day,written_at_ms
                 FROM scan_snapshots ORDER BY last_access_day,written_at_ms,snapshot_key LIMIT 1)
             SELECT 0,* FROM p UNION ALL SELECT 1,* FROM n
                 UNION ALL SELECT 2,snapshot_key,'','',last_access_day,written_at_ms FROM s
             ORDER BY 5,6,1,2,3,4 LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, u8>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional(),
        )?;
        let Some((table, a, b, c)) = victim else {
            return Err(invalid("扫描缓存无法在大小上限内提交"));
        };
        if table == 2 {
            sql(
                deadline,
                tx.execute("DELETE FROM scan_snapshots WHERE snapshot_key=?1", [a]),
            )?;
        } else {
            let table = if table == 0 {
                "parse_cache"
            } else {
                "pdf_negative_cache"
            };
            let query = format!(
                "DELETE FROM {table} WHERE file_id=?1 AND source_guard=?2 AND profile_hash=?3"
            );
            sql(
                deadline,
                sql(deadline, tx.prepare_cached(&query))?.execute(params![a, b, c]),
            )?;
            removed_reference = true;
        }
    }
    if removed_reference {
        prune_snapshots(tx, deadline)?;
    }
    Ok(())
}
