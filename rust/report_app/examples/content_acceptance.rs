//! Development acceptance runner. Uses only the seven synthetic source files in the fixture.
//! `prepare` makes no model calls; `send` must be explicitly authorized by the operator.
use ai_daily_report::{
    config::AppConfig,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    run::{prepare_daily_input, run_report, App},
    snapshots::canonical_json,
    types::{new_run_id, Period, ReportMode, ReportRequest, ReportSource, RunContext},
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::{
    fs::{self, File, FileTimes},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, UNIX_EPOCH},
};

struct AcceptanceTransport {
    inner: Box<dyn ModelTransport>,
    used: Arc<AtomicU64>,
}
impl ModelTransport for AcceptanceTransport {
    fn prepare(&self) -> Result<(), ai_daily_report::ReportError> {
        self.inner.prepare()
    }
    fn send(&self, body: &[u8], timeout: Duration) -> Result<HttpResponse, TransportFailure> {
        self.used.fetch_add(1, Ordering::SeqCst);
        self.inner.send(body, timeout)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3
        || !matches!(
            args[1].as_str(),
            "prepare"
                | "send"
                | "retry"
                | "recompose"
                | "refresh"
                | "recompose-current"
                | "topic-refresh"
        )
    {
        return Err("usage: content_acceptance prepare|send|retry|recompose|refresh|recompose-current OUTPUT_DIRECTORY".into());
    }
    let root = fs::canonicalize(&args[2])?;
    let settings: Value = serde_json::from_slice(&fs::read(root.join("model-settings.json"))?)?;
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("../tests/fixtures/content-cases.json"))?;
    let mut results = Vec::new();
    let mut previous = 0u64;
    for case in &cases {
        let db = root
            .join(case["case"].as_str().unwrap())
            .join("data/db/reports.sqlite3");
        if db.exists() {
            let conn = rusqlite::Connection::open_with_flags(
                db,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            previous += u64::try_from(conn.query_row(
                "SELECT COUNT(*) FROM report_model_calls",
                [],
                |r| r.get::<_, i64>(0),
            )?)?;
        }
    }
    let used = Arc::new(AtomicU64::new(previous));
    let target = ai_daily_report::types::parse_date("2026-10-11")?;
    let stamp = DateTime::parse_from_rfc3339("2026-10-11T02:00:00Z")?.timestamp() as u64;
    let workers = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/debug"));
    for case in cases {
        let name = case["case"].as_str().ok_or("case name missing")?;
        if args[1] == "topic-refresh" && name != "same_topic_across_sources" {
            continue;
        }
        let dir = root.join(name);
        if args[1] == "topic-refresh" && dir.join("candidate-v7.md").exists() {
            continue;
        }
        if args[1] == "recompose-current" && dir.join("candidate-v5.md").exists() {
            results.push(json!({"case":name,"status":"existing_candidate_preserved"}));
            continue;
        }
        if args[1] == "refresh" && dir.join("candidate-v3.md").exists() {
            results.push(json!({"case":name,"status":"existing_candidate_preserved"}));
            continue;
        }
        if args[1] == "recompose" && dir.join("candidate-v2.md").exists() {
            results.push(json!({"case":name,"status":"existing_candidate_preserved"}));
            continue;
        }
        fs::create_dir_all(dir.join("config"))?;
        fs::create_dir_all(dir.join("work"))?;
        if args[1] == "prepare" {
            for source in case["sources"].as_array().ok_or("sources missing")? {
                let path = dir
                    .join("work")
                    .join(source["path"].as_str().ok_or("source path missing")?);
                fs::write(&path, source["text"].as_str().ok_or("source text missing")?)?;
                File::options().write(true).open(path)?.set_times(
                    FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(stamp)),
                )?;
            }
            fs::write(
                dir.join("config/settings.yaml"),
                canonical_json(
                    &json!({"paths":{"work_dir":"work"},"llm":settings["llm"],"recovery":settings["recovery"]}),
                )?,
            )?;
            fs::write(
                dir.join("expected.json"),
                canonical_json(&case["expected"])?,
            )?;
        }
        let mut config = AppConfig::load(&dir, Some(&dir.join("config")))?;
        config.office_worker_path = workers.join("ai-daily-office-parser.exe");
        config.pdf_worker_path = workers.join("ai-daily-pdf-parser.exe");
        if args[1] == "recompose" {
            config.llm.max_retries = 3;
        }
        let now = Utc::now();
        let mut app = App::open(config, now)?;
        app.transport = Box::new(AcceptanceTransport {
            inner: app.transport,
            used: used.clone(),
        });
        let request = ReportRequest {
            context: RunContext::new(new_run_id(), now, target, String::new())?,
            period: Period {
                mode: ReportMode::Daily,
                source: ReportSource::Scan,
                start: target,
                end: target,
                label: target.to_string(),
            },
            input: String::new(),
            save: !matches!(args[1].as_str(), "refresh" | "topic-refresh"),
            dry_run: false,
            scheduled: false,
            regeneration_reason: if matches!(args[1].as_str(), "refresh" | "topic-refresh")
                || args[1] == "retry" && !dir.join("candidate.md").exists()
            {
                Some("授权内容验收：验证当前提示词；保留已有调用".into())
            } else {
                None
            },
        };
        if args[1] == "prepare" {
            let scan = app.scan(&request)?;
            let mut request = request;
            request.context.config_fingerprint = app.fingerprint(false)?;
            let input = prepare_daily_input(&app, &request, &scan)?;
            if input.plan.batches.is_empty() {
                return Err(format!("{name}: no fact batches").into());
            }
            let normal = input.plan.batches.len() + 1;
            let maximum = normal * app.config.llm.max_retries as usize;
            fs::write(
                dir.join("prepared-input.json"),
                canonical_json(&serde_json::to_value(&input)?)?,
            )?;
            results.push(json!({"case":name,"facts_batches":input.plan.batches.len(),"normal_calls":normal,"maximum_calls":maximum,"request_bytes":input.plan.batches.iter().map(|batch|batch.bytes().map(|bytes|bytes.len())).collect::<Result<Vec<_>,_>>()?,"fact_capacity_bytes":input.plan.batches.iter().map(|batch|batch.output_capacity_bytes).collect::<Vec<_>>(),"status":"prepared_no_model_calls"}));
        } else {
            let result = if matches!(args[1].as_str(), "recompose" | "recompose-current") {
                use ai_daily_report::{
                    model::recovery::{FrozenCall, RecoveryInputs},
                    run::{prepare_frozen, resume_frozen, FrozenInput},
                    snapshots::{fingerprint, SnapshotRef},
                };
                let parent = if args[1] == "recompose-current" {
                    let id:String=app.store.connection()?.query_row("SELECT run_id FROM report_runs WHERE run_kind='preview' AND regeneration_reason=? AND target_date=? ORDER BY created_at DESC LIMIT 1",rusqlite::params!["授权内容验收：验证当前提示词；保留已有调用",target.to_string()],|r|r.get(0))?;
                    app.store.get(&id)?
                } else {
                    app.store
                        .current(&target.to_string())?
                        .ok_or("generated report missing")?
                };
                let reference = SnapshotRef {
                    path: parent
                        .snapshot_path
                        .clone()
                        .ok_or("snapshot path missing")?,
                    sha256: parent
                        .snapshot_sha256
                        .clone()
                        .ok_or("snapshot hash missing")?,
                };
                let mut input: FrozenInput =
                    serde_json::from_value(app.snapshots.read(&reference)?)?;
                let mut recovery = RecoveryInputs::default();
                for row in app.store.calls(&parent.run_id)? {
                    let pin = FrozenCall::freeze(&parent, &row)?;
                    if pin.status == "succeeded" && pin.logical_step.starts_with("batch_") {
                        recovery
                            .reused_calls
                            .insert(pin.logical_step.clone(), pin.clone());
                    }
                    recovery.prior_calls.push(pin);
                }
                input.context =
                    RunContext::new(new_run_id(), now, target, app.fingerprint(false)?)?;
                input.recovery_fingerprint = app.fingerprint(true)?;
                input.recovery_inputs = Some(recovery);
                input.input_fingerprint = fingerprint(
                    &json!({"parent":input.input_fingerprint,"compose_review":2,"recovery":input.recovery_inputs}),
                )?;
                let id = prepare_frozen(
                    &app,
                    &input,
                    false,
                    "授权验收：复用事实，验证去重后的成稿提示词",
                )?;
                resume_frozen(&app, &id, now)
            } else {
                run_report(request, &app)
            };
            match result {
                Ok(outcome) => {
                    fs::write(
                        dir.join(if args[1] == "topic-refresh" {
                            "candidate-v7.md"
                        } else if args[1] == "recompose-current" {
                            "candidate-v5.md"
                        } else if args[1] == "refresh" {
                            "candidate-v3.md"
                        } else if args[1] == "recompose" {
                            "candidate-v2.md"
                        } else {
                            "candidate.md"
                        }),
                        outcome.markdown.as_bytes(),
                    )?;
                    results.push(json!({"case":name,"outcome":outcome,"status":"generated_awaiting_content_review"}));
                }
                Err(error) => {
                    results.push(json!({"case":name,"status":"failed","error_code":error.code}));
                    if args[1] == "retry" {
                        break;
                    }
                }
            }
        }
    }
    let file = if args[1] == "prepare" {
        "prepared.json".to_owned()
    } else if matches!(
        args[1].as_str(),
        "retry" | "recompose" | "refresh" | "recompose-current" | "topic-refresh"
    ) {
        format!("generated-{}-{}.json", args[1], new_run_id())
    } else {
        "generated.json".to_owned()
    };
    fs::write(root.join(file), serde_json::to_vec_pretty(&results)?)?;
    println!("{}", serde_json::to_string_pretty(&results)?);
    println!(
        "Total acceptance calls recorded: {}",
        used.load(Ordering::SeqCst)
    );
    Ok(())
}
