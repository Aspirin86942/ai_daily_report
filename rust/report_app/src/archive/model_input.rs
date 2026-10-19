//! Only observed, immutable changes enter a report. Publication owns coverage.
mod group;

use super::{
    diff, error, identity, parse,
    store::{atomic_write, ArchiveStore},
};
use crate::{
    config::AppConfig,
    model::{
        contracts::SelectedFact,
        plan::PlannedStep,
        transport::{HttpResponse, ModelTransport, TransportFailure},
    },
    reports::data::{CoverageGap, ReportSource},
    run::FrozenInput,
    snapshots::digest,
    store::RunRecord,
    types::ReportRequest,
    ReportError,
};
use ai_daily_worker_contract::{
    archive::{ArchiveArtifact, ArchiveUnit},
    SourceRange,
};
use chrono::DateTime;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::Duration,
};

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

pub struct Prepared {
    pub sources: Vec<ReportSource>,
    pub texts: BTreeMap<String, String>,
    pub gaps: Vec<CoverageGap>,
    pub changes: Vec<String>,
    pub excluded_changes: Vec<String>,
    pub continued: u64,
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

pub fn prepare(config: &AppConfig, request: &ReportRequest) -> Result<Prepared, ReportError> {
    let store = ArchiveStore::open(config, false)?;
    let baseline: String = store
        .conn
        .query_row("SELECT value FROM metadata WHERE key='baseline'", [], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or_else(|| error("ARCHIVE_BASELINE_REQUIRED"))?;
    let baseline =
        DateTime::parse_from_rfc3339(&baseline).map_err(|_| error("ARCHIVE_TIME_INVALID"))?;
    let profile = parse::profile(config)?;
    let mut statement=store.conn.prepare("SELECT c.id,c.version_id,c.before_version,c.extraction_id,c.before_extraction,c.observed_at,c.manifest_json FROM changes c JOIN extractions e ON e.id=c.extraction_id WHERE e.profile=? AND c.rowid=(SELECT max(newer.rowid) FROM changes newer WHERE newer.version_id=c.version_id AND newer.extraction_id=c.extraction_id) AND c.observed_at<? AND c.ready_at<=? AND NOT EXISTS(SELECT 1 FROM report_changes rc JOIN report_evidence r ON r.run_id=rc.run_id JOIN changes published_change ON published_change.id=rc.change_id WHERE published_change.version_id=c.version_id AND r.status='published') ORDER BY c.observed_at,c.rowid")?;
    let rows = statement
        .query_map(
            params![
                profile,
                request.context.window_end_utc.to_rfc3339(),
                request.context.started_at.to_rfc3339()
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Prepared {
        sources: vec![],
        texts: BTreeMap::new(),
        gaps: vec![],
        changes: vec![],
        excluded_changes: vec![],
        continued: 0,
    };
    let mut total = 0usize;
    for (change, version, before_version, extraction, before_extraction, observed, manifest) in rows
    {
        let current = store.version(&version)?;
        if !permitted(config, Path::new(&current.path)) {
            out.gaps.push(CoverageGap::new(
                "ARCHIVE_LOCAL_ONLY",
                "discovery",
                None,
                "部分变化仅允许本地归档，未发送模型。",
            ));
            continue;
        }
        let changes: diff::Changes = serde_json::from_str(&manifest)?;
        out.changes.push(change.clone());
        let name = Path::new(&current.path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        if config
            .archive
            .model_ignored_prefixes
            .iter()
            .any(|prefix| name.starts_with(&prefix.to_lowercase()))
        {
            out.excluded_changes.push(change);
            continue;
        }
        let artifact: ArchiveArtifact = serde_json::from_value(store.read_manifest(&extraction)?)?;
        let prior_artifact: Option<ArchiveArtifact> = before_extraction
            .as_deref()
            .map(|id| {
                store
                    .read_manifest(id)
                    .and_then(|v| Ok(serde_json::from_value(v)?))
            })
            .transpose()?;
        let comparison_incomplete = changes.granularity == "before_unavailable"
            || artifact.truncated
            || prior_artifact
                .as_ref()
                .is_some_and(|a| a.truncated || a.usable_chars == 0)
            || (before_version.is_some() && artifact.usable_chars == 0);
        if changes.kind == "continued"
            || changes
                .hunks
                .iter()
                .all(|h| h.before.start == h.before.end && h.after.start == h.after.end)
        {
            if comparison_incomplete
                || !artifact.gaps.is_empty()
                || prior_artifact.as_ref().is_some_and(|a| !a.gaps.is_empty())
            {
                out.gaps.push(CoverageGap::new("ARCHIVE_COMPARISON_INCOMPLETE", "parse", None, "已采集的原件发生变化，正文比较无法覆盖其中的图片、缺失正文或超限内容；请核对原件。"));
            } else if changes.kind == "continued" {
                out.continued += 1;
            }
            continue;
        }
        let prior = before_version
            .as_deref()
            .map(|id| store.version(id))
            .transpose()?;
        let relative = Path::new(&current.path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let day = DateTime::parse_from_rfc3339(&observed)
            .map_err(|_| error("ARCHIVE_TIME_INVALID"))?
            .with_timezone(&crate::types::shanghai())
            .date_naive();
        for (hunk_index, hunk) in changes.hunks.iter().enumerate() {
            let count =
                (hunk.before.end - hunk.before.start).max(hunk.after.end - hunk.after.start);
            for offset in 0..count {
                let id = format!(
                    "src_{}",
                    identity(&[
                        "archive-source",
                        &change,
                        &hunk_index.to_string(),
                        &offset.to_string()
                    ])
                );
                let kind = if hunk.before.start == hunk.before.end {
                    "added"
                } else if hunk.after.start == hunk.after.end {
                    "removed"
                } else {
                    changes.kind.as_str()
                };
                let mut text = format!(
                    "观察日期：{day}；变化类型：{}；比较粒度：{}。观察到新版不代表制度已生效。\n",
                    kind, changes.granularity
                );
                let mut evidence = Vec::new();
                let sides = [
                    (
                        "before",
                        before_extraction.as_deref(),
                        prior.as_ref(),
                        hunk.before.start + offset,
                        hunk.before.end,
                    ),
                    (
                        "after",
                        Some(extraction.as_str()),
                        Some(&current),
                        hunk.after.start + offset,
                        hunk.after.end,
                    ),
                ];
                for (side, extract, version, unit_no, end) in sides {
                    let (Some(extract), Some(version)) = (extract, version) else {
                        continue;
                    };
                    if unit_no >= end {
                        continue;
                    }
                    let dir = store.extraction_dir(extract)?;
                    let unit = diff::unit(&dir, unit_no)?;
                    let quote = diff::text(&dir, &unit)?;
                    let manifest = if side == "before" {
                        prior_artifact.as_ref()
                    } else {
                        Some(&artifact)
                    };
                    let label = unit
                        .ranges
                        .first()
                        .and_then(|r| r.label.as_deref())
                        .unwrap_or("table");
                    if matches!(
                        unit.kind,
                        ai_daily_worker_contract::ContentUnitKind::TableRow
                            | ai_daily_worker_contract::ContentUnitKind::Total
                    ) {
                        if let Some(header_no) = manifest
                            .and_then(|m| m.coverage["table_headers"][label].as_u64())
                            .filter(|n| *n != unit_no)
                        {
                            let header = diff::unit(&dir, header_no)?;
                            text.push_str("对应表头（上下文）：\n");
                            text.push_str(&diff::text(&dir, &header)?);
                            text.push('\n');
                        }
                    }
                    text.push_str(if side == "before" {
                        "修改前原文：\n"
                    } else {
                        "本次原文：\n"
                    });
                    let start = text.chars().count();
                    text.push_str(&quote);
                    let end = text.chars().count();
                    text.push('\n');
                    evidence.push(EvidenceRef {
                        source_id: id.clone(),
                        version_id: version.id.clone(),
                        extraction_id: extract.into(),
                        unit_id: unit.unit_id,
                        side: side.into(),
                        raw_sha256: version.sha256.clone(),
                        unit_sha256: unit.sha256,
                        quote_sha256: digest(quote.as_bytes()),
                        quote,
                        unit_char_start: 0,
                        input_start: start,
                        input_end: end,
                        ranges: unit.ranges,
                    });
                }
                total = total.saturating_add(text.len());
                if total > 64 * 1024 * 1024 {
                    return Err(error("ARCHIVE_REPORT_INPUT_LIMIT"));
                }
                let chars = text.chars().count();
                let matter = identity(&[
                    "matter",
                    &current.document_id,
                    &change,
                    &hunk_index.to_string(),
                    &offset.to_string(),
                ]);
                let archive = json!({"change_id":change,"document_id":current.document_id,"version_id":version,"extraction_id":extraction,"path":current.path,"observed_date":day,"kind":kind,"comparison_incomplete":comparison_incomplete,"matter_id":matter,"evidence":evidence,"hunk_index":hunk_index,"unit_offset":offset});
                let source = ReportSource {
                    source_id: id.clone(),
                    source_kind: "file".into(),
                    relative_path: Some(relative.clone()),
                    extension: Some(current.extension.clone()),
                    size_bytes: current.size_bytes,
                    modified_at_utc: Some(current.modified_at.clone()),
                    observed_at_utc: observed.clone(),
                    content_sha256: Some(current.sha256.clone()),
                    excerpt_sha256: digest(text.as_bytes()),
                    excerpt_chars: chars,
                    model_chars: 0,
                    parser_backend: Some(artifact.parser_backend.clone()),
                    worker_lane: Some(artifact.worker_lane.clone()),
                    parse_status: "success".into(),
                    parse_truncated: artifact.truncated,
                    excerpt_truncated: false,
                    model_truncated: false,
                    coverage_limits: json!({"archive":archive,"evidence_audit":{"version":1,"primary_status":"usable_excerpt","secondary_tags":[],"content_units":[{"text_start":0,"text_end":chars,"kind":"paragraph","ranges":[]} ]}}),
                    reason: None,
                    snapshot_ref: None,
                };
                source.validate()?;
                if day < request.context.target_date {
                    out.gaps.push(CoverageGap::new(
                        "ARCHIVE_LATE_CHANGE",
                        "schedule",
                        Some(&id),
                        &format!("补充此前未发布变化，首次观察日期：{day}。"),
                    ));
                }
                if comparison_incomplete {
                    out.gaps.push(CoverageGap::new(
                        "ARCHIVE_COMPARISON_INCOMPLETE",
                        "parse",
                        Some(&id),
                        "相邻版本正文未完整取得，变化结论需核对原件。",
                    ));
                }
                if artifact.truncated || !artifact.gaps.is_empty() {
                    out.gaps.push(CoverageGap::new(
                        "ARCHIVE_PARSE_GAP",
                        "parse",
                        Some(&id),
                        &format!("归档解析覆盖缺口：{}", artifact.gaps.join("；")),
                    ));
                }
                out.texts.insert(id, text);
                out.sources.push(source);
            }
        }
    }
    if baseline > request.context.window_start_utc {
        out.gaps.push(CoverageGap::new(
            "ARCHIVE_BASELINE_WINDOW",
            "discovery",
            None,
            &format!(
                "归档采集始于 {}（Asia/Shanghai），此前的变化无法还原；首次基线中的已有内容不计为新增成果。",
                baseline.with_timezone(&crate::types::shanghai()).format("%Y-%m-%d %H:%M:%S")
            ),
        ));
    }
    if let Some(gap) = collection_coverage(&store, request, baseline.with_timezone(&chrono::Utc))? {
        out.gaps.push(gap);
    }
    let pending:i64=store.conn.query_row("SELECT count(*) FROM versions v WHERE v.baseline=0 AND v.pruned_at IS NULL AND v.observed_at<? AND NOT EXISTS(SELECT 1 FROM version_extractions ve JOIN extractions e ON e.id=ve.extraction_id WHERE ve.version_id=v.id AND e.profile=? AND e.status IN('complete','partial'))",params![request.context.window_end_utc.to_rfc3339(),profile],|r|r.get(0))?;
    if pending > 0 {
        out.gaps.push(CoverageGap::new(
            "ARCHIVE_PARSE_PENDING",
            "parse",
            None,
            &format!("{pending} 个版本尚未完成解析，完成后进入下一份日报。"),
        ));
    }
    let unresolved: i64 = store.conn.query_row(
        "SELECT count(*) FROM gaps g JOIN collections c ON c.id=g.collection_id WHERE g.resolved_at IS NULL OR (c.started_at>=?1 AND c.started_at<?2) OR (g.code='COLLECTION_INTERVAL_GAP' AND json_extract(g.detail,'$.from')<?2 AND json_extract(g.detail,'$.to')>?1)",
        params![request.context.window_start_utc.to_rfc3339(),request.context.window_end_utc.to_rfc3339()],
        |r| r.get(0),
    )?;
    if unresolved > 0 {
        out.gaps.push(CoverageGap::new(
            "ARCHIVE_CAPTURE_GAP",
            "discovery",
            None,
            &format!("本次范围内或尚未恢复的采集、解析缺口共 {unresolved} 条，可通过 stats 查看原因与恢复时间。"),
        ));
    }
    group::coalesce(&mut out, &config.archive)?;
    out.gaps
        .sort_by(|a, b| (&a.code, &a.source_id).cmp(&(&b.code, &b.source_id)));
    out.gaps
        .dedup_by(|a, b| a.code == b.code && a.source_id == b.source_id);
    Ok(out)
}

fn collection_coverage(
    store: &ArchiveStore,
    request: &ReportRequest,
    baseline: DateTime<chrono::Utc>,
) -> Result<Option<CoverageGap>, ReportError> {
    let start = request.context.window_start_utc.max(baseline);
    let end = request
        .context
        .window_end_utc
        .min(request.context.started_at);
    if start >= end {
        return Ok(None);
    }
    let coverage = super::schedule::coverage(store, start, end, request.context.started_at)?;
    let completed = coverage.completed;
    let count = coverage.gaps;
    let ranges = coverage.ranges;
    if coverage.planned_idle || completed && count == 0 {
        return Ok(None);
    }
    let detail = if completed {
        format!(
            "按当时生效的采集间隔检查（允许 1 分钟调度偏差），发现 {count} 段缺少已完成采集记录的时间，以下列出前 {} 段（Asia/Shanghai）：{}。这些时间内是否有变化无法判断。",
            ranges.len(), ranges.join("；")
        )
    } else {
        "目标范围尚无可核对的已完成采集记录，无法判断是否有变化。".into()
    };
    Ok(Some(CoverageGap::new(
        "ARCHIVE_COLLECTION_GAP",
        "discovery",
        None,
        &detail,
    )))
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
