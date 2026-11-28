use super::{fs::reject_redirects, locking::ReportLock};
use crate::ReportError;
use rusqlite::{Connection, OpenFlags};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Duration,
};

pub fn open_read_only(
    path: &Path,
    busy_timeout: Duration,
    redirect_code: &'static str,
    missing_code: &'static str,
) -> Result<Connection, ReportError> {
    reject_redirects(path, redirect_code)?;
    if !path.is_file() {
        return Err(ReportError::new(missing_code, "数据库文件不存在"));
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(busy_timeout)?;
    Ok(connection)
}

pub fn open_read_only_nolock(path: &Path) -> Result<Connection, rusqlite::Error> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
}

pub fn quick_check(connection: &Connection) -> bool {
    matches!(
        connection.query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0)),
        Ok(value) if value == "ok"
    )
}

pub enum QuickCheckOutcome {
    Ok,
    Deferred,
    Failed,
}

pub fn quick_check_yielding(
    connection: &Connection,
    database: &Path,
    scope: &str,
) -> QuickCheckOutcome {
    if ReportLock::is_held(database, scope) {
        return QuickCheckOutcome::Deferred;
    }
    let interrupt = connection.get_interrupt_handle();
    let state = Arc::new((Mutex::new(false), Condvar::new()));
    let interrupted = Arc::new(AtomicBool::new(false));
    let watcher_state = Arc::clone(&state);
    let watcher_interrupted = Arc::clone(&interrupted);
    let watcher_db = database.to_path_buf();
    let watcher_scope = scope.to_owned();
    let watcher = std::thread::spawn(move || {
        let (lock, cvar) = &*watcher_state;
        let mut done = lock.lock().unwrap_or_else(|error| error.into_inner());
        while !*done {
            if ReportLock::is_held(&watcher_db, &watcher_scope) {
                watcher_interrupted.store(true, Ordering::Release);
                interrupt.interrupt();
                break;
            }
            let (guard, _) = cvar
                .wait_timeout(done, Duration::from_millis(50))
                .unwrap_or_else(|error| error.into_inner());
            done = guard;
        }
    });
    let result = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0));
    {
        let (lock, cvar) = &*state;
        let mut done = lock.lock().unwrap_or_else(|error| error.into_inner());
        *done = true;
        cvar.notify_one();
    }
    let _ = watcher.join();
    match result {
        Ok(value) if value == "ok" => QuickCheckOutcome::Ok,
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::OperationInterrupted
                && interrupted.load(Ordering::Acquire) =>
        {
            QuickCheckOutcome::Deferred
        }
        _ => QuickCheckOutcome::Failed,
    }
}
