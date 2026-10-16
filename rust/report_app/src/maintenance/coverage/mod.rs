mod events;
mod recovery;
use super::files::{read_bytes, read_envelope, relative_path, write_envelope, write_immutable};
use crate::{
    config::ModelPricing,
    model::recovery::{JobBudget, RecoveryInputs},
    publish::Publisher,
    run::{self, App, FrozenInput},
    snapshots::{digest, fingerprint, reject_redirects, valid_hash, SnapshotRef},
    store::{locking::ReportLock, RunStatus, StagedReport},
    types::{
        self, Period, ReportMode, ReportRequest, ReportSource as InputSource, RunContext, RunPhase,
    },
    ReportError,
};
use ai_daily_scanner_contract::AdapterPaths;
use ai_daily_scanner_core::{ScanRequest, Scanner, ScannerConfig};
use chrono::{DateTime, NaiveDate, Utc};
pub use recovery::{amend, recover};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::{self, File, FileTimes},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePin {
    pub relative_path: String,
    pub size_bytes: u64,
    pub prior_content_sha256: Option<String>,
    pub content_sha256: Option<String>,
    pub mtime_ns: Option<u64>,
    #[serde(default)]
    pub current_version_changed: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairDate {
    pub date: String,
    pub gap_codes: Vec<String>,
    pub old_run_id: String,
    pub old_sha256: String,
    pub old_quality: String,
    pub files: Vec<FilePin>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairPlan {
    pub version: u32,
    pub job_id: String,
    pub created_at: String,
    pub work_root: PathBuf,
    pub approved_root: PathBuf,
    pub db_path: PathBuf,
    pub reports_root: PathBuf,
    pub config_fingerprint: String,
    pub algorithm_fingerprint: String,
    pub pricing: Option<ModelPricing>,
    pub calls_limit: u64,
    pub tokens_limit: u64,
    pub cost_limit_microunits: Option<u64>,
    pub worst_tokens_per_call: u64,
    pub worst_cost_per_call_microunits: Option<u64>,
    pub refreshed: bool,
    pub dates: Vec<RepairDate>,
}
pub struct PlanRequest<'a> {
    pub dates: &'a [NaiveDate],
    pub gap_codes: &'a [String],
    pub scope_root: &'a Path,
    pub calls_limit: u64,
    pub tokens_limit: u64,
    pub cost_limit_microunits: Option<u64>,
    pub refresh: bool,
}
pub(super) fn error(code: &'static str) -> ReportError {
    ReportError::new(code, "覆盖修复材料、配置或运行状态不符合要求")
}
pub fn algorithm_fingerprint() -> Result<String, ReportError> {
    fingerprint(
        &json!({"app":env!("AI_DAILY_REPORT_BUILD"),"scanner":ai_daily_scanner_core::ENGINE_BUILD_IDENTITY}),
    )
}
fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/")
        .eq_ignore_ascii_case(
            &b.to_string_lossy()
                .trim_start_matches(r"\\?\")
                .replace('\\', "/"),
        )
}
fn pin(root: &Path, relative: &str, prior: Option<String>) -> Result<FilePin, ReportError> {
    let path = relative_path(root, relative)?;
    let meta = path.metadata()?;
    let bytes = read_bytes(&path, 512 * 1024 * 1024)?;
    if meta.len() != bytes.len() as u64 || meta.modified()? != path.metadata()?.modified()? {
        return Err(error("COVERAGE_SOURCE_CHANGED"));
    }
    let hash = digest(&bytes);
    let changed = prior.as_ref().map(|old| old != &hash);
    Ok(FilePin {
        relative_path: relative.into(),
        size_bytes: meta.len(),
        prior_content_sha256: prior,
        content_sha256: Some(hash),
        mtime_ns: Some(
            u64::try_from(
                meta.modified()?
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| error("COVERAGE_SOURCE_CHANGED"))?
                    .as_nanos(),
            )
            .map_err(|_| error("COVERAGE_SOURCE_CHANGED"))?,
        ),
        current_version_changed: changed,
    })
}
fn verify_pin(plan: &RepairPlan, expected: &FilePin) -> Result<(), ReportError> {
    let actual = pin(
        &plan.work_root,
        &expected.relative_path,
        expected.prior_content_sha256.clone(),
    )?;
    if serde_json::to_value(&actual)? != serde_json::to_value(expected)? {
        return Err(error("COVERAGE_SOURCE_CHANGED"));
    }
    Ok(())
}
fn current(app: &App, item: &RepairDate) -> Result<crate::store::RunRecord, ReportError> {
    let old = app
        .store
        .current(&item.date)?
        .ok_or_else(|| error("COVERAGE_CURRENT_MISSING"))?;
    if old.run_id != item.old_run_id || old.expected_sha256.as_deref() != Some(&item.old_sha256) {
        return Err(error("COVERAGE_CURRENT_CHANGED"));
    }
    let path =
        Publisher::new(&app.config.paths.reports_dir, &app.store)?.output_path(&item.date)?;
    if digest(&read_bytes(&path, 8 * 1024 * 1024)?) != item.old_sha256 {
        return Err(error("COVERAGE_CURRENT_CHANGED"));
    }
    Ok(old)
}
pub fn make_plan(
    app: &App,
    request: PlanRequest<'_>,
    now: DateTime<Utc>,
) -> Result<PathBuf, ReportError> {
    if request.dates.is_empty()
        || request.dates.len() > 31
        || request.dates.iter().collect::<BTreeSet<_>>().len() != request.dates.len()
        || request.gap_codes.is_empty()
        || request.gap_codes.len() > 64
        || request.calls_limit > 10000
        || request.tokens_limit == 0
        || request.cost_limit_microunits.is_some() && app.config.recovery.model_pricing.is_none()
    {
        return Err(error("COVERAGE_PLAN_INVALID"));
    }
    reject_redirects(request.scope_root, "COVERAGE_SCOPE_REDIRECTED")?;
    let work = fs::canonicalize(&app.config.paths.work_dir)?;
    let approved = fs::canonicalize(request.scope_root)?;
    if !approved.starts_with(&work) {
        return Err(error("COVERAGE_SCOPE_OUTSIDE_WORK_ROOT"));
    }
    let _lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs_f64(app.config.recovery.lock_wait_seconds),
    )?;
    let mut dates = Vec::new();
    let mut total = 0u64;
    for date in request.dates {
        let date = date.to_string();
        let old = app
            .store
            .current(&date)?
            .ok_or_else(|| error("COVERAGE_CURRENT_MISSING"))?;
        let data: Value = serde_json::from_str(
            old.report_json
                .as_deref()
                .ok_or_else(|| error("COVERAGE_REPORT_INVALID"))?,
        )?;
        let gaps: Vec<_> = data["gaps"]
            .as_array()
            .ok_or_else(|| error("COVERAGE_GAP_MISSING"))?
            .iter()
            .filter_map(|g| g["code"].as_str())
            .filter(|c| request.gap_codes.iter().any(|expected| expected == c))
            .map(str::to_owned)
            .collect();
        if gaps.is_empty() {
            return Err(error("COVERAGE_GAP_MISSING"));
        }
        let mut files = Vec::new();
        for source in data["sources"]
            .as_array()
            .ok_or_else(|| error("COVERAGE_REPORT_INVALID"))?
        {
            let Some(relative) = source["relative_path"].as_str() else {
                continue;
            };
            let path = relative_path(&work, relative)?;
            if !path.starts_with(&approved) {
                return Err(error("COVERAGE_SCOPE_INCOMPLETE"));
            }
            let prior = source["content_sha256"].as_str().map(str::to_owned);
            let file = if request.refresh {
                pin(&work, relative, prior)?
            } else {
                FilePin {
                    relative_path: relative.into(),
                    size_bytes: source["size_bytes"]
                        .as_u64()
                        .ok_or_else(|| error("COVERAGE_REPORT_INVALID"))?,
                    prior_content_sha256: prior,
                    content_sha256: None,
                    mtime_ns: None,
                    current_version_changed: Some(false),
                }
            };
            total = total.saturating_add(file.size_bytes);
            files.push(file);
        }
        if files.is_empty() || files.len() > 4096 || total > 512 * 1024 * 1024 {
            return Err(error("COVERAGE_INPUT_LIMIT"));
        }
        let item = RepairDate {
            date,
            gap_codes: gaps,
            old_run_id: old.run_id,
            old_sha256: old
                .expected_sha256
                .ok_or_else(|| error("COVERAGE_REPORT_INVALID"))?,
            old_quality: old.quality.unwrap_or_else(|| "partial".into()),
            files,
        };
        current(app, &item)?;
        dates.push(item);
    }
    dates.sort_by(|a, b| a.date.cmp(&b.date));
    let output = app
        .config
        .llm
        .max_tokens
        .max(app.config.llm.facts_output_tokens());
    let worst = app.config.llm.max_request_utf8_bytes as u64 + 1024 + output;
    let plan = RepairPlan {
        version: 3,
        job_id: format!("coverage_{}", types::new_run_id()),
        created_at: now.to_rfc3339(),
        work_root: work,
        approved_root: approved,
        db_path: app.store.path.clone(),
        reports_root: app.config.paths.reports_dir.clone(),
        config_fingerprint: app.fingerprint(false)?,
        algorithm_fingerprint: algorithm_fingerprint()?,
        pricing: app.config.recovery.model_pricing.clone(),
        calls_limit: request.calls_limit,
        tokens_limit: request.tokens_limit,
        cost_limit_microunits: request.cost_limit_microunits,
        worst_tokens_per_call: worst,
        worst_cost_per_call_microunits: app
            .config
            .recovery
            .model_pricing
            .as_ref()
            .map(|p| crate::model::budget::cost(worst - output, output, p)),
        refreshed: request.refresh,
        dates,
    };
    let dir = app
        .config
        .paths
        .data_dir
        .join("coverage_repair")
        .join(&plan.job_id);
    let path = dir.join("plan.json");
    let hash = write_envelope(&path, "plan", &serde_json::to_value(&plan)?)?;
    let mut events = events::Events::open(&dir, &plan.job_id, &hash)?;
    events.append(
        "planned",
        None,
        json!({}),
        json!({"refreshed":request.refresh}),
    )?;
    if request.refresh {
        for item in &plan.dates {
            let input = prepare_input(app, &plan, item, &dir, None, now)?;
            let path = dir.join("preflight").join(format!("{}.json", item.date));
            write_envelope(&path, "input", &serde_json::to_value(&input)?)?;
        }
    }
    Ok(path)
}
pub(super) fn load(app: &App, path: &Path) -> Result<(RepairPlan, String, PathBuf), ReportError> {
    let (value, hash) = read_envelope(path, "plan")?;
    let plan: RepairPlan = serde_json::from_value(value)?;
    let dir = app
        .config
        .paths
        .data_dir
        .join("coverage_repair")
        .join(&plan.job_id);
    if !matches!(plan.version, 1 | 3)
        || plan.job_id.len() != 41
        || !plan.job_id.starts_with("coverage_")
        || !plan.job_id[9..]
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        || !same_path(
            &fs::canonicalize(path)?,
            &fs::canonicalize(dir.join("plan.json"))?,
        )
        || !same_path(&plan.work_root, &app.config.paths.work_dir)
        || !same_path(&plan.db_path, &app.store.path)
        || !same_path(&plan.reports_root, &app.config.paths.reports_dir)
        || serde_json::to_value(&plan.pricing)?
            != serde_json::to_value(&app.config.recovery.model_pricing)?
        || plan.dates.is_empty()
        || plan.dates.len() > 31
        || plan.calls_limit > 10000
        || plan.tokens_limit == 0
        || !valid_hash(&plan.config_fingerprint)
        || !valid_hash(&plan.algorithm_fingerprint)
    {
        return Err(error("COVERAGE_PLAN_CHANGED"));
    }
    let mut dates = BTreeSet::new();
    let mut total = 0u64;
    for item in &plan.dates {
        types::parse_date(&item.date)?;
        if !dates.insert(&item.date)
            || item.files.is_empty()
            || item.files.len() > 4096
            || !valid_hash(&item.old_sha256)
        {
            return Err(error("COVERAGE_PLAN_INVALID"));
        }
        let mut paths = BTreeSet::new();
        for pin in &item.files {
            if !paths.insert(&pin.relative_path)
                || !relative_path(&plan.work_root, &pin.relative_path)?
                    .starts_with(&plan.approved_root)
                || pin.content_sha256.as_ref().is_some_and(|h| !valid_hash(h))
            {
                return Err(error("COVERAGE_PLAN_INVALID"));
            }
            total = total.saturating_add(pin.size_bytes);
        }
    }
    if total > 512 * 1024 * 1024 {
        return Err(error("COVERAGE_INPUT_LIMIT"));
    }
    Ok((plan, hash, dir))
}
fn request(app: &App, date: NaiveDate, now: DateTime<Utc>) -> Result<ReportRequest, ReportError> {
    Ok(ReportRequest {
        context: RunContext::new(types::new_run_id(), now, date, app.fingerprint(false)?)?,
        period: Period {
            mode: ReportMode::Daily,
            source: InputSource::Scan,
            start: date,
            end: date,
            label: date.to_string(),
        },
        input: String::new(),
        save: false,
        dry_run: false,
        scheduled: false,
        regeneration_reason: None,
    })
}
pub(super) fn budget(app: &App, plan: &RepairPlan) -> Result<JobBudget, ReportError> {
    let revisions = app.store.job_budget_revisions(&plan.job_id)?;
    Ok(if let Some((head, sha)) = revisions.last() {
        JobBudget {
            job_id: plan.job_id.clone(),
            calls_limit: head.calls_limit,
            tokens_limit: head.tokens_limit,
            cost_limit_microunits: head.cost_limit_microunits,
            revision_sha256: Some(sha.clone()),
        }
    } else {
        JobBudget {
            job_id: plan.job_id.clone(),
            calls_limit: plan.calls_limit,
            tokens_limit: plan.tokens_limit,
            cost_limit_microunits: plan.cost_limit_microunits,
            revision_sha256: None,
        }
    })
}
fn prepare_input(
    app: &App,
    plan: &RepairPlan,
    item: &RepairDate,
    dir: &Path,
    recovery: Option<RecoveryInputs>,
    now: DateTime<Utc>,
) -> Result<FrozenInput, ReportError> {
    if !plan.refreshed {
        return Err(error("COVERAGE_PREFLIGHT_REQUIRED"));
    }
    let work = dir.join("sources").join(&item.date);
    fs::create_dir_all(&work)?;
    let mut earliest = u64::MAX;
    let mut latest = 0u64;
    for pin in &item.files {
        verify_pin(plan, pin)?;
        let source = relative_path(&plan.work_root, &pin.relative_path)?;
        let target = relative_path(&work, &pin.relative_path)?;
        write_immutable(&target, &read_bytes(&source, pin.size_bytes)?)?;
        let stamp = pin
            .mtime_ns
            .ok_or_else(|| error("COVERAGE_PREFLIGHT_REQUIRED"))?;
        File::options()
            .write(true)
            .open(target)?
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_nanos(stamp)))?;
        earliest = earliest.min(stamp);
        latest = latest.max(stamp);
    }
    let index_dir = dir.join("indexes").join(&item.date);
    fs::create_dir_all(&index_dir)?;
    let scanner = Scanner::open(ScannerConfig {
        work_dir: work.to_string_lossy().into_owned(),
        scan_db_path: index_dir
            .join("scan_index_rust_v3.sqlite3")
            .to_string_lossy()
            .into_owned(),
        scanner_settings: app.config.scanner.clone(),
        adapters: AdapterPaths {
            office_worker_path: app.config.office_worker_path.to_string_lossy().into_owned(),
            pdf_worker_path: app.config.pdf_worker_path.to_string_lossy().into_owned(),
        },
    })
    .map_err(|cause| {
        ReportError::new(
            "SCANNER_UNAVAILABLE",
            format!("初始化覆盖扫描失败：{cause}"),
        )
    })?;
    let start = DateTime::<Utc>::from_timestamp_nanos(earliest as i64);
    let end = DateTime::<Utc>::from_timestamp_nanos(latest as i64) + chrono::Duration::seconds(1);
    let mut scan = scanner
        .build_context(&ScanRequest {
            report_mode: ai_daily_scanner_contract::ReportMode::Daily,
            start_date: start
                .with_timezone(&types::shanghai())
                .date_naive()
                .to_string(),
            end_date: end
                .with_timezone(&types::shanghai())
                .date_naive()
                .to_string(),
            window_start_utc: Some(start.to_rfc3339()),
            window_end_utc: Some(end.to_rfc3339()),
            compression_profile: None,
        })
        .map_err(|cause| ReportError::new("SCANNER_UNAVAILABLE", format!("覆盖扫描失败：{cause}")))?
        .value;
    if scan
        .sources
        .iter()
        .map(|s| s.relative_path.as_str())
        .collect::<BTreeSet<_>>()
        != item
            .files
            .iter()
            .map(|p| p.relative_path.as_str())
            .collect()
    {
        return Err(error("COVERAGE_FROZEN_SCOPE_INCOMPLETE"));
    }
    for source in &mut scan.sources {
        let pin = item
            .files
            .iter()
            .find(|p| p.relative_path == source.relative_path)
            .unwrap();
        if source.content_sha256.is_some() && source.content_sha256 != pin.content_sha256 {
            return Err(error("COVERAGE_SOURCE_CHANGED"));
        }
        source.coverage_limits["history"] = json!({"current_version_changed":pin.current_version_changed,"prior_content_sha256":pin.prior_content_sha256,"excerpt_priority":pin.current_version_changed==Some(false)});
    }
    let req = request(app, types::parse_date(&item.date)?, now)?;
    let mut input = run::prepare_daily_input(app, &req, &scan)?;
    if input.quality == "unavailable" {
        return Err(error("COVERAGE_INPUT_UNAVAILABLE"));
    }
    input.recovery_inputs = Some(recovery.unwrap_or(RecoveryInputs {
        job_budget: Some(budget(app, plan)?),
        ..Default::default()
    }));
    input.input_fingerprint =
        fingerprint(&json!({"input":input.input_fingerprint,"job":input.recovery_inputs}))?;
    Ok(input)
}

pub fn generate(app: &App, path: &Path, now: DateTime<Utc>) -> Result<PathBuf, ReportError> {
    let (plan, hash, dir) = load(app, path)?;
    let _lock = ReportLock::acquire(
        &dir.join("job"),
        &plan.job_id,
        Duration::from_secs_f64(app.config.recovery.lock_wait_seconds),
    )?;
    recovery::validate_current(app, &plan, &hash, &dir)?;
    let mut events = events::Events::open(&dir, &plan.job_id, &hash)?;
    let mut candidates = Vec::new();
    for item in &plan.dates {
        let old = current(app, item)?;
        let run_id = if let Some(id) = events.state["dates"][&item.date]["run_id"].as_str() {
            id.to_owned()
        } else {
            let input = prepare_input(app, &plan, item, &dir, None, now)?;
            let id = input.context.run_id.clone();
            let snapshot = dir.join("prepared").join(format!("{id}.json"));
            write_envelope(&snapshot, "input", &serde_json::to_value(&input)?)?;
            events.append(
                "prepared",
                Some(&item.date),
                json!({"run_id":id,"status":"prepared","input_path":snapshot}),
                json!({}),
            )?;
            id
        };
        let prepared_path = events.state["dates"][&item.date]["input_path"]
            .as_str()
            .map(PathBuf::from);
        if let Some(path) = prepared_path {
            let (value, _) = read_envelope(&path, "input")?;
            let input: FrozenInput = serde_json::from_value(value)?;
            if input.context.run_id != run_id {
                return Err(error("COVERAGE_PREPARED_CHANGED"));
            }
            run::prepare_frozen(
                app,
                &input,
                false,
                &format!("coverage_candidate:{}", plan.job_id),
            )?;
        }
        let outcome = match run::resume_frozen(app, &run_id, now) {
            Ok(outcome) => outcome,
            Err(error) => {
                events.append(
                    "failed",
                    Some(&item.date),
                    json!({"run_id":run_id,"status":"failed","error_code":error.code}),
                    json!({}),
                )?;
                return Err(error);
            }
        };
        let run = app.store.get(&run_id)?;
        let report: Value = serde_json::from_str(
            run.report_json
                .as_deref()
                .ok_or_else(|| error("COVERAGE_CANDIDATE_INVALID"))?,
        )?;
        let old_report: Value = serde_json::from_str(old.report_json.as_deref().unwrap())?;
        let before = old.markdown.as_deref().unwrap_or("");
        let diff = format!(
            "--- {} (原报告)\n+++ {} (候选报告)\n{}{}",
            item.date,
            item.date,
            before
                .lines()
                .map(|line| format!("-{line}\n"))
                .collect::<String>(),
            outcome
                .markdown
                .lines()
                .map(|line| format!("+{line}\n"))
                .collect::<String>()
        );
        let folder = dir.join("candidates");
        let candidate_path = folder.join(format!("{}.json", item.date));
        let candidate = json!({"version":3,"job_id":plan.job_id,"plan_sha256":hash,"date":item.date,"run_id":run_id,"generation_id":run.generation_id,"input_fingerprint":run.input_fingerprint,"snapshot_sha256":run.snapshot_sha256,"old_run_id":item.old_run_id,"old_sha256":item.old_sha256,"report":report,"markdown_sha256":digest(outcome.markdown.as_bytes()),"old_metrics":metrics(&old_report),"candidate_metrics":metrics(&report),"model_calls":app.store.calls(&run_id)?.len(),"diff_sha256":digest(diff.as_bytes())});
        let candidate_hash = write_envelope(&candidate_path, "candidate", &candidate)?;
        write_immutable(
            &folder.join(format!("{}.md", item.date)),
            outcome.markdown.as_bytes(),
        )?;
        write_immutable(&folder.join(format!("{}.diff", item.date)), diff.as_bytes())?;
        let snapshot = app
            .snapshots
            .put(&run_id, "coverage-candidate", &candidate)?;
        candidates.push(json!({"date":item.date,"run_id":run_id,"sha256":candidate_hash,"snapshot_path":snapshot.path,"snapshot_sha256":snapshot.sha256}));
        events.append(
            "candidate_saved",
            Some(&item.date),
            json!({"status":"candidate","run_id":run_id,"candidate_sha256":candidate_hash}),
            json!({}),
        )?;
    }
    let path = dir.join("candidates.json");
    write_envelope(
        &path,
        "manifest",
        &json!({"version":3,"job_id":plan.job_id,"plan_sha256":hash,"candidates":candidates}),
    )?;
    Ok(path)
}
fn metrics(report: &Value) -> Value {
    let sources = report["sources"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    json!({"quality":report["quality"],"sources":sources.len(),"excerpt_chars":sources.iter().filter_map(|s|s["excerpt_chars"].as_u64()).sum::<u64>(),"model_chars":sources.iter().filter_map(|s|s["model_chars"].as_u64()).sum::<u64>(),"gaps":report["gaps"].as_array().map_or(0,Vec::len)})
}
pub fn publish(app: &App, path: &Path, now: DateTime<Utc>) -> Result<Value, ReportError> {
    let (manifest, _) = read_envelope(path, "manifest")?;
    let dir = path
        .parent()
        .ok_or_else(|| error("COVERAGE_MANIFEST_INVALID"))?;
    let (plan, hash, expected_dir) = load(app, &dir.join("plan.json"))?;
    if manifest["plan_sha256"] != hash
        || manifest["job_id"] != plan.job_id
        || !matches!(manifest["version"].as_u64(), Some(1 | 3))
        || !same_path(dir, &expected_dir)
    {
        return Err(error("COVERAGE_MANIFEST_INVALID"));
    }
    let _job_lock = ReportLock::acquire(&dir.join("job"), &plan.job_id, Duration::from_secs(5))?;
    let _lock = ReportLock::acquire(
        &app.store.path,
        &app.config.report.scope_id,
        Duration::from_secs_f64(app.config.recovery.lock_wait_seconds),
    )?;
    let mut events = events::Events::open(dir, &plan.job_id, &hash)?;
    let entries = manifest["candidates"]
        .as_array()
        .filter(|entries| entries.len() == plan.dates.len())
        .ok_or_else(|| error("COVERAGE_MANIFEST_INVALID"))?;
    let mut seen = BTreeSet::new();
    let mut published = Vec::new();
    for entry in entries {
        let date = entry["date"]
            .as_str()
            .ok_or_else(|| error("COVERAGE_MANIFEST_INVALID"))?;
        if !seen.insert(date) {
            return Err(error("COVERAGE_MANIFEST_INVALID"));
        }
        let item = plan
            .dates
            .iter()
            .find(|d| d.date == date)
            .ok_or_else(|| error("COVERAGE_MANIFEST_INVALID"))?;
        let (candidate, sha) = read_envelope(
            &dir.join("candidates").join(format!("{date}.json")),
            "candidate",
        )?;
        let snapshot = SnapshotRef {
            path: entry["snapshot_path"]
                .as_str()
                .ok_or_else(|| error("COVERAGE_CANDIDATE_INVALID"))?
                .into(),
            sha256: entry["snapshot_sha256"]
                .as_str()
                .ok_or_else(|| error("COVERAGE_CANDIDATE_INVALID"))?
                .into(),
        };
        let preview = app.store.get(
            entry["run_id"]
                .as_str()
                .ok_or_else(|| error("COVERAGE_CANDIDATE_INVALID"))?,
        )?;
        let markdown = read_bytes(
            &dir.join("candidates").join(format!("{date}.md")),
            8 * 1024 * 1024,
        )?;
        if candidate != app.snapshots.read(&snapshot)?
            || !matches!(candidate["version"].as_u64(), Some(1 | 3))
            || entry["sha256"] != sha
            || candidate["plan_sha256"] != hash
            || candidate["job_id"] != plan.job_id
            || candidate["run_id"] != preview.run_id
            || candidate["generation_id"] != preview.generation_id
            || candidate["input_fingerprint"] != json!(preview.input_fingerprint)
            || candidate["snapshot_sha256"] != json!(preview.snapshot_sha256)
            || candidate["date"] != date
            || candidate["old_run_id"] != item.old_run_id
            || candidate["old_sha256"] != item.old_sha256
            || preview.run_kind != "preview"
            || !matches!(preview.status, RunStatus::Succeeded)
            || candidate["report"]
                != serde_json::from_str::<Value>(
                    preview
                        .report_json
                        .as_deref()
                        .ok_or_else(|| error("COVERAGE_CANDIDATE_INVALID"))?,
                )?
            || preview
                .markdown
                .as_ref()
                .is_none_or(|text| text.as_bytes() != markdown)
            || candidate["markdown_sha256"] != digest(&markdown)
            || candidate["diff_sha256"]
                != digest(&read_bytes(
                    &dir.join("candidates").join(format!("{date}.diff")),
                    16 * 1024 * 1024,
                )?)
        {
            return Err(error("COVERAGE_CANDIDATE_CHANGED"));
        }
        let publish_id = if let Some(id) = events.state["dates"][date]["publish_run_id"].as_str() {
            id.to_owned()
        } else {
            current(app, item)?;
            let id = types::new_run_id();
            events.append(
                "publish_prepared",
                Some(date),
                json!({"publish_run_id":id}),
                json!({"candidate_sha256":sha}),
            )?;
            id
        };
        let exists: i64 = app.store.connection()?.query_row(
            "SELECT COUNT(*) FROM report_runs WHERE run_id=?",
            [&publish_id],
            |r| r.get(0),
        )?;
        if exists == 0 {
            let mut context: RunContext = serde_json::from_str(&preview.context_json)?;
            context.run_id = publish_id.clone();
            context.started_at = now;
            app.store.create(
                &context,
                "report",
                &publish_id,
                Some(&format!("coverage_publish:{}", preview.run_id)),
                now,
            )?;
        }
        if app.store.get(&publish_id)?.phase == RunPhase::Created {
            app.store.prepare(
                &publish_id,
                &snapshot,
                &sha,
                candidate["report"]["sources"]
                    .as_array()
                    .ok_or_else(|| error("COVERAGE_CANDIDATE_INVALID"))?,
                now,
            )?;
        }
        let publication = app.store.get(&publish_id)?;
        if !matches!(
            publication.phase,
            RunPhase::DbCommitted | RunPhase::Published
        ) {
            current(app, item)?;
            let output =
                Publisher::new(&app.config.paths.reports_dir, &app.store)?.output_path(date)?;
            app.store.stage_report(
                &publish_id,
                &StagedReport {
                    report: &candidate["report"],
                    markdown: preview.markdown.as_deref().unwrap(),
                    output_path: &output.to_string_lossy(),
                    publication_fingerprint: &sha,
                },
                now,
            )?;
        }
        let output =
            Publisher::new(&app.config.paths.reports_dir, &app.store)?.publish(&publish_id, now)?;
        events.append(
            "published",
            Some(date),
            json!({"status":"published","publish_run_id":publish_id}),
            json!({}),
        )?;
        published.push(json!({"date":date,"run_id":publish_id,"path":output}));
    }
    Ok(json!({"job_id":plan.job_id,"published":published,"model_calls":0}))
}
