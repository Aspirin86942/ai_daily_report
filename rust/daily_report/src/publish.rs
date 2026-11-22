use crate::{
    processing_cache::write_immutable,
    reports::personal::{EvidenceAttachment, PersonalReport},
    snapshots::{digest, reject_redirects},
    store::{RunRecord, Store},
    types::{parse_date, RunPhase},
    ReportError,
};
use chrono::{DateTime, Utc};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};

pub struct Publisher<'a> {
    pub root: PathBuf,
    pub store: &'a Store,
}

pub(crate) fn output_hash(path: &Path) -> Result<Option<String>, ReportError> {
    reject_redirects(path, "OUTPUT_PATH_REDIRECTED")?;
    match File::open(path) {
        Ok(file) => Ok(Some(daily_parsers::reader_sha256(file)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn attachment(
    report_json: &str,
    validate_detailed: impl FnOnce(&PersonalReport) -> Result<(), ReportError>,
) -> Result<Option<(String, EvidenceAttachment)>, ReportError> {
    let value = crate::snapshots::read_frozen_report_json(report_json)?;
    if value["evidence_attachment"].is_null() && value["schema_version"] != 6 {
        return Ok(None);
    }
    let report: PersonalReport = serde_json::from_value(value)?;
    if report.schema_version == 6 {
        validate_detailed(&report)?;
    } else {
        report.validate_saved()?;
    }
    Ok(report.evidence_attachment.map(|a| (report.date, a)))
}

pub(crate) fn verify_attachment(target: &Path, report_json: &str) -> Result<(), ReportError> {
    if let Some((date, evidence)) = attachment(report_json, |report| {
        reject_redirects(target, "OUTPUT_PATH_REDIRECTED")?;
        report.validate_output_size(fs::metadata(target)?.len())?;
        report.validate_output(&fs::read_to_string(target)?)
    })? {
        let path = target
            .parent()
            .ok_or_else(|| ReportError::new("OUTPUT_PATH_INVALID", "日报路径缺少目录"))?
            .join(evidence.link(&date));
        verify_attachment_file(&path, &evidence.sha256)?;
    }
    Ok(())
}

fn verify_attachment_file(path: &Path, expected: &str) -> Result<(), ReportError> {
    if output_hash(path)?.as_deref() != Some(expected) {
        return Err(ReportError::new(
            "REPORT_EVIDENCE_MISMATCH",
            "依据附件缺失或内容已改变",
        ));
    }
    Ok(())
}

fn plain_path(path: &Path) -> Result<PathBuf, ReportError> {
    let absolute = std::path::absolute(path)?;
    #[cfg(windows)]
    if absolute.to_str().is_none() {
        return Err(ReportError::new(
            "OUTPUT_PATH_INVALID",
            "报告路径不是有效文字",
        ));
    }
    Ok(crate::processing_cache::normalized(&absolute))
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
        Ok(self.legacy_path(kind, &label))
    }

    fn legacy_path(&self, kind: &str, label: &str) -> PathBuf {
        self.root.join(kind).join(format!("{label}.md"))
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

    fn conflict(&self, run: &RunRecord, body: &[u8]) -> Result<(), ReportError> {
        let path = self
            .root
            .join("conflicts")
            .join(format!("{}.{}.md", run.target_date, run.run_id));
        self.check(&path)?;
        let verify = || {
            if output_hash(&path)? != run.expected_sha256 {
                return Err(ReportError::new(
                    "OUTPUT_CONFLICT_COPY_MODIFIED",
                    "已保存的冲突副本发生修改",
                ));
            }
            Ok(())
        };
        if path.exists() {
            return verify();
        }
        write_immutable(&path, body, None, "OUTPUT_PATH_REDIRECTED", |_| verify())
    }

    fn publish_attachment(
        &self,
        attachment: &Option<(String, EvidenceAttachment)>,
        target: &Path,
    ) -> Result<(), ReportError> {
        let Some((date, evidence)) = attachment else {
            return Ok(());
        };
        let path = target.parent().unwrap().join(evidence.link(date));
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
        write_immutable(
            &path,
            evidence.markdown.as_bytes(),
            None,
            "OUTPUT_PATH_REDIRECTED",
            |_| Ok(()),
        )?;
        verify_attachment_file(&path, &evidence.sha256)
    }

    pub fn publish(&self, run_id: &str, now: DateTime<Utc>) -> Result<PathBuf, ReportError> {
        self.publish_inner(run_id, now, None)
    }

    pub fn publish_with_config(
        &self,
        run_id: &str,
        now: DateTime<Utc>,
        config: &crate::config::AppConfig,
    ) -> Result<PathBuf, ReportError> {
        self.publish_inner(run_id, now, Some(config))
    }

    fn publish_inner(
        &self,
        run_id: &str,
        now: DateTime<Utc>,
        config: Option<&crate::config::AppConfig>,
    ) -> Result<PathBuf, ReportError> {
        let run = self.store.get(run_id)?;
        let staged = run.phase == RunPhase::DbCommitted;
        let markdown = run
            .markdown
            .as_deref()
            .filter(|body| !body.is_empty() && (staged || run.phase == RunPhase::Published))
            .ok_or_else(|| ReportError::new("REPORT_NOT_STAGED", "报告正文尚未提交数据库"))?;
        let period_label = if matches!(run.run_kind.as_str(), "weekly" | "monthly") {
            let raw = run
                .report_json
                .as_deref()
                .ok_or_else(|| ReportError::new("REPORT_NOT_STAGED", "周月报正文缺失"))?;
            let report = crate::snapshots::read_frozen_report_json(raw)?;
            Some(crate::reports::period::validate_identity(
                &run.run_kind,
                &report,
            )?)
        } else {
            None
        };
        let target = match &period_label {
            Some(label) => self.legacy_path(&run.run_kind, label),
            None => self.output_path(&run.target_date)?,
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
        let body = markdown.as_bytes();
        let expected = digest(body);
        if run.expected_sha256.as_deref() != Some(&expected) {
            return Err(ReportError::new(
                "STAGED_REPORT_HASH_MISMATCH",
                "已提交报告正文的哈希不符",
            ));
        }
        let current = self
            .store
            .publication_pointer(&run, period_label.as_deref())?;
        let expected_current = if staged {
            run.previous_run_id.as_deref()
        } else {
            Some(run_id)
        };
        if current.as_deref() != expected_current {
            return Err(ReportError::new(
                "PUBLICATION_VERSION_CHANGED",
                "正式报告版本已经变化",
            ));
        }
        let evidence = if period_label.is_some() {
            None
        } else {
            attachment(run.report_json.as_deref().unwrap_or("{}"), |report| {
                report.validate_output(markdown)?;
                if staged && report.sources.iter().any(|s| s.source_kind == "file") {
                    let config = config.ok_or_else(crate::evidence::permission_denied)?;
                    if !crate::evidence::denied_sources(config, report.sources.iter())?.is_empty() {
                        return Err(crate::evidence::permission_denied());
                    }
                }
                Ok(())
            })?
        };
        let existing = output_hash(&target)?;
        let already_written = existing == run.expected_sha256;
        if already_written && run.error_code.as_deref() == Some("OUTPUT_CONFLICT_CAPTURED") {
            return Err(ReportError::new(
                "OUTPUT_CONFLICT_CAPTURED",
                "替换期间的外部修改已保存，需先处理冲突",
            ));
        }
        if !already_written && existing.is_some() && !(staged && existing == run.previous_sha256) {
            self.conflict(&run, body)?;
            return Err(ReportError::new(
                "OUTPUT_CONFLICT",
                "已有报告被外部修改，候选正文已保存到冲突目录",
            ));
        }
        // Publish the attachment first; a visible summary must have usable evidence.
        self.publish_attachment(&evidence, &target)?;
        if !already_written {
            self.write_summary(&run, &target, body, existing, now)?;
        }
        if staged {
            self.store.mark_published(run_id, &expected, now)?;
        }
        Ok(target)
    }

    fn write_summary(
        &self,
        run: &RunRecord,
        target: &Path,
        body: &[u8],
        existing: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<(), ReportError> {
        let run_id = &run.run_id;
        let directory = target.parent().unwrap();
        fs::create_dir_all(directory)?;
        self.check(target)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        crate::maintenance::write_synced(temporary.as_file_mut(), body)?;
        let temporary = temporary.into_temp_path();
        if output_hash(target)? != existing {
            self.conflict(run, body)?;
            return Err(ReportError::new("OUTPUT_CONFLICT", "发布期间发现外部修改"));
        }
        if existing.is_none() {
            if let Err(error) = fs::hard_link(&temporary, target) {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    self.conflict(run, body)?;
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
            replace_with_backup(target, &temporary, &backup)?;
            if output_hash(&backup)? != existing {
                self.conflict(run, body)?;
                self.store
                    .fail(run_id, "OUTPUT_CONFLICT_CAPTURED", false, now)?;
                return Err(ReportError::new(
                    "OUTPUT_CONFLICT_CAPTURED",
                    "替换期间的外部修改已保留到备份",
                ));
            }
        }
        if output_hash(target)? != run.expected_sha256 {
            self.conflict(run, body)?;
            return Err(ReportError::new("OUTPUT_CONFLICT", "发布后文件哈希不符"));
        }
        Ok(())
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
