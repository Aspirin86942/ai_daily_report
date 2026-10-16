use crate::{
    snapshots::{digest, reject_redirects},
    ReportError,
};
use std::{
    fs::{File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::Path,
    time::{Duration, Instant},
};

/// Keep the same filename and first-byte OS lock as the old Python process.
/// A leftover lock file is harmless: the operating system owns the live lock.
pub struct ReportLock {
    file: File,
}

impl ReportLock {
    pub fn acquire(database: &Path, scope: &str, wait: Duration) -> Result<Self, ReportError> {
        let database = std::path::absolute(database)?;
        reject_redirects(&database, "REPORT_PATH_REDIRECTED")?;
        std::fs::create_dir_all(database.parent().unwrap())?;
        let parent = std::fs::canonicalize(database.parent().unwrap())?;
        let full = parent.join(database.file_name().unwrap());
        let mut identity = full.to_string_lossy().into_owned();
        #[cfg(windows)]
        {
            identity = if let Some(unc) = identity.strip_prefix("\\\\?\\UNC\\") {
                format!("\\\\{unc}")
            } else {
                identity
                    .strip_prefix("\\\\?\\")
                    .unwrap_or(&identity)
                    .to_owned()
            };
            identity = identity.replace('/', "\\").to_lowercase();
        }
        identity.push('\0');
        identity.push_str(scope);
        let path = parent.join(format!(
            ".report-{}.lock",
            &digest(identity.as_bytes())[..24]
        ));
        reject_redirects(&path, "REPORT_PATH_REDIRECTED")?;
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
