use crate::{
    snapshots::{digest, reject_redirects},
    ReportError,
};
use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Keep the same filename and first-byte OS lock as the old Python process.
/// A leftover lock file is harmless: the operating system owns the live lock.
pub struct ReportLock {
    file: File,
}

impl ReportLock {
    fn lock_path(
        database: &Path,
        scope: &str,
        create_parent: bool,
    ) -> Result<Option<PathBuf>, ReportError> {
        let database = std::path::absolute(database)?;
        reject_redirects(&database, "REPORT_PATH_REDIRECTED")?;
        let Some(parent_dir) = database.parent() else {
            return Ok(None);
        };
        let Some(file_name) = database.file_name() else {
            return Ok(None);
        };
        if create_parent {
            std::fs::create_dir_all(parent_dir)?;
        } else if !parent_dir.is_dir() {
            return Ok(None);
        }
        let parent = std::fs::canonicalize(parent_dir)?;
        let full = parent.join(file_name);
        #[cfg(windows)]
        let mut identity = crate::processing_cache::normalized(&full)
            .to_string_lossy()
            .replace('/', "\\")
            .to_lowercase();
        #[cfg(not(windows))]
        let mut identity = full.to_string_lossy().into_owned();
        identity.push('\0');
        identity.push_str(scope);
        let path = parent.join(format!(
            ".report-{}.lock",
            &digest(identity.as_bytes())[..24]
        ));
        reject_redirects(&path, "REPORT_PATH_REDIRECTED")?;
        Ok(Some(path))
    }

    pub fn is_held(database: &Path, scope: &str) -> bool {
        let Ok(Some(path)) = Self::lock_path(database, scope, false) else {
            return false;
        };
        let Ok(file) = OpenOptions::new().read(true).open(&path) else {
            return false;
        };
        try_probe_held(&file)
    }

    pub fn acquire(database: &Path, scope: &str, wait: Duration) -> Result<Self, ReportError> {
        let path = Self::lock_path(database, scope, true)?
            .ok_or_else(|| ReportError::new("REPORT_PATH_INVALID", "报告库路径无效"))?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        if file.metadata()?.len() == 0 {
            file.write_all(&[0])?;
            file.flush()?;
        }
        file.seek(SeekFrom::Start(0))?;
        let deadline = Instant::now() + wait;
        loop {
            if try_lock(&file)? {
                return Ok(Self { file });
            }
            if Instant::now() >= deadline {
                return Err(ReportError::new(
                    "REPORT_ALREADY_RUNNING",
                    "同一报告库已有运行任务",
                ));
            }
            std::thread::sleep(
                Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

#[cfg(windows)]
fn try_lock(file: &File) -> Result<bool, ReportError> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_LOCK_VIOLATION},
        Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY},
        System::IO::OVERLAPPED,
    };
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if result != 0 {
        return Ok(true);
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_LOCK_VIOLATION {
        Ok(false)
    } else {
        Err(std::io::Error::from_raw_os_error(error as i32).into())
    }
}

#[cfg(not(windows))]
fn try_lock(file: &File) -> Result<bool, ReportError> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

#[cfg(windows)]
fn try_probe_held(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::{GetLastError, ERROR_LOCK_VIOLATION},
        Storage::FileSystem::{LockFileEx, UnlockFileEx, LOCKFILE_FAIL_IMMEDIATELY},
        System::IO::OVERLAPPED,
    };
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if result != 0 {
        let mut unlock_overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        unsafe {
            UnlockFileEx(file.as_raw_handle(), 0, 1, 0, &mut unlock_overlapped);
        }
        false
    } else {
        unsafe { GetLastError() == ERROR_LOCK_VIOLATION }
    }
}

#[cfg(not(windows))]
fn try_probe_held(file: &File) -> bool {
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(_) => false,
    }
}

impl Drop for ReportLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::{Storage::FileSystem::UnlockFileEx, System::IO::OVERLAPPED};
            let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
            unsafe {
                UnlockFileEx(self.file.as_raw_handle(), 0, 1, 0, &mut overlapped);
            }
        }
        #[cfg(not(windows))]
        {
            let _ = self.file.unlock();
        }
    }
}
