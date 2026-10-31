//! Only observed, immutable changes enter a report. Publication owns coverage.

use super::{
    diff, error, identity,
    store::{atomic_write, ArchiveStore},
};
use crate::{
    config::AppConfig,
    model::{
        contracts::SelectedFact,
        plan::PlannedStep,
        transport::{HttpResponse, ModelTransport, TransportFailure},
    },
    run::FrozenInput,
    snapshots::digest,
    store::RunRecord,
    ReportError,
};
use ai_daily_worker_contract::{archive::ArchiveUnit, SourceRange};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    pub source_id: String,
    pub version_id: String,
    pub extraction_id: String,
    pub unit_id: String,
    pub side: String,
    pub raw_sha256: String,
    pub unit_sha256: String,
    pub quote: String,
    pub quote_sha256: String,
    pub unit_char_start: usize,
    pub input_start: usize,
    pub input_end: usize,
    pub ranges: Vec<SourceRange>,
}

fn comparable(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .trim_end_matches('/')
        .to_lowercase()
}
fn within(path: &Path, root: &Path) -> bool {
    let path = comparable(path);
    let root = comparable(root);
    path == root
        || path
            .strip_prefix(&root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
pub fn permitted(config: &AppConfig, path: &Path) -> bool {
    // Additional roots stay local until explicitly granted, including nested roots.
    let allowed = config
        .archive
        .model_roots
        .iter()
        .any(|root| within(path, root))
        || config.archive.allow_work_dir_for_model
            && within(path, &config.paths.work_dir)
            && !config.archive.roots.iter().any(|root| within(path, root));
    if !allowed {
        return false;
    }
    let Ok(profile) = ai_daily_scanner_core::config::normalize_scanner_settings(
        &config.scanner,
        ai_daily_scanner_contract::ReportMode::Daily,
    ) else {
        return false;
    };
    let excluded: Vec<_> = profile
        .discovery
        .excluded_dirs
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    ai_daily_discovery::archived_path_allowed(
        path,
        &config.paths.work_dir,
        &profile.discovery.allowed_extensions,
        &profile.discovery.ignored_patterns,
        &excluded,
    )
    .unwrap_or(false)
}

pub fn recovery_runs(
    config: &AppConfig,
    runs: Vec<RunRecord>,
    archive_source: bool,
) -> Result<Vec<RunRecord>, ReportError> {
    if runs.is_empty() {
        return Ok(runs);
    }
    let store = match ArchiveStore::open(config, false) {
        Ok(store) => store,
        Err(error) if error.code == "ARCHIVE_NOT_INITIALIZED" && !archive_source => {
            return Ok(runs)
        }
        Err(error) => return Err(error),
    };
    // Every archive input is pinned before its run becomes recoverable, even with no changes.
    let mut statement = store
        .conn
        .prepare("SELECT EXISTS(SELECT 1 FROM report_evidence WHERE run_id=?)")?;
    let mut selected = Vec::with_capacity(runs.len());
    for run in runs {
        let archived: bool = statement.query_row([&run.run_id], |row| row.get(0))?;
        if archived == archive_source {
            selected.push(run);
        }
    }
    Ok(selected)
}

pub fn pin(config: &AppConfig, input: &FrozenInput) -> Result<(), ReportError> {
    if input.version != 4 {
        return Ok(());
    }
    let mut store = ArchiveStore::open(config, true)?;
    let run = &input.context.run_id;
    let relative = format!("report_evidence/{run}/input.json");
    let hash = store.put_json(&store.root.join(&relative), input)?;
    let tx = store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for reference in input.sources.iter().flat_map(|s| {
        s.coverage_limits["archive"]["evidence"]
            .as_array()
            .into_iter()
            .flatten()
    }) {
        let id = reference["version_id"]
            .as_str()
            .ok_or_else(|| error("ARCHIVE_EVIDENCE_INVALID"))?;
        let available: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM versions WHERE id=? AND pruned_at IS NULL)",
            [id],
            |r| r.get(0),
        )?;
        if !available {
            return Err(error("ARCHIVE_EVIDENCE_PRUNED"));
        }
    }
    tx.execute("INSERT OR IGNORE INTO report_evidence(run_id,target_date,status,path,sha256,created_at) VALUES(?,?,'frozen',?,?,?)",params![run,input.context.target_date.to_string(),relative,hash,input.context.started_at.to_rfc3339()])?;
    for change in input.plan.target["archive_changes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let id = change
            .as_str()
            .ok_or_else(|| error("ARCHIVE_CHANGE_INVALID"))?;
        tx.execute(
            "INSERT OR IGNORE INTO report_changes VALUES(?,?)",
            params![run, id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO report_versions SELECT ?,version_id FROM changes WHERE id=?",
            params![run, id],
        )?;
        tx.execute("INSERT OR IGNORE INTO report_versions SELECT ?,before_version FROM changes WHERE id=? AND before_version IS NOT NULL",params![run,id])?;
    }
    tx.commit()?;
    Ok(())
}

pub fn read_frozen(config: &AppConfig, run: &str, hash: &str) -> Result<Value, ReportError> {
    let store = ArchiveStore::open(config, false)?;
    let (path, expected): (String, String) = store.conn.query_row(
        "SELECT path,sha256 FROM report_evidence WHERE run_id=?",
        [run],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let bytes = fs::read(store.root.join(path))?;
    if expected != hash || digest(&bytes) != hash {
        return Err(error("ARCHIVE_REPORT_INPUT_CORRUPT"));
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn published(config: &AppConfig, run: &str, markdown: &str) -> Result<(), ReportError> {
    if !config.paths.data_dir.join("archive/index.sqlite3").exists() {
        return Ok(());
    }
    let store = ArchiveStore::open(config, true)?;
    let exists: bool = store.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM report_evidence WHERE run_id=?)",
        [run],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let path = store
        .root
        .join("report_evidence")
        .join(run)
        .join("report.md");
    if path.exists() {
        if fs::read(&path)? != markdown.as_bytes() {
            return Err(error("ARCHIVE_IMMUTABLE_CONFLICT"));
        }
    } else {
        atomic_write(&path, markdown.as_bytes())?;
    }
    store.conn.execute(
        "UPDATE report_evidence SET status='published',markdown_sha256=? WHERE run_id=?",
        params![digest(markdown.as_bytes()), run],
    )?;
    Ok(())
}

pub fn validate_permissions(config: &AppConfig, input: &FrozenInput) -> Result<(), ReportError> {
    for source in &input.sources {
        if let Some(path) = source.coverage_limits["archive"]["path"].as_str() {
            if !permitted(config, Path::new(path)) {
                return Err(error("ARCHIVE_MODEL_PERMISSION_DENIED"));
            }
        }
    }
    Ok(())
}
pub struct ProtectedTransport<'a> {
    pub config: &'a AppConfig,
    pub input: &'a FrozenInput,
    pub inner: &'a dyn ModelTransport,
}
impl ProtectedTransport<'_> {
    fn check(&self) -> Result<(), ReportError> {
        if self.input.version != 4 {
            return Ok(());
        }
        validate_permissions(self.config, self.input)?;
        let current = AppConfig::load(&self.config.program_dir, Some(&self.config.config_dir))?;
        validate_permissions(&current, self.input)
    }
}
impl ModelTransport for ProtectedTransport<'_> {
    fn prepare(&self) -> Result<(), ReportError> {
        self.check()?;
        self.inner.prepare()
    }
    fn send(&self, request: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure> {
        self.check()
            .map_err(|_| TransportFailure::PermissionDenied)?;
        self.inner.send(request, timeout)
    }
}

/// Bind exact excerpts that were present in this effective (possibly split) request.
pub fn bind_facts(step: &PlannedStep, facts: &mut [SelectedFact]) -> Result<(), ReportError> {
    if step.version != 5 {
        return Ok(());
    }
    for fact in facts {
        let mut matters = BTreeSet::new();
        for source in &fact.fact.source_ids {
            let metadata = &step.source_map[source]["archive"];
            if source == "user_input" {
                continue;
            }
            matters.insert(
                metadata["matter_id"]
                    .as_str()
                    .ok_or_else(|| error("ARCHIVE_EVIDENCE_MISSING"))?
                    .to_owned(),
            );
            let refs: Vec<EvidenceRef> = serde_json::from_value(metadata["evidence"].clone())?;
            for reference in refs {
                for range in step
                    .entries
                    .iter()
                    .filter(|entry| entry.source_id == *source)
                    .flat_map(|entry| &entry.text_ranges)
                {
                    let start = reference.input_start.max(range.start);
                    let end = reference.input_end.min(range.end);
                    if start >= end {
                        continue;
                    }
                    let mut used = reference.clone();
                    used.quote = reference
                        .quote
                        .chars()
                        .skip(start - reference.input_start)
                        .take(end - start)
                        .collect();
                    used.unit_char_start += start - reference.input_start;
                    used.input_start = start;
                    used.input_end = end;
                    used.quote_sha256 = digest(used.quote.as_bytes());
                    if !fact.evidence.contains(&used) {
                        fact.evidence.push(used);
                    }
                }
            }
        }
        fact.matter_id = Some(if matters.len() == 1 {
            matters.into_iter().next().unwrap()
        } else {
            identity(&["ambiguous-matter", &fact.fact_id])
        });
        if fact.fact.source_ids.iter().any(|id| id != "user_input") && fact.evidence.is_empty() {
            return Err(error("ARCHIVE_EVIDENCE_MISSING"));
        }
        let fact_text = format!(
            "{} {} {} {}",
            fact.fact.topic,
            fact.fact.detail,
            fact.fact.conditions.join(" "),
            fact.fact.status.as_deref().unwrap_or("")
        );
        let sensitive = [
            "金额", "元", "审批", "完成", "阻塞", "门槛", "条件", "¥", "￥",
        ]
        .iter()
        .any(|word| fact_text.contains(word));
        let missing_pair = fact
            .fact
            .source_ids
            .iter()
            .filter(|id| {
                matches!(
                    step.source_map[*id]["archive"]["kind"].as_str(),
                    Some("modified" | "removed")
                )
            })
            .any(|id| {
                !["before", "after"].into_iter().all(|side| {
                    fact.evidence
                        .iter()
                        .any(|r| r.source_id == *id && r.side == side)
                })
            });
        let incomplete = fact
            .fact
            .source_ids
            .iter()
            .any(|id| step.source_map[id]["archive"]["comparison_incomplete"] == true);
        if (sensitive && missing_pair) || incomplete {
            fact.pending = true;
            fact.fact.uncertainty = Some("前后依据未同时覆盖，变化结论待确认。".into());
        }
    }
    Ok(())
}

pub fn validate_evidence(config: &AppConfig, facts: &[SelectedFact]) -> Result<(), ReportError> {
    let store = ArchiveStore::open(config, false)?;
    for fact in facts {
        for evidence in &fact.evidence {
            let version = store.version(&evidence.version_id)?;
            if version.sha256 != evidence.raw_sha256
                || !fact.fact.source_ids.contains(&evidence.source_id)
            {
                return Err(error("ARCHIVE_EVIDENCE_INVALID"));
            }
            let linked:bool=store.conn.query_row("SELECT EXISTS(SELECT 1 FROM version_extractions WHERE version_id=? AND extraction_id=?)",params![evidence.version_id,evidence.extraction_id],|r|r.get(0))?;
            if !linked {
                return Err(error("ARCHIVE_EVIDENCE_INVALID"));
            }
            let number = evidence
                .unit_id
                .strip_prefix('u')
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| error("ARCHIVE_EVIDENCE_INVALID"))?;
            let dir = store.extraction_dir(&evidence.extraction_id)?;
            let unit: ArchiveUnit = diff::unit(&dir, number)?;
            let original = diff::text(&dir, &unit)?;
            let quote: String = original
                .chars()
                .skip(evidence.unit_char_start)
                .take(evidence.quote.chars().count())
                .collect();
            if unit.sha256 != evidence.unit_sha256
                || unit.ranges != evidence.ranges
                || quote != evidence.quote
                || digest(quote.as_bytes()) != evidence.quote_sha256
            {
                return Err(error("ARCHIVE_EVIDENCE_INVALID"));
            }
        }
    }
    Ok(())
}

/// Period reports keep their archive paths only in the local frozen input.
pub struct PathsTransport<'a> {
    pub config: &'a AppConfig,
    pub paths: &'a [String],
    pub inner: &'a dyn ModelTransport,
}
impl PathsTransport<'_> {
    fn check(&self) -> Result<(), ReportError> {
        if self.paths.is_empty() {
            return Ok(());
        }
        let current = AppConfig::load(&self.config.program_dir, Some(&self.config.config_dir))?;
        if self.paths.iter().any(|path| {
            !permitted(self.config, Path::new(path)) || !permitted(&current, Path::new(path))
        }) {
            return Err(error("ARCHIVE_MODEL_PERMISSION_DENIED"));
        }
        Ok(())
    }
}
impl ModelTransport for PathsTransport<'_> {
    fn prepare(&self) -> Result<(), ReportError> {
        self.check()?;
        self.inner.prepare()
    }
    fn send(&self, request: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure> {
        self.check()
            .map_err(|_| TransportFailure::PermissionDenied)?;
        self.inner.send(request, timeout)
    }
}
