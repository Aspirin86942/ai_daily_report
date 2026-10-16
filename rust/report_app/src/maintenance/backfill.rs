use super::files::{read_bytes, read_envelope, read_json, write_envelope};
use crate::{
    cli::BackfillArgs,
    model::recovery::{FrozenCall, JobBudget},
    run::{self, App, FrozenInput},
    snapshots::{canonical_json, digest, fingerprint, reject_redirects},
    store::{locking::ReportLock, RunStatus},
    types::{
        new_run_id, parse_date, Period, ReportMode, ReportRequest, ReportSource, RunContext,
        RunPhase,
    },
    ReportError,
};
use chrono::{DateTime, NaiveDate, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
fn error(code: &'static str) -> ReportError {
    ReportError::new(code, "补跑计划、预算或已保存材料不符合要求")
}
fn dates(
    start: NaiveDate,
    end: NaiveDate,
    now: DateTime<Utc>,
) -> Result<Vec<NaiveDate>, ReportError> {
    RunContext::new(new_run_id(), now, end, String::new())?;
    let count = (end - start).num_days() + 1;
    if !(1..=3660).contains(&count) {
        return Err(error("BACKFILL_RANGE_INVALID"));
    }
    Ok((0..count)
        .map(|i| start + chrono::Duration::days(i))
        .collect())
}
fn request(app: &App, date: NaiveDate, now: DateTime<Utc>) -> Result<ReportRequest, ReportError> {
    Ok(ReportRequest {
        context: RunContext::new(new_run_id(), now, date, app.fingerprint(false)?)?,
        period: Period {
            mode: ReportMode::Daily,
            source: ReportSource::Scan,
            start: date,
            end: date,
            label: date.to_string(),
        },
        input: String::new(),
        save: true,
        dry_run: false,
        scheduled: false,
        regeneration_reason: None,
    })
}
fn existing(app: &App, date: &str) -> Result<Option<Value>, ReportError> {
    if let Some(run) = app.store.current(date)? {
        if run.phase != RunPhase::Published || !matches!(run.status, RunStatus::Succeeded) {
            return Err(error("BACKFILL_EXISTING_PUBLICATION_INCOMPLETE"));
        }
        let path = PathBuf::from(
            run.output_path
                .as_ref()
                .ok_or_else(|| error("BACKFILL_EXISTING_OUTPUT_MISSING"))?,
        );
        if !path.is_file() {
            return Err(error("BACKFILL_EXISTING_OUTPUT_MISSING"));
        }
        let hash = digest(&read_bytes(&path, 8 * 1024 * 1024)?);
        if run.expected_sha256.as_deref() != Some(hash.as_str()) {
            return Err(error("BACKFILL_EXISTING_OUTPUT_MODIFIED"));
        }
        let report: Value = serde_json::from_str(
            run.report_json
                .as_deref()
                .ok_or_else(|| error("BACKFILL_EXISTING_PUBLICATION_INCOMPLETE"))?,
        )?;
        return Ok(Some(
            json!({"date":date,"status":"complete","quality":run.quality,"run_id":run.run_id,"output_path":path,"sha256":hash,"sources":report["sources"].as_array().map_or(0,Vec::len),"action":"preserved"}),
        ));
    }
    let report: Option<String> = app
        .store
        .connection()?
        .query_row(
            "SELECT raw_json FROM daily_reports WHERE date=?",
            [date],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(report) = report {
        let path = app
            .config
            .paths
            .reports_dir
            .join(&date[..7])
            .join(format!("{date}.md"));
        if !path.is_file() {
            return Err(error("BACKFILL_EXISTING_OUTPUT_MISSING"));
        }
        let report: Value = serde_json::from_str(&report)?;
        return Ok(Some(
            json!({"date":date,"status":"complete","quality":"legacy","run_id":null,"output_path":path,"sha256":digest(&read_bytes(&path,8*1024*1024)?),"sources":report["sources"].as_array().map_or(0,Vec::len),"action":"preserved"}),
        ));
    }
    Ok(None)
}
fn dir(app: &App, start: NaiveDate, end: NaiveDate) -> PathBuf {
    app.config
        .paths
        .data_dir
        .join("backfill")
        .join(format!("{start}_{end}"))
}

/// Immutable, zero-model input preparation. Every selected date has exactly one row.
pub fn preflight(
    app: &App,
    start: NaiveDate,
    end: NaiveDate,
    now: DateTime<Utc>,
) -> Result<PathBuf, ReportError> {
    let selected = dates(start, end, now)?;
    let dir = dir(app, start, end);
    fs::create_dir_all(&dir)?;
    let _job = ReportLock::acquire(&dir.join("job"), "backfill", Duration::from_secs(5))?;
    let path = dir.join("preflight-v3.json");
    if path.exists() {
        let (value, _) = read_envelope(&path, "preflight")?;
        validate_preflight(&value, start, end, &selected)?;
        return Ok(path);
    }
    let mut rows = Vec::new();
    for date in selected.iter().copied() {
        if existing(app, &date.to_string())?.is_some() {
            rows.push(json!({"date":date.to_string(),"status":"existing"}));
            continue;
        }
        let request = request(app, date, now)?;
        let scan = app.scan(&request)?;
        let input = run::prepare_daily_input(app, &request, &scan)?;
        let input_path = dir.join("preflight-inputs").join(format!("{date}.json"));
        let sha = write_envelope(&input_path, "input", &serde_json::to_value(&input)?)?;
        rows.push(json!({"date":date.to_string(),"status":if input.plan.batches.is_empty(){"no_changes"}else{"model_required"},"input_path":input_path,"input_sha256":sha}));
    }
    write_envelope(
        &path,
        "preflight",
        &json!({"version":3,"date_start":start.to_string(),"date_end":end.to_string(),"completed_dates":selected.len(),"config_fingerprint":app.fingerprint(false)?,"dates":rows}),
    )?;
    Ok(path)
}
fn validate_preflight(
    value: &Value,
    start: NaiveDate,
    end: NaiveDate,
    selected: &[NaiveDate],
) -> Result<(), ReportError> {
    let rows = value["dates"]
        .as_array()
        .ok_or_else(|| error("BACKFILL_PREFLIGHT_INVALID"))?;
    let declared: Vec<_> = rows
        .iter()
        .map(|r| r["date"].as_str().unwrap_or(""))
        .collect();
    let expected: BTreeSet<_> = selected.iter().map(ToString::to_string).collect();
    if value["date_start"] != start.to_string()
        || value["date_end"] != end.to_string()
        || value["completed_dates"] != selected.len()
        || declared.len() != expected.len()
        || declared
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>()
            != expected
        || rows.iter().any(|r| {
            !matches!(
                r["status"].as_str(),
                Some("no_changes" | "existing" | "model_required" | "ready" | "partial" | "error")
            )
        })
    {
        return Err(error("BACKFILL_PREFLIGHT_INVALID"));
    }
    Ok(())
}
fn sync_file(path: &Path, bytes: &[u8], previous: Option<&str>) -> Result<(), ReportError> {
    reject_redirects(path, "MAINTENANCE_PATH_REDIRECTED")?;
    if path.exists() {
        let old = read_bytes(path, 8 * 1024 * 1024)?;
        if old == bytes {
            return Ok(());
        }
        if previous != Some(digest(&old).as_str()) {
            return Err(error("BACKFILL_STATE_MODIFIED"));
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| error("BACKFILL_STATE_INVALID"))?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|_| error("BACKFILL_STATE_WRITE_FAILED"))?;
    Ok(())
}
fn read_state(app: &App, job: &str, path: &Path) -> Result<Option<Value>, ReportError> {
    let text: Option<String> = app
        .store
        .connection()?
        .query_row(
            "SELECT value FROM report_metadata WHERE key=?",
            [format!("backfill_state:{job}")],
            |r| r.get(0),
        )
        .optional()?;
    let Some(text) = text else { return Ok(None) };
    let record: Value = serde_json::from_str(&text)?;
    let bytes = canonical_json(&record["state"])?;
    if digest(&bytes) != record["sha256"] {
        return Err(error("BACKFILL_STATE_INVALID"));
    }
    sync_file(path, &bytes, record["previous_sha256"].as_str())?;
    Ok(Some(record["state"].clone()))
}
fn save_state(app: &App, job: &str, path: &Path, state: &Value) -> Result<(), ReportError> {
    let prior = read_state(app, job, path)?;
    let previous = prior
        .as_ref()
        .map(canonical_json)
        .transpose()?
        .map(|b| digest(&b));
    if prior.is_none() && path.exists() {
        return Err(error("BACKFILL_STATE_MODIFIED"));
    }
    let bytes = canonical_json(state)?;
    let record = json!({"state":state,"sha256":digest(&bytes),"previous_sha256":previous});
    app.store.connection()?.execute("INSERT INTO report_metadata(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![format!("backfill_state:{job}"),record.to_string()])?;
    sync_file(path, &bytes, previous.as_deref())
}
fn input_for(
    app: &App,
    row: &Value,
    date: NaiveDate,
    now: DateTime<Utc>,
) -> Result<FrozenInput, ReportError> {
    if let Some(path) = row["input_path"].as_str() {
        let (value, sha) = read_envelope(Path::new(path), "input")?;
        if row["input_sha256"] != sha {
            return Err(error("BACKFILL_INPUT_CHANGED"));
        }
        let input: FrozenInput = serde_json::from_value(value)?;
        if input.context.target_date != date
            || input.context.config_fingerprint != app.fingerprint(false)?
        {
            return Err(error("BACKFILL_INPUT_CONFIG_CHANGED"));
        }
        return Ok(input);
    }
    let request = request(app, date, now)?;
    run::prepare_daily_input(app, &request, &app.scan(&request)?)
}

pub fn run(app: &App, args: &BackfillArgs, now: DateTime<Utc>) -> Result<Value, ReportError> {
    if args.repair_coverage || args.publish_candidates.is_some() {
        return coverage_command(app, args, now);
    }
    if !args.dates.is_empty()
        || !args.gap_codes.is_empty()
        || args.scope_root.is_some()
        || args.refresh_preflight
        || args.plan.is_some()
        || args.generate_candidates
        || args.token_budget.is_some()
        || args.cost_budget_microunits.is_some()
    {
        return Err(error("BACKFILL_OPTIONS_INVALID"));
    }
    let start = args
        .start_date
        .ok_or_else(|| error("BACKFILL_RANGE_REQUIRED"))?;
    let end = args
        .end_date
        .ok_or_else(|| error("BACKFILL_RANGE_REQUIRED"))?;
    let selected = dates(start, end, now)?;
    if args.dry_run {
        return Ok(json!({"preflight":preflight(app,start,end,now)?,"new_model_calls":0}));
    }
    let cap = args
        .call_budget
        .filter(|n| *n <= 100000)
        .ok_or_else(|| error("BACKFILL_CALL_BUDGET_REQUIRED"))?;
    let preflight_path = args
        .preflight
        .as_ref()
        .ok_or_else(|| error("BACKFILL_PREFLIGHT_REQUIRED"))?;
    let raw = read_json(preflight_path)?;
    let preflight = if raw.get("preflight").is_some() {
        read_envelope(preflight_path, "preflight")?.0
    } else {
        raw
    };
    validate_preflight(&preflight, start, end, &selected)?;
    let dir = dir(app, start, end);
    fs::create_dir_all(&dir)?;
    let _job = ReportLock::acquire(&dir.join("job"), "backfill", Duration::from_secs(5))?;
    let jobfile = dir.join("job-v3.json");
    let mut initial = None;
    let jobvalue = if jobfile.exists() {
        read_envelope(&jobfile, "job")?.0
    } else {
        let _lock = ReportLock::acquire(
            &app.store.path,
            &app.config.report.scope_id,
            Duration::from_secs(5),
        )?;
        let old = dir.join("state.json");
        let (baseline, backup_hash) = if old.exists() {
            let state = read_json(&old)?;
            if state["start_date"] != start.to_string()
                || state["end_date"] != end.to_string()
                || state["call_budget"] != cap
            {
                return Err(error("BACKFILL_BUDGET_CHANGED"));
            }
            let baseline = state["baseline_call_id"]
                .as_i64()
                .filter(|n| *n >= 0)
                .ok_or_else(|| error("BACKFILL_STATE_INVALID"))?;
            let hash = crate::store::file_sha(&dir.join("reports-before-backfill.sqlite3"))?;
            if state["backup_sha256"] != hash {
                return Err(error("BACKFILL_BACKUP_CHANGED"));
            }
            initial = Some(state);
            (baseline, hash)
        } else {
            let backup = dir.join("reports-before-backfill.sqlite3");
            // An existing unregistered backup needs review; never replace it.
            (app.store.last_call_id()?, app.store.backup(&backup)?)
        };
        let value = json!({"version":3,"job_id":new_run_id(),"start_date":start.to_string(),"end_date":end.to_string(),"baseline_call_id":baseline,"call_budget":cap,"backup_sha256":backup_hash});
        write_envelope(&jobfile, "job", &value)?;
        value
    };
    if jobvalue["start_date"] != start.to_string()
        || jobvalue["end_date"] != end.to_string()
        || jobvalue["call_budget"] != cap
    {
        return Err(error("BACKFILL_BUDGET_CHANGED"));
    }
    let job = jobvalue["job_id"]
        .as_str()
        .ok_or_else(|| error("BACKFILL_STATE_INVALID"))?;
    let baseline = jobvalue["baseline_call_id"]
        .as_i64()
        .ok_or_else(|| error("BACKFILL_STATE_INVALID"))?;
    if crate::store::file_sha(&dir.join("reports-before-backfill.sqlite3"))?
        != jobvalue["backup_sha256"]
    {
        return Err(error("BACKFILL_BACKUP_CHANGED"));
    }
    app.store.register_backfill_budget(job, baseline, cap)?;
    let statepath = dir.join("state-v3.json");
    if initial.is_none() && dir.join("state.json").exists() {
        let old = read_json(&dir.join("state.json"))?;
        if old["start_date"] != jobvalue["start_date"]
            || old["end_date"] != jobvalue["end_date"]
            || old["baseline_call_id"] != jobvalue["baseline_call_id"]
            || old["backup_sha256"] != jobvalue["backup_sha256"]
            || old["call_budget"] != jobvalue["call_budget"]
        {
            return Err(error("BACKFILL_STATE_INVALID"));
        }
        initial = Some(old);
    }
    let mut state=read_state(app,job,&statepath)?.or(initial).unwrap_or_else(||json!({"start_date":start.to_string(),"end_date":end.to_string(),"total_dates":selected.len(),"call_budget":cap,"baseline_call_id":baseline,"backup_sha256":jobvalue["backup_sha256"],"dates":{},"created_at":now.to_rfc3339()}));
    state["job_id"] = json!(job);
    state["version"] = json!(3);
    state.as_object_mut().unwrap().remove("stop_reason");
    save_state(app, job, &statepath, &state)?;
    let mut rows = preflight["dates"].as_array().unwrap().clone();
    rows.sort_by_key(|row| {
        (
            !matches!(row["status"].as_str(), Some("existing" | "no_changes")),
            row["date"].as_str().unwrap().to_owned(),
        )
    });
    for row in rows {
        let date_text = row["date"].as_str().unwrap();
        let date = parse_date(date_text)?;
        if args.only_failed && state["dates"][date_text]["status"] != "failed" {
            continue;
        }
        if args.only_no_model && !matches!(row["status"].as_str(), Some("existing" | "no_changes"))
        {
            continue;
        }
        let result = (|| -> Result<Value, ReportError> {
            if let Some(existing) = existing(app, date_text)? {
                return Ok(existing);
            }
            let mut input = if let Some(path) = state["dates"][date_text]["input_path"].as_str() {
                let (value, sha) = read_envelope(Path::new(path), "input")?;
                if state["dates"][date_text]["input_sha256"] != sha {
                    return Err(error("BACKFILL_INPUT_CHANGED"));
                }
                serde_json::from_value(value)?
            } else {
                input_for(app, &row, date, now)?
            };
            if args.only_no_model && !input.plan.batches.is_empty() {
                return Err(error("BACKFILL_MODEL_NOT_ALLOWED"));
            }
            if args.only_failed {
                if let Some(id) = state["dates"][date_text]["run_id"].as_str() {
                    let old = app.store.get(id)?;
                    let calls = app.store.calls(id)?;
                    if calls
                        .iter()
                        .any(|r| !matches!(r["status"].as_str(), Some("succeeded" | "failed")))
                    {
                        return Err(error("MODEL_CALL_RESULT_UNKNOWN"));
                    }
                    let mut recovery = input.recovery_inputs.take().unwrap_or_default();
                    for row in calls {
                        let pin = FrozenCall::freeze(&old, &row)?;
                        if pin.status == "succeeded" {
                            recovery
                                .reused_calls
                                .insert(pin.logical_step.clone(), pin.clone());
                        }
                        recovery.prior_calls.push(pin);
                    }
                    if input.context.config_fingerprint != app.fingerprint(false)? {
                        if !recovery.reused_calls.is_empty() {
                            return Err(error("BACKFILL_INPUT_CONFIG_CHANGED"));
                        }
                        let request = request(app, date, now)?;
                        input = run::prepare_daily_input(app, &request, &app.scan(&request)?)?;
                    }
                    input.recovery_inputs = Some(recovery);
                    input.context =
                        RunContext::new(new_run_id(), now, date, app.fingerprint(false)?)?;
                }
            }
            let mut recovery = input.recovery_inputs.take().unwrap_or_default();
            recovery.job_budget = Some(JobBudget {
                job_id: job.into(),
                tokens_limit: u64::MAX,
                calls_limit: cap,
                cost_limit_microunits: None,
                revision_sha256: None,
            });
            input.recovery_inputs = Some(recovery);
            input.input_fingerprint = fingerprint(
                &json!({"input":input.input_fingerprint,"backfill":job,"run":input.context.run_id,"recovery":input.recovery_inputs}),
            )?;
            // Existing immutable inputs are reused verbatim on ordinary resume.
            let path = dir
                .join("inputs")
                .join(format!("{}.json", input.context.run_id));
            if path.exists() {
                let (value, _) = read_envelope(&path, "input")?;
                input = serde_json::from_value(value)?;
            }
            let sha = write_envelope(&path, "input", &serde_json::to_value(&input)?)?;
            state["dates"][date_text] = json!({"date":date_text,"status":"prepared","run_id":input.context.run_id,"input_path":path,"input_sha256":sha});
            save_state(app, job, &statepath, &state)?;
            let reason = format!("backfill:{job}");
            let id = run::prepare_frozen(app, &input, true, &reason)?;
            let outcome = run::resume_frozen(app, &id, now)?;
            let mut entry =
                existing(app, date_text)?.ok_or_else(|| error("BACKFILL_PUBLICATION_MISSING"))?;
            entry["action"] = json!(outcome.action);
            entry["run_model_calls"] = json!(outcome.model_calls);
            Ok(entry)
        })();
        match result {
            Ok(entry) => state["dates"][date_text] = entry,
            Err(e) => {
                let entry = &mut state["dates"][date_text];
                if !entry.is_object() {
                    *entry = json!({"date":date_text});
                }
                entry["status"] = json!("failed");
                entry["error_code"] = json!(e.code);
                if matches!(
                    e.code,
                    "BACKFILL_CALL_BUDGET_EXHAUSTED"
                        | "MODEL_CALL_BUDGET_EXHAUSTED"
                        | "MODEL_HTTP_401"
                        | "MODEL_HTTP_403"
                        | "MODEL_CREDENTIAL_MISSING"
                ) {
                    state["stop_reason"] = json!(e.code);
                }
            }
        }
        state["calls_used"] = json!(app.store.calls_after(baseline)?);
        state["updated_at"] = json!(now.to_rfc3339());
        save_state(app, job, &statepath, &state)?;
        if state.get("stop_reason").is_some() {
            break;
        }
    }
    state["calls_used"] = json!(app.store.calls_after(baseline)?);
    let complete = state["dates"]
        .as_object()
        .unwrap()
        .values()
        .filter(|r| r["status"] == "complete")
        .count();
    state["complete"] = json!(complete == selected.len());
    if complete == selected.len() {
        app.store.connection()?.execute(
            "INSERT OR IGNORE INTO report_metadata(key,value) VALUES(?,?)",
            params![
                format!("backfill_complete:{job}"),
                json!({"calls_used":state["calls_used"],"finished_at":now.to_rfc3339()})
                    .to_string()
            ],
        )?;
    }
    let mut index=format!("# 历史日报补录：{start} 至 {end}\n\n已保存并核验 {complete} / {} 天，累计调用 {} / {cap} 次。\n\n历史补录依据当前文件的修改时间和现有内容，无法还原已覆盖的历史版本。OCR 文字需核对原件。\n\n| 日期 | 状态 | 日报 |\n| --- | --- | --- |\n",selected.len(),state["calls_used"]);
    for date in selected {
        let date = date.to_string();
        let row = &state["dates"][&date];
        let label = if row["status"] == "complete" {
            match row["quality"].as_str() {
                Some("no_changes") => "未发现修改文件",
                Some("partial") => "部分覆盖",
                Some("complete") => "完整覆盖",
                _ => "保留既有日报",
            }
        } else {
            row["error_code"].as_str().unwrap_or("待补录")
        };
        let link = if row["status"] == "complete" {
            format!("[打开]({}/{date}.md)", &date[..7])
        } else {
            "—".into()
        };
        index.push_str(&format!("| {date} | {label} | {link} |\n"));
    }
    let index_path = app
        .config
        .paths
        .reports_dir
        .join(format!("历史日报补录_{start}_{end}.md"));
    // Previous Python indexes have no ownership hash; retain those and use an immutable Rust index.
    let index_path = if index_path.exists() && state["index_sha256"].is_null() {
        dir.join(format!("index-{}.md", digest(index.as_bytes())))
    } else {
        index_path
    };
    sync_file(
        &index_path,
        index.as_bytes(),
        state["index_sha256"].as_str(),
    )?;
    state["index_path"] = json!(index_path);
    state["index_sha256"] = json!(digest(index.as_bytes()));
    save_state(app, job, &statepath, &state)?;
    Ok(state)
}

fn coverage_command(
    app: &App,
    args: &BackfillArgs,
    now: DateTime<Utc>,
) -> Result<Value, ReportError> {
    use super::coverage;
    if args.start_date.is_some()
        || args.end_date.is_some()
        || args.preflight.is_some()
        || args.only_failed
        || args.only_no_model
        || args.ocr_manifest.is_some()
        || args.output_tokens.is_some()
    {
        return Err(error("BACKFILL_OPTIONS_INVALID"));
    }
    if let Some(candidates) = &args.publish_candidates {
        if args.repair_coverage
            || args.dry_run
            || args.generate_candidates
            || args.plan.is_some()
            || args.call_budget.is_some()
            || args.token_budget.is_some()
            || args.cost_budget_microunits.is_some()
            || !args.dates.is_empty()
            || !args.gap_codes.is_empty()
            || args.scope_root.is_some()
            || args.refresh_preflight
        {
            return Err(error("BACKFILL_OPTIONS_INVALID"));
        }
        return coverage::publish(app, candidates, now);
    }
    if args.dry_run && !args.generate_candidates && args.plan.is_none() {
        let path = coverage::make_plan(
            app,
            coverage::PlanRequest {
                dates: &args.dates,
                gap_codes: &args.gap_codes,
                scope_root: args
                    .scope_root
                    .as_deref()
                    .ok_or_else(|| error("BACKFILL_OPTIONS_INVALID"))?,
                calls_limit: args
                    .call_budget
                    .ok_or_else(|| error("BACKFILL_OPTIONS_INVALID"))?,
                tokens_limit: args
                    .token_budget
                    .ok_or_else(|| error("BACKFILL_OPTIONS_INVALID"))?,
                cost_limit_microunits: args.cost_budget_microunits,
                refresh: args.refresh_preflight,
            },
            now,
        )?;
        return Ok(json!({"artifact":path,"new_model_calls":0}));
    }
    if args.generate_candidates
        && !args.dry_run
        && !args.refresh_preflight
        && args.scope_root.is_none()
        && args.call_budget.is_none()
        && args.token_budget.is_none()
        && args.cost_budget_microunits.is_none()
        && args.dates.is_empty()
        && args.gap_codes.is_empty()
    {
        return Ok(
            json!({"artifact":coverage::generate(app,args.plan.as_deref().ok_or_else(||error("BACKFILL_OPTIONS_INVALID"))?,now)?}),
        );
    }
    Err(error("BACKFILL_OPTIONS_INVALID"))
}
