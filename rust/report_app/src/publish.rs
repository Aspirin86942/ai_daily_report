use crate::{
    snapshots::{digest, reject_redirects},
    store::{RunRecord, Store},
    types::{parse_date, RunPhase},
    ReportError,
};
use chrono::{DateTime, Utc};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub struct Publisher<'a> {
    pub root: PathBuf,
    pub store: &'a Store,
}

fn output_hash(path: &Path) -> Result<Option<String>, ReportError> {
    reject_redirects(path, "OUTPUT_PATH_REDIRECTED")?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(ReportError::new(
            "OUTPUT_FILE_TOO_LARGE",
            "既有报告文件超过大小上限",
        ));
    }
    Ok(Some(digest(&bytes)))
}

fn plain_path(path: &Path) -> Result<PathBuf, ReportError> {
    let absolute = std::path::absolute(path)?;
    #[cfg(windows)]
    {
        let text = absolute
            .to_str()
            .ok_or_else(|| ReportError::new("OUTPUT_PATH_INVALID", "报告路径不是有效文字"))?;
        if let Some(unc) = text.strip_prefix("\\\\?\\UNC\\") {
            return Ok(PathBuf::from(format!("\\\\{unc}")));
        }
        if let Some(drive) = text.strip_prefix("\\\\?\\") {
            return Ok(PathBuf::from(drive));
        }
    }
    Ok(absolute)
}

impl<'a> Publisher<'a> {
    pub fn new(root: &Path, store: &'a Store) -> Result<Self, ReportError> {
        reject_redirects(root, "OUTPUT_PATH_REDIRECTED")?;
        fs::create_dir_all(root)?;
        Ok(Self {
            root: plain_path(&fs::canonicalize(root)?)?,
            store,
        })
    }

    pub fn output_path(&self, date: &str) -> Result<PathBuf, ReportError> {
        parse_date(date)?;
        Ok(self.root.join(&date[..7]).join(format!("{date}.md")))
    }

    pub fn period_path(
        &self,
        kind: &str,
        report: &serde_json::Value,
    ) -> Result<PathBuf, ReportError> {
        let label = crate::reports::period::validate_identity(kind, report)?;
        Ok(self.root.join(kind).join(format!("{label}.md")))
    }

    fn check(&self, path: &Path) -> Result<(), ReportError> {
        if !plain_path(path)?.starts_with(&self.root) {
            return Err(ReportError::new(
                "OUTPUT_PATH_INVALID",
                "报告路径超出输出目录",
            ));
        }
        reject_redirects(path, "OUTPUT_PATH_REDIRECTED")
    }

    fn conflict(&self, run: &RunRecord, body: &[u8]) -> Result<PathBuf, ReportError> {
        let path = self
            .root
            .join("conflicts")
            .join(format!("{}.{}.md", run.target_date, run.run_id));
        self.check(&path)?;
        fs::create_dir_all(path.parent().unwrap())?;
        self.check(&path)?;
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(body)?;
                file.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if output_hash(&path)? != run.expected_sha256 {
                    return Err(ReportError::new(
                        "OUTPUT_CONFLICT_COPY_MODIFIED",
                        "已保存的冲突副本发生修改",
                    ));
                }
            }
            Err(error) => return Err(error.into()),
        }
        Ok(path)
    }

    pub fn publish(&self, run_id: &str, now: DateTime<Utc>) -> Result<PathBuf, ReportError> {
        let run = self.store.get(run_id)?;
        if !matches!(run.phase, RunPhase::DbCommitted | RunPhase::Published)
            || run.markdown.as_ref().is_none_or(String::is_empty)
        {
            return Err(ReportError::new(
                "REPORT_NOT_STAGED",
                "报告正文尚未提交数据库",
            ));
        }
        let target = if matches!(run.run_kind.as_str(), "weekly" | "monthly") {
            self.period_path(
                &run.run_kind,
                &serde_json::from_str(
                    run.report_json
                        .as_deref()
                        .ok_or_else(|| ReportError::new("REPORT_NOT_STAGED", "周月报正文缺失"))?,
                )?,
            )?
        } else {
            self.output_path(&run.target_date)?
        };
        self.check(&target)?;
        if run
            .output_path
            .as_deref()
            .map(Path::new)
            .map(plain_path)
            .transpose()?
            .as_ref()
            != Some(&target)
        {
            return Err(ReportError::new(
                "OUTPUT_LOCATION_CHANGED",
                "报告目录与已提交记录不一致",
            ));
        }
        let body = run.markdown.as_ref().unwrap().as_bytes();
        if Some(digest(body)) != run.expected_sha256 {
            return Err(ReportError::new(
                "STAGED_REPORT_HASH_MISMATCH",
                "已提交报告正文的哈希不符",
            ));
        }
        let current = self.store.publication_current(&run)?.map(|r| r.run_id);
        let expected_current = if run.phase == RunPhase::Published {
            Some(run.run_id.clone())
        } else {
            run.previous_run_id.clone()
        };
        if current != expected_current {
            return Err(ReportError::new(
                "PUBLICATION_VERSION_CHANGED",
                "正式报告版本已经变化",
            ));
        }
        let existing = output_hash(&target)?;
        if run.error_code.as_deref() == Some("OUTPUT_CONFLICT_CAPTURED")
            && existing == run.expected_sha256
        {
            return Err(ReportError::new(
                "OUTPUT_CONFLICT_CAPTURED",
                "替换期间的外部修改已保存，需先处理冲突",
            ));
        }
        if existing == run.expected_sha256 {
            if run.phase == RunPhase::DbCommitted {
                self.store
                    .mark_published(run_id, existing.as_deref().unwrap(), now)?;
            }
            return Ok(target);
        }
        let allowed = if run.phase == RunPhase::Published {
            &run.expected_sha256
        } else {
            &run.previous_sha256
        };
        if existing.is_some() && &existing != allowed {
            self.conflict(&run, body)?;
            return Err(ReportError::new(
                "OUTPUT_CONFLICT",
                "已有报告被外部修改，候选正文已保存到冲突目录",
            ));
        }
        fs::create_dir_all(target.parent().unwrap())?;
        self.check(&target)?;
        let mut temporary = tempfile::NamedTempFile::new_in(target.parent().unwrap())?;
        temporary.write_all(body)?;
        temporary.as_file().sync_all()?;
        let temporary = temporary.into_temp_path();
        if output_hash(&target)? != existing {
            self.conflict(&run, body)?;
            return Err(ReportError::new("OUTPUT_CONFLICT", "发布期间发现外部修改"));
        }
        if existing.is_none() {
            if let Err(error) = fs::hard_link(&temporary, &target) {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    self.conflict(&run, body)?;
                    return Err(ReportError::new(
                        "OUTPUT_CONFLICT",
                        "发布期间出现新的报告文件",
                    ));
                }
                return Err(error.into());
            }
        } else {
            let directory = self.root.join("conflicts");
            self.check(&directory)?;
            fs::create_dir_all(&directory)?;
            let backup = tempfile::Builder::new()
                .prefix(&format!("{}.{}.", run.target_date, run_id))
                .suffix(".replaced.md")
                .tempfile_in(&directory)?
                .into_temp_path()
                .keep()
                .map_err(|_| ReportError::new("OUTPUT_BACKUP_FAILED", "无法预留替换备份"))?;
            // This empty file was created above solely to reserve a unique name.
            fs::remove_file(&backup)?;
            replace_with_backup(&target, &temporary, &backup)?;
            if output_hash(&backup)? != existing {
                self.conflict(&run, body)?;
                self.store
                    .fail(run_id, "OUTPUT_CONFLICT_CAPTURED", false, now)?;
                return Err(ReportError::new(
                    "OUTPUT_CONFLICT_CAPTURED",
                    "替换期间的外部修改已保留到备份",
                ));
            }
        }
        let actual = output_hash(&target)?;
        if actual != run.expected_sha256 {
            self.conflict(&run, body)?;
            return Err(ReportError::new("OUTPUT_CONFLICT", "发布后文件哈希不符"));
        }
        if run.phase == RunPhase::DbCommitted {
            self.store
                .mark_published(run_id, actual.as_deref().unwrap(), now)?;
        }
        Ok(target)
    }
}

#[cfg(windows)]
fn replace_with_backup(target: &Path, candidate: &Path, backup: &Path) -> Result<(), ReportError> {
    use std::os::windows::ffi::OsStrExt;
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let (target, candidate, backup) = (wide(target), wide(candidate), wide(backup));
    let replaced = unsafe {
        windows_sys::Win32::Storage::FileSystem::ReplaceFileW(
            target.as_ptr(),
            candidate.as_ptr(),
            backup.as_ptr(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        return Err(ReportError::new(
            "OUTPUT_REPLACE_FAILED",
            "报告替换失败，数据库正文仍可恢复",
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_with_backup(target: &Path, candidate: &Path, backup: &Path) -> Result<(), ReportError> {
    fs::hard_link(target, backup)?;
    fs::rename(candidate, target)?;
    Ok(())
}
