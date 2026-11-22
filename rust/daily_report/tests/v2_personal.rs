use ai_daily_report::{
    config::AppConfig,
    model::transport::{HttpResponse, ModelTransport, TransportFailure},
    reports::personal::{PersonalReport, MAX_REPORT_BYTES, SCAN_TEMPLATE_V7},
    run::{repair_by_id, repair_output, run_daily, App},
    snapshots::SnapshotRef,
    types::{new_run_id, Period, ReportMode, ReportRequest, ReportSource, RunContext},
};
use chrono::{DateTime, Days, NaiveDate, Utc};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TITLE: &str = "核对进度😀";
const FACT: &str = "金额从 100 元变为 120 元，仍待批准，未生效。";
const NEXT: &str = "审批有结论后，再按确认的日期跟进。";

fn fixed_now() -> DateTime<Utc> {
    "2026-11-22T02:00:00Z".parse().unwrap()
}

fn read_request(stream: &mut TcpStream) -> (String, Value) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut target = String::new();
    reader.read_line(&mut target).unwrap();
    let mut length = None;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let length = length.unwrap();
    assert!(length < MAX_REPORT_BYTES);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (target, serde_json::from_slice(&body).unwrap())
}

fn loopback(replies: Vec<Option<Value>>) -> (String, JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let thread = thread::spawn(move || {
        let mut requests = Vec::new();
        for reply in replies {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "缺少预期的 HTTP 请求");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            requests.push(read_request(&mut stream));
            if let Some(reply) = reply {
                let body = json!({
                    "choices":[{"finish_reason":"stop", "message":{"content":reply.to_string()}}],
                    "usage":{"prompt_tokens":150, "completion_tokens":100}
                })
                .to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        }
        requests
    });
    (address, thread)
}

fn config(root: &Path, provider: &str, base: &str) -> AppConfig {
    fs::create_dir_all(root.join("config")).unwrap();
    fs::create_dir_all(root.join("work")).unwrap();
    fs::write(
        root.join("config/settings.yaml"),
        json!({
            "paths":{"work_dir":"work"}, "report":{"profile":"personal"},
            "llm":{"provider":provider, "base_url":base, "model_id":"synthetic",
                "max_request_utf8_bytes":128000, "context_window_tokens":128000,
                "verified_max_output_tokens":8192, "max_tokens":4096,
                "invalid_response_retries":0, "request_timeout_seconds":3},
            "archive":{"allow_work_dir_for_model":true},
        })
        .to_string(),
    )
    .unwrap();
    let env = BTreeMap::from([
        ("DEEPSEEK_API_KEY".into(), "synthetic-test".into()),
        ("OPENAI_API_KEY".into(), "synthetic-test".into()),
    ]);
    AppConfig::load_with_env(root, None, &env).unwrap()
}

fn request() -> ReportRequest {
    let now = fixed_now();
    let day = now.date_naive() - Days::new(1);
    ReportRequest {
        context: RunContext::new(new_run_id(), now, day, String::new()).unwrap(),
        period: Period {
            mode: ReportMode::Daily,
            source: ReportSource::Scan,
            start: day,
            end: day,
            label: day.to_string(),
        },
        input: format!("{FACT}\n\n{NEXT}"),
        save: true,
        dry_run: false,
        scheduled: false,
        regeneration_reason: None,
    }
}

fn item(text: &str) -> Value {
    json!({"title":TITLE, "text":text, "kind":"progress",
        "statement_kind":"user_statement", "source_ids":["user_input"]})
}

fn envelope(items: Vec<Value>) -> Value {
    json!({"schema_version":5, "items":items})
}

fn published_body(markdown: &str) -> &str {
    markdown
        .split_once("\n\n")
        .unwrap()
        .1
        .split_once("\n[完整依据](")
        .unwrap()
        .0
}

fn exact_draft(day: NaiveDate, size: usize) -> (String, String) {
    let link = format!("attachments/{day}.{}.md", "0".repeat(64));
    let fixed = format!("- **{TITLE}** · 进展 · 用户陈述\n\n   [S001]({link}#s001)\n");
    let remaining = size - fixed.chars().count();
    let count = remaining / FACT.chars().count();
    let padding = "😀".repeat(remaining % FACT.chars().count());
    (
        format!("{}{padding}", FACT.repeat(count)),
        format!("{FACT}{padding}"),
    )
}

fn saved_report(app: &App, id: &str) -> PersonalReport {
    serde_json::from_str(&app.store.get(id).unwrap().report_json.unwrap()).unwrap()
}

#[test]
fn final_markdown_counts_chinese_emoji_links_and_newlines() {
    for (size, provider) in [(1499, "deepseek"), (1500, "openai"), (1501, "openai")] {
        let dir = tempfile::tempdir().unwrap();
        let mut request = request();
        request.scheduled = size == 1499;
        let (draft, short) = exact_draft(request.context.target_date, size);
        let mut replies = vec![Some(envelope(vec![item(&draft)]))];
        if size > 1500 {
            let mut compact = item(&short);
            compact["item_id"] = json!(1);
            replies.push(Some(envelope(vec![compact])));
        }
        let (url, server) = loopback(replies);
        let app = App::open(config(dir.path(), provider, &url), fixed_now()).unwrap();
        let outcome = run_daily(request, &app).unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].0.starts_with("POST /"));
        assert_eq!(outcome.model_calls, requests.len() as u64);
        assert!(published_body(&outcome.markdown)
            .contains("金额从 100 元变为 120 元，仍待批准，未生效。"));
        assert!(published_body(&outcome.markdown).chars().count() <= 1500);
        if size <= 1500 {
            assert_eq!(published_body(&outcome.markdown).chars().count(), size);
            assert_eq!(requests.len(), 1);
        } else {
            assert_eq!(requests.len(), 2);
            let compact: Value =
                serde_json::from_str(requests[1].1["messages"][1]["content"].as_str().unwrap())
                    .unwrap();
            assert_eq!(compact["draft_chars"], 1501);
            assert_eq!(compact["max_chars"], 1500);
            assert_eq!(
                app.store.calls(&outcome.run_id).unwrap()[1]["status"],
                "succeeded"
            );
        }
        let saved = saved_report(&app, &outcome.run_id);
        assert_eq!(saved.schema_version, 6);
        assert_eq!(
            app.store.current(&saved.date).unwrap().unwrap().run_id,
            outcome.run_id
        );
        assert_eq!(
            app.store
                .personal_calls("personal_work", &saved.date)
                .unwrap(),
            outcome.model_calls
        );
    }
}

#[test]
fn complete_uncited_evidence_survives_repair_by_date_and_run_id() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = loopback(vec![Some(envelope(vec![item(FACT)]))]);
    let config = config(dir.path(), "openai", &url);
    let source = config.paths.work_dir.join("sheet-note.md");
    fs::write(
        &source,
        "附件独有数值 18446744073709551616；范围 A < B & C > D。",
    )
    .unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified((fixed_now() - Days::new(1)).into())
        .unwrap();
    let app = App::open(config, fixed_now()).unwrap();
    let outcome = run_daily(request(), &app).unwrap();
    let requests = server.join().unwrap();
    let request: Value =
        serde_json::from_str(requests[0].1["messages"][1]["content"].as_str().unwrap()).unwrap();
    assert!(request["sources"].as_array().unwrap().len() >= 2);
    let saved = saved_report(&app, &outcome.run_id);
    assert_eq!(saved.items[0].source_ids, ["user_input"]);
    let attachment = saved.evidence_attachment.unwrap();
    let link = attachment.link(&saved.date);
    let summary = outcome.output_path.unwrap();
    let path = summary.parent().unwrap().join(&link);
    assert!(attachment.markdown.contains("sheet-note.md"));
    assert!(attachment
        .markdown
        .contains("18446744073709551616；范围 A &lt; B &amp; C &gt; D"));
    assert!(attachment.markdown.contains("<a id=\"s001\"></a>"));
    assert!(published_body(&outcome.markdown).contains(&format!("[S001]({link}#s001)")));
    assert!(outcome.markdown.len() + attachment.markdown.len() <= MAX_REPORT_BYTES);
    fs::remove_file(&path).unwrap();
    repair_output(&app, &saved.date, fixed_now()).unwrap();
    fs::remove_file(&summary).unwrap();
    repair_by_id(&app, &outcome.run_id, fixed_now()).unwrap();
    assert_eq!(fs::read(summary).unwrap(), outcome.markdown.as_bytes());
    assert_eq!(fs::read(path).unwrap(), attachment.markdown.as_bytes());
}

#[test]
fn compaction_with_changed_facts_keeps_complete_next_step_and_original_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut next = item(NEXT);
    next["kind"] = json!("next_step");
    next["title"] = json!("跟进审批");
    let draft = envelope(vec![item(&FACT.repeat(90)), next.clone()]);
    let mut changed = item("金额从 100 元变为 140 元，已经批准并生效。");
    changed["item_id"] = json!(1);
    next["item_id"] = json!(2);
    let (url, server) = loopback(vec![Some(draft), Some(envelope(vec![changed, next]))]);
    let app = App::open(config(dir.path(), "deepseek", &url), fixed_now()).unwrap();
    let outcome = run_daily(request(), &app).unwrap();
    assert_eq!(server.join().unwrap().len(), 2);
    let report = saved_report(&app, &outcome.run_id);
    assert_eq!(report.items.len(), 1);
    assert_eq!(report.items[0].text, NEXT);
    assert!(report.evidence_attachment.unwrap().markdown.contains(FACT));
    assert!(report
        .gaps
        .iter()
        .any(|gap| gap.code == "MODEL_RESPONSE_INVALID"));
    assert!(report
        .gaps
        .iter()
        .any(|gap| gap.code == "PERSONAL_OUTPUT_REDUCED"));
    assert!(!published_body(&outcome.markdown).contains("140"));
    assert_eq!(
        app.store.calls(&outcome.run_id).unwrap()[1]["status"],
        "failed"
    );
}

#[test]
fn interrupted_http_call_is_counted_and_the_same_frozen_step_is_not_sent_again() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = loopback(vec![None]);
    let app = App::open(config(dir.path(), "deepseek", &url), fixed_now()).unwrap();
    let req = request();
    let id = req.context.run_id.clone();
    let day = req.context.target_date;
    assert!(run_daily(req, &app).unwrap_err().code.starts_with("MODEL_"));
    assert_eq!(server.join().unwrap().len(), 1);
    assert_eq!(app.store.calls(&id).unwrap()[0]["status"], "unknown");
    let error = run_daily(request(), &app).unwrap_err();
    assert_eq!(error.code, "MODEL_CALL_RESULT_UNKNOWN");
    assert_eq!(app.store.calls(&id).unwrap().len(), 1);
    assert_eq!(
        app.store
            .personal_calls("personal_work", &day.to_string())
            .unwrap(),
        1
    );
    assert!(app.store.current(&day.to_string()).unwrap().is_none());
}

#[test]
fn failed_compaction_with_only_overlong_next_step_preserves_current_publication() {
    let dir = tempfile::tempdir().unwrap();
    let mut next = item(&NEXT.repeat(100));
    next["kind"] = json!("next_step");
    let (url, server) = loopback(vec![
        Some(envelope(vec![item(FACT)])),
        Some(envelope(vec![next])),
        None,
    ]);
    let app = App::open(config(dir.path(), "openai", &url), fixed_now()).unwrap();
    let original = run_daily(request(), &app).unwrap();
    let path = original.output_path.unwrap();
    let mut retry = request();
    let id = retry.context.run_id.clone();
    retry.regeneration_reason = Some("验证超长下一步的失败恢复".into());
    let error = run_daily(retry, &app).unwrap_err();
    assert_eq!(error.code, "PERSONAL_REPORT_BODY_TOO_LONG");
    assert_eq!(server.join().unwrap().len(), 3);
    assert_eq!(
        app.store
            .current(&original.target_date)
            .unwrap()
            .unwrap()
            .run_id,
        original.run_id
    );
    assert_eq!(fs::read(path).unwrap(), original.markdown.as_bytes());
    let run = app.store.get(&id).unwrap();
    assert!(run.snapshot_path.is_some());
    assert_eq!(app.store.calls(&id).unwrap()[1]["status"], "unknown");
}

#[test]
fn local_dry_run_and_audited_preview_keep_full_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = loopback(vec![Some(envelope(vec![item(FACT)]))]);
    let app = App::open(config(dir.path(), "openai", &url), fixed_now()).unwrap();
    let mut dry_run = request();
    dry_run.dry_run = true;
    let scan = run_daily(dry_run, &app).unwrap();
    assert_eq!(scan.action, "dry_run");
    assert_eq!(scan.model_calls, 0);
    let material: Value = serde_json::from_str(&scan.markdown).unwrap();
    assert!(material["materials"][0]["text"]
        .as_str()
        .unwrap()
        .contains(NEXT));

    let mut preview = request();
    preview.save = false;
    let out = run_daily(preview, &app).unwrap();
    assert_eq!(server.join().unwrap().len(), 1);
    assert_eq!(out.action, "preview");
    assert!(out.output_path.is_none());
    assert!(out.markdown.contains("[完整依据](#personal-evidence)"));
    assert!(out.markdown.contains(NEXT));
    assert!(app.store.current(&out.target_date).unwrap().is_none());
    assert_eq!(app.store.calls(&out.run_id).unwrap().len(), 1);
}

struct BeforeSend;

impl ModelTransport for BeforeSend {
    fn prepare(&self) -> Result<(), ai_daily_report::ReportError> {
        panic!("synthetic interruption after plan freeze")
    }
    fn send(&self, _: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        unreachable!()
    }
}

#[test]
fn selected_template_override_bytes_and_request_survive_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = loopback(vec![Some(envelope(vec![item(FACT)]))]);
    let config = config(dir.path(), "openai", &url);
    let templates = config.config_dir.join("templates");
    fs::create_dir(&templates).unwrap();
    let prompt_path = templates.join("daily_prompt.md");
    let render_path = templates.join("daily_render_template.md");
    let prompt = "固定 v2 提示词\r\n保留原始换行。\r\n";
    let render = SCAN_TEMPLATE_V7.replace("个人日报", "冻结版日报");
    fs::write(&prompt_path, prompt).unwrap();
    fs::write(&render_path, &render).unwrap();
    let mut app = App::open(config.clone(), fixed_now()).unwrap();
    app.transport = Box::new(BeforeSend);
    let req = request();
    let id = req.context.run_id.clone();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_daily(req, &app))).is_err()
    );
    let row = app.store.get(&id).unwrap();
    let reference = SnapshotRef {
        path: row.snapshot_path.unwrap(),
        sha256: row.snapshot_sha256.unwrap(),
    };
    let path = app
        .processing
        .as_ref()
        .unwrap()
        .quota
        .root
        .join("snapshots")
        .join(reference.path);
    let input: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(input["version"], 7);
    assert_eq!(input["plan"]["version"], 8);
    assert_eq!(input["plan"]["target"]["summary_prompt_version"], 9);
    assert_eq!(input["plan"]["target"]["publication"]["max_chars"], 1500);
    assert_eq!(input["template"], render);
    assert_eq!(input["prompts"]["summary"], prompt);
    assert!(!input["prompts"]["compaction"].as_str().unwrap().is_empty());
    fs::write(prompt_path, "后续操作变更的提示词").unwrap();
    fs::write(render_path, "后续操作变更的模板").unwrap();
    drop(app);
    let app = App::open(config, fixed_now()).unwrap();
    let result = run_daily(request(), &app).unwrap();
    assert_eq!(result.run_id, id);
    assert!(result.markdown.starts_with("# 2026-11-21 冻结版日报"));
    assert_eq!(server.join().unwrap()[0].1, input["plan"]["request"]);
}

#[test]
fn oversized_attachment_is_rejected_before_the_first_model_request() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path(), "openai", "http://127.0.0.1:1");
    let templates = config.config_dir.join("templates");
    fs::create_dir(&templates).unwrap();
    let template = SCAN_TEMPLATE_V7.replace(
        "## 采纳材料原文",
        &format!("## 采纳材料原文\n{}", "证".repeat(MAX_REPORT_BYTES / 3)),
    );
    fs::write(templates.join("daily_render_template.md"), template).unwrap();
    let mut app = App::open(config, fixed_now()).unwrap();
    app.transport = Box::new(BeforeSend);
    let req = request();
    let id = req.context.run_id.clone();
    assert_eq!(
        run_daily(req, &app).unwrap_err().code,
        "PERSONAL_REPORT_TOO_LARGE"
    );
    let run = app.store.get(&id).unwrap();
    assert!(run.snapshot_path.is_some());
    assert!(app.store.calls(&id).unwrap().is_empty());
    assert!(app.store.current(&run.target_date).unwrap().is_none());
}
