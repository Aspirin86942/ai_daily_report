//! NTFS compression preserves ordinary reads, seek offsets and evidence hashes.
use super::{parse, store::ArchiveStore};
use crate::{config::AppConfig, store::locking::ReportLock, ReportError};
use ai_daily_worker_contract::archive::ArchiveArtifact;
use serde_json::{json, Value};
use std::{fs, io, path::Path, time::Duration};

pub fn allocated_bytes(path: &Path) -> io::Result<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::{
            Foundation::{GetLastError, SetLastError},
            Storage::FileSystem::GetCompressedFileSizeW,
        };
        let wide: Vec<_> = fs::canonicalize(path)?
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let mut high = 0;
        unsafe {
            SetLastError(0);
            let low = GetCompressedFileSizeW(wide.as_ptr(), &mut high);
            if low == u32::MAX && GetLastError() != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok((u64::from(high) << 32) | u64::from(low))
        }
    }
    #[cfg(not(windows))]
    {
        Ok(fs::metadata(path)?.len())
    }
}

pub fn compress(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::{fs::MetadataExt, fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::{
            Storage::FileSystem::{FILE_ATTRIBUTE_COMPRESSED, FILE_FLAG_BACKUP_SEMANTICS},
            System::{Ioctl::FSCTL_SET_COMPRESSION, IO::DeviceIoControl},
        };
        if fs::metadata(path)?.file_attributes() & FILE_ATTRIBUTE_COMPRESSED != 0 {
            return Ok(());
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let format: u16 = 1; // COMPRESSION_FORMAT_DEFAULT (LZNT1).
        let mut returned = 0;
        let success = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_SET_COMPRESSION,
                (&format as *const u16).cast(),
                std::mem::size_of_val(&format) as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if success == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "NTFS compression requires Windows",
        ))
    }
}

/// Idempotent maintenance: evidence bytes and database schema stay unchanged.
pub fn optimize(config: &AppConfig) -> Result<Value, ReportError> {
    let _lock = ReportLock::acquire(
        &config.paths.data_dir.join("archive/collector"),
        "archive",
        Duration::ZERO,
    )?;
    let store = ArchiveStore::open(config, true)?;
    let mut statement = store.conn.prepare(
        "SELECT id,manifest_json,sha256 FROM extractions WHERE status IN ('complete','partial') ORDER BY id",
    )?;
    let mut files = 0u64;
    let mut logical_bytes = 0u64;
    let mut before = 0u64;
    let mut after = 0u64;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (id, manifest, hash) = row?;
        let directory = store.extraction_dir(&id)?;
        crate::snapshots::reject_redirects(&directory, "ARCHIVE_PATH_REDIRECT")?;
        let artifact: ArchiveArtifact = serde_json::from_str(&manifest)?;
        parse::verify_artifact(&directory, &artifact, &hash)?;
        if config.archive.compress_extractions {
            compress(&directory)?;
        }
        for name in ["body.md", "units.jsonl", "units.idx"] {
            let path = directory.join(name);
            crate::snapshots::reject_redirects(&path, "ARCHIVE_PATH_REDIRECT")?;
            logical_bytes += fs::metadata(&path)?.len();
            before += allocated_bytes(&path)?;
            if config.archive.compress_extractions {
                compress(&path)?;
            }
            after += allocated_bytes(&path)?;
            files += 1;
        }
        parse::verify_artifact(&directory, &artifact, &hash)?;
    }
    Ok(json!({
        "status": "verified",
        "files": files,
        "logical_bytes": logical_bytes,
        "allocated_bytes_before": before,
        "allocated_bytes_after": after,
        "reclaimed_bytes": before.saturating_sub(after),
        "collection_interval_minutes": config.archive.collection_interval_minutes,
        "compression": if config.archive.compress_extractions { "ntfs_lznt1" } else { "unchanged" },
        "content_hashes_verified": true,
    }))
}
