use rusqlite::{Connection, OptionalExtension, Transaction};
use std::collections::BTreeMap;

pub(super) fn read(snapshot: &Transaction<'_>) -> Option<String> {
    if let Ok(Some(revision)) = managed(snapshot) {
        return Some(revision);
    }
    #[cfg(windows)]
    return rollback(snapshot);
    #[cfg(not(windows))]
    None
}

fn managed(reports: &Connection) -> rusqlite::Result<Option<String>> {
    let installed = reports
        .prepare("SELECT name,sql FROM main.sqlite_schema WHERE type='trigger' AND name GLOB 'search_revision_*'")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<BTreeMap<String, String>, _>>()?;
    if !crate::store::search_revision_triggers()
        .all(|(name, sql)| installed.get(&name) == Some(&sql))
    {
        return Ok(None);
    }
    // A drop/recreate of the same trigger definitions also changes the schema cookie.
    reports
        .query_row(
            "SELECT d.value || ':' || r.value || ':' || s.schema_version
         FROM report_metadata d, report_metadata r, pragma_schema_version s
         WHERE d.key='search_database_id_v1' AND r.key='search_revision_v1'
           AND length(d.value)=32 AND d.value NOT GLOB '*[^0-9a-f]*'
           AND length(r.value)=32 AND r.value NOT GLOB '*[^0-9a-f]*'",
            [],
            |row| row.get(0),
        )
        .optional()
}

// Unmarked Windows databases can still prove a rollback-journal snapshot. Use the
// handle that SQLite actually locked, even if the pathname now denotes another file.
// The header change counter is not a commit sequence in WAL mode.
#[cfg(windows)]
fn rollback(snapshot: &Transaction<'_>) -> Option<String> {
    use rusqlite::ffi;
    use std::mem::size_of_val;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx, FILE_BASIC_INFO, FILE_ID_INFO,
    };
    let mode: String = snapshot
        .query_row("PRAGMA main.journal_mode", [], |r| r.get(0))
        .ok()?;
    if !matches!(mode.as_str(), "delete" | "truncate" | "persist") {
        return None;
    }
    let mut header = [0u8; 100];
    let mut id = FILE_ID_INFO::default();
    let mut basic = FILE_BASIC_INFO::default();
    // Both VFS pointers are borrowed from this live connection, on its owning thread.
    // Never close the OS handle or issue IO outside the established read transaction.
    unsafe {
        let db = snapshot.handle();
        let main = c"main".as_ptr();
        let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
        let mut handle: HANDLE = std::ptr::null_mut();
        if ffi::sqlite3_txn_state(db, main) != ffi::SQLITE_TXN_READ
            || ffi::sqlite3_file_control(
                db,
                main,
                ffi::SQLITE_FCNTL_FILE_POINTER,
                (&raw mut file).cast(),
            ) != ffi::SQLITE_OK
            || ffi::sqlite3_file_control(
                db,
                main,
                ffi::SQLITE_FCNTL_WIN32_GET_HANDLE,
                (&raw mut handle).cast(),
            ) != ffi::SQLITE_OK
        {
            return None;
        }
        let read = file.as_ref()?.pMethods.as_ref()?.xRead?;
        if read(file, header.as_mut_ptr().cast(), header.len() as i32, 0) != ffi::SQLITE_OK
            || GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&raw mut id).cast(),
                size_of_val(&id) as u32,
            ) == 0
            || GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                (&raw mut basic).cast(),
                size_of_val(&basic) as u32,
            ) == 0
        {
            return None;
        }
    }
    if &header[..16] != b"SQLite format 3\0"
        || header[18..20] != [1, 1]
        || header[24..28] != header[92..96]
        || header[28..32] == [0; 4]
        || id.VolumeSerialNumber == 0
        || id.FileId.Identifier == [0; 16]
        || basic.CreationTime == 0
        || basic.ChangeTime == 0
        || basic.LastWriteTime == 0
    {
        return None;
    }
    Some(format!(
        "rollback:{}:{}:{}:{}:{}:{}",
        id.VolumeSerialNumber,
        ai_daily_worker_contract::archive::hex(&id.FileId.Identifier),
        basic.CreationTime,
        basic.ChangeTime,
        basic.LastWriteTime,
        crate::snapshots::digest(&header)
    ))
}
