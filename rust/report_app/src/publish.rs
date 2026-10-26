use crate::{
    reports::personal::{EvidenceAttachment, PersonalReport},
    snapshots::{digest, reject_redirects},
    store::{RunRecord, Store},
    types::{parse_date, RunPhase},
    ReportError,
};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
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
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // Evidence appendices can exceed 8 MiB. Hash every byte with bounded memory.
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(Some(ai_daily_worker_contract::archive::hex(
        &hash.finalize(),
    )))
}

fn attachment(report_json: &str) -> Result<Option<(String, EvidenceAttachment)>, ReportError> {
    let value: serde_json::Value = serde_json::from_str(report_json)?;
    if value["evidence_attachment"].is_null() {
        return Ok(None);
    }
    let report: PersonalReport = serde_json::from_value(value)?;
    report.validate()?;
    Ok(report.evidence_attachment.map(|a| (report.date, a)))
}

pub(crate) fn verify_attachment(target: &Path, report_json: &str) -> Result<(), ReportError> {
    if let Some((date, evidence)) = attachment(report_json)? {
        let path = target
            .parent()
            .ok_or_else(|| ReportError::new("OUTPUT_PATH_INVALID", "日报路径缺少目录"))?
            .join(evidence.link(&date));
        if output_hash(&path)?.as_deref() != Some(&evidence.sha256) {
            return Err(ReportError::new(
                "REPORT_EVIDENCE_MISMATCH",
                "依据附件缺失或内容已改变",
            ));
        }
    }
    Ok(())
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

    fn publish_attachment(&self, run: &RunRecord, target: &Path) -> Result<(), ReportError> {
        let Some((date, evidence)) = attachment(run.report_json.as_deref().unwrap_or("{}"))? else {
            return Ok(());
        };
        let path = target.parent().unwrap().join(evidence.link(&date));
        self.check(&path)?;
        if let Some(hash) = output_hash(&path)? {
            if hash == evidence.sha256 {
                return Ok(());
            }
            return Err(ReportError::new(
                "REPORT_EVIDENCE_CONFLICT",
                "已有依据附件被修改，保留原文件",
            ));
        }
        fs::create_dir_all(path.parent().unwrap())?;
        self.check(&path)?;
        let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        temporary.write_all(evidence.markdown.as_bytes())?;
        temporary.as_file().sync_all()?;
        // The content hash gives each revision an immutable name. Linking the
        // complete temporary file never overwrites an existing attachment.
        if let Err(error) = fs::hard_link(temporary.path(), &path) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        verify_attachment(target, run.report_json.as_deref().unwrap_or("{}"))
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
            self.publish_attachment(&run, &target)?;
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
        // Publish the attachment first; a visible summary must have usable evidence.
        self.publish_attachment(&run, &target)?;
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
