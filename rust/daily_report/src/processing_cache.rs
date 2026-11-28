//! One OS lock and one shared reservation pool for every personal processing writer.
use crate::{
    infra::{
        fs::{normalized_path, reject_redirects, write_synced},
        locking::ReportLock,
    },
    types::DAILY_CACHE_DIR,
    ReportError,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const MAX_BYTES: u64 = 1024 * 1024 * 1024;
pub const CONTROL_BYTES: u64 = 72 * 1024 * 1024;
pub const MAX_INPUT_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
pub const RETENTION_DAYS: i64 = 7;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    created: DateTime<Utc>,
    pending: bool,
}

struct State {
    used: u64,
    reserved: u64,
    entries: BTreeMap<String, Entry>,
}

pub struct CacheQuota {
    pub root: PathBuf,
    max_bytes: u64,
    control_bytes: u64,
    now: DateTime<Utc>,
    deadline: Instant,
    state: Mutex<State>,
}

pub struct ProcessingSession {
    pub quota: Arc<CacheQuota>,
    _lock: ReportLock,
}

impl ProcessingSession {
    pub fn acquire(
        data_dir: &Path,
        now: DateTime<Utc>,
        deadline: Instant,
    ) -> Result<Self, ReportError> {
        Self::at(
            &data_dir.join(DAILY_CACHE_DIR),
            now,
            deadline,
            MAX_BYTES,
            CONTROL_BYTES,
            // Concurrent report commands share this lock; waiting uses the run budget.
            // Waiting still consumes the caller's original application budget.
            Duration::from_secs(120),
        )
    }

    pub fn at(
        root: &Path,
        now: DateTime<Utc>,
        deadline: Instant,
        max_bytes: u64,
        control_bytes: u64,
        wait: Duration,
    ) -> Result<Self, ReportError> {
        check_deadline(deadline)?;
        reject_redirects(root, "PROCESSING_PATH_REDIRECTED")?;
        let lock = ReportLock::acquire(
            &root.join("processing"),
            "personal",
            wait.min(deadline.saturating_duration_since(Instant::now())),
        )?;
        let quota = CacheQuota::open(root, now, deadline, max_bytes, control_bytes)?;
        Ok(Self {
            quota: Arc::new(quota),
            _lock: lock,
        })
    }

    pub fn prune_snapshots(&self) -> Result<(), ReportError> {
        let snapshots = self.quota.root.join("snapshots");
        for path in self.quota.expired()? {
            check_deadline(self.quota.deadline)?;
            // Legacy archive objects are retired explicitly, never by this scanner.
            if path.starts_with(&snapshots) {
                self.quota.remove(&path)?;
            }
        }
        Ok(())
    }
}

fn control(path: &str) -> bool {
    matches!(
        path,
        ".cache-ledger.json"
            | ".cache-ledger.next"
            | "archive/index.sqlite3"
            | "archive/index.sqlite3-journal"
    ) || !path.contains('/') && path.starts_with(".report-") && path.ends_with(".lock")
}

impl CacheQuota {
    fn open(
        root: &Path,
        now: DateTime<Utc>,
        deadline: Instant,
        max_bytes: u64,
        control_bytes: u64,
    ) -> Result<Self, ReportError> {
        let root = normalized_path(&std::path::absolute(root)?);
        let ledger = root.join(".cache-ledger.json");
        let mut entries: BTreeMap<String, Entry> = if ledger.exists() {
            if fs::metadata(&ledger)?.len() > (control_bytes / 4).min(2 * 1024 * 1024) {
                return Err(full());
            }
            serde_json::from_slice(&fs::read(&ledger)?)?
        } else {
            BTreeMap::new()
        };
        // A prior owner has exited. Count its writes and keep their original expiry.
        for entry in entries.values_mut() {
            entry.pending = false;
        }
        let quota = Self {
            root,
            now,
            deadline,
            max_bytes,
            control_bytes,
            state: Mutex::new(State {
                used: 0,
                reserved: 0,
                entries,
            }),
        };
        {
            let mut state = quota.state.lock().map_err(|_| full())?;
            let (used, control_used) = quota.measure(&state.entries)?;
            if control_used > control_bytes {
                return Err(full());
            }
            state.used = used;
            quota.save(&state)?;
        }
        Ok(quota)
    }

    pub fn path(&self, relative: &str) -> Result<PathBuf, ReportError> {
        let p = Path::new(relative);
        if relative.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(ReportError::new(
                "PROCESSING_PATH_INVALID",
                "处理材料必须位于固定处理区",
            ));
        }
        let path = self.root.join(p);
        reject_redirects(&path, "PROCESSING_PATH_REDIRECTED")?;
        Ok(path)
    }

    fn relative(&self, path: &Path) -> Result<String, ReportError> {
        let path = normalized_path(&std::path::absolute(path)?);
        // canonicalize only the existing root; destinations need not exist yet.
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| ReportError::new("PROCESSING_PATH_INVALID", "写入超出处理区"))?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        self.path(&relative)?;
        if control(&relative) {
            return Err(ReportError::new(
                "PROCESSING_PATH_INVALID",
                "材料不能覆盖控制文件",
            ));
        }
        Ok(relative)
    }

    fn measure(&self, entries: &BTreeMap<String, Entry>) -> Result<(u64, u64), ReportError> {
        let mut used = 0u64;
        let mut controls = 0u64;
        let read_failed = || ReportError::new("PROCESSING_READ_FAILED", "无法核对处理区");
        for entry in self.measured_files(&self.root, read_failed) {
            let (entry, size) = entry?;
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if control(&relative) {
                controls = controls.saturating_add(size);
            } else if entries.contains_key(relative.as_str())
                || relative
                    .match_indices('/')
                    .any(|(index, _)| entries.contains_key(&relative[..index]))
            {
                used = used.saturating_add(size);
            } else {
                return Err(ReportError::new(
                    "PROCESSING_UNKNOWN_FILE",
                    format!("处理区存在未登记文件：{relative}"),
                ));
            }
        }
        Ok((used, controls))
    }

    fn save(&self, state: &State) -> Result<(), ReportError> {
        let bytes = serde_json::to_vec(&state.entries)?;
        if bytes.len() as u64 > (self.control_bytes / 4).min(2 * 1024 * 1024) {
            return Err(full());
        }
        let next = self.root.join(".cache-ledger.next");
        reject_redirects(&next, "PROCESSING_PATH_REDIRECTED")?;
        let mut file = fs::File::create(&next)?;
        write_synced(&mut file, &bytes)?;
        drop(file);
        fs::rename(next, self.root.join(".cache-ledger.json"))?;
        Ok(())
    }

    pub fn reserve(self: &Arc<Self>, bytes: u64) -> Result<CacheReservation, ReportError> {
        check_deadline(self.deadline)?;
        let mut state = self.state.lock().map_err(|_| full())?;
        if self
            .remaining(&state)
            .is_none_or(|remaining| bytes > remaining)
        {
            return Err(full());
        }
        state.reserved += bytes;
        Ok(CacheReservation {
            quota: Arc::clone(self),
            bytes,
            paths: BTreeMap::new(),
            done: false,
        })
    }

    pub fn used_bytes(&self) -> Result<u64, ReportError> {
        Ok(self.state.lock().map_err(|_| full())?.used + self.control_bytes)
    }

    pub fn write(self: &Arc<Self>, path: &Path, bytes: &[u8]) -> Result<(), ReportError> {
        write_immutable(
            path,
            bytes,
            Some(self),
            "PROCESSING_PATH_REDIRECTED",
            |error| Err(error.into()),
        )
    }

    pub fn expired(&self) -> Result<Vec<PathBuf>, ReportError> {
        let state = self.state.lock().map_err(|_| full())?;
        state
            .entries
            .iter()
            .filter(|(_, e)| {
                !e.pending && e.created <= self.now - chrono::Duration::days(RETENTION_DAYS)
            })
            .map(|(path, _)| self.path(path))
            .collect()
    }

    pub fn available_bytes(&self) -> Result<u64, ReportError> {
        let state = self.state.lock().map_err(|_| full())?;

        Ok(self.remaining(&state).unwrap_or(0))
    }

    fn remaining(&self, state: &State) -> Option<u64> {
        self.max_bytes
            .checked_sub(self.control_bytes)?
            .checked_sub(state.used)?
            .checked_sub(state.reserved)
    }

    /// The caller must release database references before removing a registered object.
    pub fn remove(&self, path: &Path) -> Result<(), ReportError> {
        let relative = self.relative(path)?;
        let mut state = self.state.lock().map_err(|_| full())?;
        if !state.entries.contains_key(&relative) {
            return Err(ReportError::new(
                "PROCESSING_UNKNOWN_FILE",
                "禁止清理未登记材料",
            ));
        }
        let size = self.remove_path(path)?;
        state.used = state.used.saturating_sub(size);
        state.entries.remove(&relative);
        self.save(&state)
    }

    fn measured_files(
        &self,
        root: &Path,
        read_failed: fn() -> ReportError,
    ) -> impl Iterator<Item = Result<(walkdir::DirEntry, u64), ReportError>> + '_ {
        walkdir::WalkDir::new(root)
            .follow_links(false)
            .max_open(16)
            .into_iter()
            .filter_map(move |entry| {
                let read = || -> Result<_, ReportError> {
                    check_deadline(self.deadline)?;
                    let entry = entry.map_err(|_| read_failed())?;
                    reject_redirects(entry.path(), "PROCESSING_PATH_REDIRECTED")?;
                    if !entry.file_type().is_file() {
                        return Ok(None);
                    }
                    let size = entry.metadata().map_err(|_| full())?.len();
                    Ok(Some((entry, size)))
                };
                read().transpose()
            })
    }

    fn size(&self, path: &Path) -> Result<u64, ReportError> {
        if !path.exists() {
            reject_redirects(path, "PROCESSING_PATH_REDIRECTED")?;
            return Ok(0);
        }
        self.measured_files(path, full)
            .try_fold(0_u64, |sum, item| Ok(sum.saturating_add(item?.1)))
    }

    fn remove_path(&self, path: &Path) -> Result<u64, ReportError> {
        self.relative(path)?;
        let size = self.size(path)?; // Check every descendant before recursive deletion.
        if path.is_dir() {
            fs::remove_dir_all(path)?;
        } else if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(size)
    }
}

pub struct CacheReservation {
    quota: Arc<CacheQuota>,
    bytes: u64,
    paths: BTreeMap<PathBuf, (String, TrackedPath)>,
    done: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrackedPath {
    Owned,
    Temporary,
    Uninstalled,
    VerifiedExisting,
}

impl CacheReservation {
    pub fn track(&mut self, path: &Path) -> Result<(), ReportError> {
        self.track_paths(&[(path, TrackedPath::Owned)])
    }

    fn track_immutable(&mut self, path: &Path, temporary: &Path) -> Result<(), ReportError> {
        self.track_paths(&[
            (path, TrackedPath::Uninstalled),
            (temporary, TrackedPath::Temporary),
        ])
    }

    fn track_paths(&mut self, paths: &[(&Path, TrackedPath)]) -> Result<(), ReportError> {
        let mut state = self.quota.state.lock().map_err(|_| full())?;
        let mut tracked: BTreeMap<_, (String, TrackedPath)> = BTreeMap::new();
        for &(path, kind) in paths {
            let key = self.quota.relative(path)?;
            let temporary = kind == TrackedPath::Temporary;
            if temporary && fs::metadata(path)?.len() != 0 {
                return Err(ReportError::new(
                    "PROCESSING_WRITE_CONFLICT",
                    "暂存文件必须为空",
                ));
            }
            if state.entries.contains_key(&key)
                || tracked.values().any(|(tracked_key, _)| tracked_key == &key)
                || !temporary && path.exists()
            {
                return Err(ReportError::new(
                    "PROCESSING_WRITE_CONFLICT",
                    "处理路径已经存在",
                ));
            }
            tracked.insert(path.to_owned(), (key, kind));
        }
        let entries = tracked.values().map(|(key, _)| {
            (
                key.clone(),
                Entry {
                    created: self.quota.now,
                    pending: true,
                },
            )
        });
        state.entries.extend(entries);
        if let Err(error) = self.quota.save(&state) {
            for (key, _) in tracked.values() {
                state.entries.remove(key);
            }
            return Err(error);
        }
        self.paths.append(&mut tracked);
        Ok(())
    }

    pub fn commit(mut self, paths: &[PathBuf]) -> Result<(), ReportError> {
        let mut total = 0u64;
        let mut retained = 0u64;
        for path in self.paths.keys() {
            let size = self.quota.size(path)?;
            total = total.saturating_add(size);
            if paths.contains(path) {
                retained = retained.saturating_add(size);
            }
        }
        if total > self.bytes || paths.iter().any(|path| !self.paths.contains_key(path)) {
            return Err(full());
        }
        let mut state = self.quota.state.lock().map_err(|_| full())?;
        for (path, (key, _)) in &self.paths {
            if paths.contains(path) {
                state.entries.get_mut(key).ok_or_else(full)?.pending = false;
            } else {
                self.quota.remove_path(path)?;
                state.entries.remove(key);
            }
        }
        self.quota.save(&state)?;
        state.used = state.used.saturating_add(retained);
        state.reserved = state.reserved.saturating_sub(self.bytes);
        self.done = true;
        Ok(())
    }
}

impl Drop for CacheReservation {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Ok(mut state) = self.quota.state.lock() {
            let mut residual = 0_u64;
            for (path, (key, kind)) in &self.paths {
                let forget = match kind {
                    TrackedPath::Uninstalled => true,
                    TrackedPath::VerifiedExisting => false,
                    TrackedPath::Owned | TrackedPath::Temporary => {
                        self.quota.remove_path(path).is_ok()
                    }
                };
                if forget {
                    state.entries.remove(key);
                } else {
                    // Keep verified files and failed cleanups pending and charged.
                    if let Some(entry) = state.entries.get_mut(key) {
                        entry.pending = true;
                    }
                    residual = residual.saturating_add(self.quota.size(path).unwrap_or(self.bytes));
                }
            }
            state.used = state.used.saturating_add(residual);
            state.reserved = state.reserved.saturating_sub(self.bytes);
            let _ = self.quota.save(&state);
        }
    }
}

pub(crate) const fn write_reservation_bytes(bytes: u64) -> u64 {
    // The staged and installed names may coexist until the temporary handle closes.
    bytes.saturating_mul(2)
}

pub(crate) fn write_immutable(
    path: &Path,
    bytes: &[u8],
    quota: Option<&Arc<CacheQuota>>,
    redirect_code: &'static str,
    verify_existing: impl FnOnce(std::io::Error) -> Result<(), ReportError>,
) -> Result<(), ReportError> {
    let mut reservation = quota
        .map(|quota| quota.reserve(write_reservation_bytes(bytes.len() as u64)))
        .transpose()?;
    if let Some(quota) = quota {
        quota.relative(path)?;
    }
    let parent = path.parent().ok_or_else(full)?;
    fs::create_dir_all(parent)?;
    reject_redirects(path, redirect_code)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    if let Some(reservation) = &mut reservation {
        reservation.track_immutable(path, temporary.path())?;
    }
    write_synced(temporary.as_file_mut(), bytes)?;
    install_immutable(path, reservation, temporary, verify_existing)
}

fn install_immutable(
    path: &Path,
    reservation: Option<CacheReservation>,
    temporary: tempfile::NamedTempFile,
    verify_existing: impl FnOnce(std::io::Error) -> Result<(), ReportError>,
) -> Result<(), ReportError> {
    let installed = match fs::hard_link(temporary.path(), path) {
        Ok(()) => Ok(TrackedPath::Owned),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            verify_existing(error).map(|()| TrackedPath::VerifiedExisting)
        }
        Err(error) => Err(error.into()),
    };
    drop(temporary);
    let kind = installed?;
    if let Some(mut reservation) = reservation {
        reservation.paths.get_mut(path).unwrap().1 = kind;
        reservation.commit(&[path.to_owned()])?;
    }
    Ok(())
}

pub fn check_deadline(deadline: Instant) -> Result<(), ReportError> {
    if Instant::now() >= deadline {
        Err(ReportError::new("PROCESSING_DEADLINE", "处理预算已耗尽"))
    } else {
        Ok(())
    }
}

fn full() -> ReportError {
    ReportError::new("PROCESSING_CACHE_FULL", "处理缓存容量不足，当前材料已暂缓")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTENT: &[u8] = b"model snapshot\0\xff";

    fn open(root: &Path) -> Arc<CacheQuota> {
        Arc::new(
            CacheQuota::open(
                root,
                Utc::now(),
                Instant::now() + Duration::from_secs(30),
                32_768,
                8_192,
            )
            .unwrap(),
        )
    }

    fn stage(quota: &Arc<CacheQuota>, path: &Path) -> (tempfile::NamedTempFile, CacheReservation) {
        let mut temporary = tempfile::NamedTempFile::new_in(&quota.root).unwrap();
        let mut reservation = quota
            .reserve(write_reservation_bytes(CONTENT.len() as u64))
            .unwrap();
        reservation.track_immutable(path, temporary.path()).unwrap();
        write_synced(temporary.as_file_mut(), CONTENT).unwrap();
        (temporary, reservation)
    }

    #[test]
    fn failed_staging_leaves_an_external_destination_unregistered() {
        let dir = tempfile::tempdir().unwrap();
        let quota = open(dir.path());
        let path = quota.root.join("snapshot.json");
        let (temporary, reservation) = stage(&quota, &path);
        fs::write(&path, b"external").unwrap();
        let temporary_path = temporary.path().to_owned();
        drop(temporary);
        drop(reservation);
        assert!(!temporary_path.exists());
        assert_eq!(
            quota.remove(&path).unwrap_err().code,
            "PROCESSING_UNKNOWN_FILE"
        );
        assert_eq!(fs::read(&path).unwrap(), b"external");
        assert_eq!(quota.available_bytes().unwrap(), 24_576);
    }

    #[test]
    fn failed_ledger_commit_rolls_back_an_owned_installation() {
        let dir = tempfile::tempdir().unwrap();
        let quota = open(dir.path());
        let path = quota.root.join("snapshot.json");
        let (temporary, reservation) = stage(&quota, &path);
        fs::create_dir(quota.root.join(".cache-ledger.next")).unwrap();
        let error = install_immutable(&path, Some(reservation), temporary, |_| {
            panic!("destination was vacant");
        })
        .unwrap_err();
        assert_eq!(error.code, "IO_FAILED");
        assert!(!path.exists());
        assert_eq!(quota.available_bytes().unwrap(), 24_576);
    }

    #[test]
    fn verified_external_snapshot_survives_ledger_failure_and_remains_charged() {
        for fail_save in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let quota = open(dir.path());
            let path = quota.root.join("snapshot.json");
            let (temporary, reservation) = stage(&quota, &path);
            let temporary_path = temporary.path().to_owned();
            fs::write(&path, CONTENT).unwrap();
            let blocker = quota.root.join(".cache-ledger.next");
            let result = install_immutable(&path, Some(reservation), temporary, |_| {
                assert_eq!(fs::read(&path)?, CONTENT);
                if fail_save {
                    fs::create_dir(&blocker)?;
                }
                Ok(())
            });
            if fail_save {
                assert_eq!(result.unwrap_err().code, "IO_FAILED");
                fs::remove_dir(&blocker).unwrap();
            } else {
                result.unwrap();
            }
            assert!(!temporary_path.exists());
            assert_eq!(fs::read(&path).unwrap(), CONTENT);
            assert_eq!(
                quota.available_bytes().unwrap(),
                24_576 - CONTENT.len() as u64
            );
            drop(quota);
            assert_eq!(
                open(dir.path()).used_bytes().unwrap(),
                8_192 + CONTENT.len() as u64
            );
        }
    }
}
