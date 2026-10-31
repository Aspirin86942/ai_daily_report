//! One OS lock and one shared reservation pool for every personal processing writer.
use crate::{
    config::AppConfig, snapshots::reject_redirects, store::locking::ReportLock, ReportError,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const MAX_BYTES: u64 = 1024 * 1024 * 1024;
pub const CONTROL_BYTES: u64 = 72 * 1024 * 1024;
pub const MAX_INPUT_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
pub const RETENTION_DAYS: i64 = 7;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        config: &AppConfig,
        now: DateTime<Utc>,
        deadline: Instant,
    ) -> Result<Self, ReportError> {
        Self::at(
            &config.paths.data_dir.join("daily_cache"),
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
        let root = normalized(&std::path::absolute(root)?);
        let ledger = root.join(".cache-ledger.json");
        let entries: BTreeMap<String, Entry> = if ledger.exists() {
            if fs::metadata(&ledger)?.len() > (control_bytes / 4).min(2 * 1024 * 1024) {
                return Err(full());
            }
            serde_json::from_slice(&fs::read(&ledger)?)?
        } else {
            BTreeMap::new()
        };
        let mut quota = Self {
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
        // A prior owner has exited. Keep registered writes for capture-receipt recovery;
        // their real bytes count immediately and their original expiry is not extended.
        for entry in quota
            .state
            .get_mut()
            .map_err(|_| full())?
            .entries
            .values_mut()
        {
            entry.pending = false;
        }
        let (used, control_used) = quota.measure()?;
        if control_used > control_bytes {
            return Err(full());
        }
        quota.state.get_mut().map_err(|_| full())?.used = used;
        quota.save(&*quota.state.lock().map_err(|_| full())?)?;
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
        let path = normalized(&std::path::absolute(path)?);
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

    fn measure(&self) -> Result<(u64, u64), ReportError> {
        let state = self.state.lock().map_err(|_| full())?;
        let mut used = 0u64;
        let mut controls = 0u64;
        for entry in walkdir::WalkDir::new(&self.root)
            .follow_links(false)
            .max_open(16)
        {
            check_deadline(self.deadline)?;
            let entry =
                entry.map_err(|_| ReportError::new("PROCESSING_READ_FAILED", "无法核对处理区"))?;
            reject_redirects(entry.path(), "PROCESSING_PATH_REDIRECTED")?;
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let size = entry.metadata().map_err(|_| full())?.len();
            if control(&relative) {
                controls = controls.saturating_add(size);
            } else if state.entries.keys().any(|key| {
                relative == *key
                    || relative
                        .strip_prefix(key)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            }) {
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
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(next, self.root.join(".cache-ledger.json"))?;
        Ok(())
    }

    pub fn reserve(self: &Arc<Self>, bytes: u64) -> Result<CacheReservation, ReportError> {
        check_deadline(self.deadline)?;
        let mut state = self.state.lock().map_err(|_| full())?;
        if state
            .used
            .saturating_add(state.reserved)
            .saturating_add(bytes)
            .saturating_add(self.control_bytes)
            > self.max_bytes
        {
            return Err(full());
        }
        state.reserved += bytes;
        Ok(CacheReservation {
            quota: Arc::clone(self),
            bytes,
            paths: vec![],
            done: false,
        })
    }

    pub fn used_bytes(&self) -> Result<u64, ReportError> {
        Ok(self.state.lock().map_err(|_| full())?.used + self.control_bytes)
    }

    pub fn write(self: &Arc<Self>, path: &Path, bytes: &[u8]) -> Result<(), ReportError> {
        let mut reservation = self.reserve(bytes.len() as u64 * 2)?;
        reservation.track(path)?;
        fs::create_dir_all(path.parent().ok_or_else(full)?)?;
        let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        reservation.track_existing_empty(temp.path())?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(path)
            .map_err(|e| ReportError::from(e.error))?;
        reservation.commit(&[path.to_owned()])
    }

    pub fn relocate(&self, source: &Path, destination: &Path) -> Result<(), ReportError> {
        let old = self.relative(source)?;
        let new = self.relative(destination)?;
        let mut state = self.state.lock().map_err(|_| full())?;
        let entry = state
            .entries
            .get(&old)
            .cloned()
            .ok_or_else(|| ReportError::new("PROCESSING_UNKNOWN_FILE", "恢复来源未登记"))?;
        if destination.exists() {
            return Err(ReportError::new(
                "PROCESSING_WRITE_CONFLICT",
                "恢复目标已经存在",
            ));
        }
        state.entries.insert(new, entry);
        self.save(&state)?;
        fs::create_dir_all(destination.parent().ok_or_else(full)?)?;
        fs::rename(source, destination)?;
        state.entries.remove(&old);
        self.save(&state)
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

    pub fn reclaim_candidates(&self) -> Result<Vec<PathBuf>, ReportError> {
        let state = self.state.lock().map_err(|_| full())?;
        let mut entries: Vec<_> = state
            .entries
            .iter()
            .filter(|(_, e)| !e.pending && e.created < self.now)
            .collect();
        entries.sort_by_key(|(path, e)| (e.created, *path));
        entries
            .into_iter()
            .map(|(path, _)| self.path(path))
            .collect()
    }

    pub fn available_bytes(&self) -> Result<u64, ReportError> {
        let state = self.state.lock().map_err(|_| full())?;
        Ok(self
            .max_bytes
            .saturating_sub(self.control_bytes)
            .saturating_sub(state.used)
            .saturating_sub(state.reserved))
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
        let size = self.size(path)?;
        self.remove_path(path)?;
        state.used = state.used.saturating_sub(size);
        state.entries.remove(&relative);
        self.save(&state)
    }

    fn size(&self, path: &Path) -> Result<u64, ReportError> {
        if !path.exists() {
            return Ok(0);
        }
        let mut size = 0u64;
        for entry in walkdir::WalkDir::new(path).follow_links(false) {
            check_deadline(self.deadline)?;
            let entry = entry.map_err(|_| full())?;
            reject_redirects(entry.path(), "PROCESSING_PATH_REDIRECTED")?;
            if entry.file_type().is_file() {
                size = size.saturating_add(entry.metadata().map_err(|_| full())?.len());
            }
        }
        Ok(size)
    }

    fn remove_path(&self, path: &Path) -> Result<(), ReportError> {
        self.relative(path)?;
        self.size(path)?; // Check every descendant before recursive deletion.
        if path.is_dir() {
            fs::remove_dir_all(path)?;
        } else if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

pub struct CacheReservation {
    quota: Arc<CacheQuota>,
    bytes: u64,
    paths: Vec<PathBuf>,
    done: bool,
}

impl CacheReservation {
    pub fn track_empty_directory(&mut self, path: &Path) -> Result<(), ReportError> {
        if !path.is_dir() || fs::read_dir(path)?.next().is_some() {
            return Err(ReportError::new(
                "PROCESSING_WRITE_CONFLICT",
                "暂存目录必须为空",
            ));
        }
        self.track_inner(path, true)
    }
    pub fn track_existing_empty(&mut self, path: &Path) -> Result<(), ReportError> {
        if fs::metadata(path)?.len() != 0 {
            return Err(ReportError::new(
                "PROCESSING_WRITE_CONFLICT",
                "暂存文件必须为空",
            ));
        }
        self.track_inner(path, true)
    }
    pub fn track(&mut self, path: &Path) -> Result<(), ReportError> {
        self.track_inner(path, false)
    }
    fn track_inner(&mut self, path: &Path, empty: bool) -> Result<(), ReportError> {
        let relative = self.quota.relative(path)?;
        let mut state = self.quota.state.lock().map_err(|_| full())?;
        if state.entries.contains_key(&relative) || !empty && path.exists() {
            return Err(ReportError::new(
                "PROCESSING_WRITE_CONFLICT",
                "处理路径已经存在",
            ));
        }
        state.entries.insert(
            relative,
            Entry {
                created: self.quota.now,
                pending: true,
            },
        );
        self.quota.save(&state)?;
        self.paths.push(path.to_owned());
        Ok(())
    }

    pub fn commit(mut self, paths: &[PathBuf]) -> Result<(), ReportError> {
        let mut total = 0u64;
        for path in &self.paths {
            total = total.saturating_add(self.quota.size(path)?);
        }
        if total > self.bytes || paths.iter().any(|p| !self.paths.contains(p)) {
            return Err(full());
        }
        let mut state = self.quota.state.lock().map_err(|_| full())?;
        for path in &self.paths {
            let relative = self.quota.relative(path)?;
            if paths.contains(path) {
                state.entries.get_mut(&relative).ok_or_else(full)?.pending = false;
            } else {
                self.quota.remove_path(path)?;
                state.entries.remove(&relative);
            }
        }
        state.used = state.used.saturating_add(
            paths
                .iter()
                .map(|p| self.quota.size(p))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .sum::<u64>(),
        );
        state.reserved = state.reserved.saturating_sub(self.bytes);
        self.quota.save(&state)?;
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
            let mut residual = 0;
            for path in &self.paths {
                // If cleanup fails, preserve a pending entry for the next lock owner.
                if self.quota.remove_path(path).is_ok() {
                    if let Ok(relative) = self.quota.relative(path) {
                        state.entries.remove(&relative);
                    }
                } else {
                    residual += self.quota.size(path).unwrap_or(self.bytes);
                }
            }
            state.used = state.used.saturating_add(residual);
            state.reserved = state.reserved.saturating_sub(self.bytes);
            let _ = self.quota.save(&state);
        }
    }
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

pub(crate) fn normalized(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(local) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(local);
        }
    }
    path.to_owned()
}
