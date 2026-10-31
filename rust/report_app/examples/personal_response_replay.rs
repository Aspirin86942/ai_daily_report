//! Replay an immutable saved response through the real validator in a temporary store.
//! Production databases and snapshots are opened read-only; transport never uses HTTP.
use ai_daily_report::{
    config::AppConfig,
    model::{
        transport::{HttpResponse, ModelTransport, TransportFailure},
        ModelRunner,
    },
    reports::personal::{body_chars, PersonalReport, StatementKind, MAX_REPORT_BYTES},
    run::personal::PersonalInput,
    snapshots::{digest, SnapshotStore},
    store::Store,
};
use chrono::Utc;
use clap::Parser;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config_dir: PathBuf,
    #[arg(long)]
    call_id: i64,
}

struct SavedResponse {
    responses: BTreeMap<Vec<u8>, Vec<u8>>,
}
impl ModelTransport for SavedResponse {
    fn send(&self, body: &[u8], _: Duration) -> Result<HttpResponse, TransportFailure> {
        let response = self
            .responses
            .get(body)
            .expect("replay request has no matching saved response");
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: response.clone(),
        })
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let config = AppConfig::load(&std::env::current_dir()?, Some(&args.config_dir))?;
    let database = Connection::open_with_flags(
        config.paths.db_dir.join("reports.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let (input_path, input_hash, response_path, response_hash): (String, String, String, String) = database.query_row(
        "SELECT r.snapshot_path,r.snapshot_sha256,c.response_path,c.response_sha256 FROM report_model_calls c JOIN report_runs r ON r.run_id=c.run_id WHERE c.call_id=?",
        [args.call_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?;
    let snapshots_root = config.paths.data_dir.join("daily_cache/snapshots");
    let input_bytes = fs::read(snapshots_root.join(input_path))?;
    let response_bytes = fs::read(snapshots_root.join(response_path))?;
    if digest(&input_bytes) != input_hash || digest(&response_bytes) != response_hash {
        return Err("saved input or response hash mismatch".into());
    }
    let input: PersonalInput = serde_json::from_slice(&input_bytes)?;
    let plan = input.plan.as_ref().ok_or("frozen plan missing")?;
    input.selection.validate()?;
    let temporary = tempfile::tempdir()?;
    let now = Utc::now();
    let store = Store::open(&temporary.path().join("reports.sqlite3"), now)?;
    let snapshots = SnapshotStore::new(temporary.path().join("snapshots"), 16 * 1024 * 1024)?;
    let id = &input.context.run_id;
    store.create(&input.context, "preview", id, None, now)?;
    store.register_personal_run(id)?;
    let saved = snapshots.put_bytes(id, "personal-input", &input_bytes)?;
    store.prepare(id, &saved, &input.input_fingerprint, &[], now)?;
    let mut responses = BTreeMap::new();
    let compaction_plan = matches!(plan.target["summary_prompt_version"].as_u64(), Some(4 | 6));
    if compaction_plan {
        let mut statement = database.prepare(
            "SELECT request_path,request_sha256,response_path,response_sha256 FROM report_model_calls WHERE run_id=? AND status='succeeded' ORDER BY call_id",
        )?;
        for row in statement.query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })? {
            let (request_path, request_hash, response_path, response_hash) = row?;
            let request = fs::read(snapshots_root.join(request_path))?;
            let response = fs::read(snapshots_root.join(response_path))?;
            if digest(&request) != request_hash || digest(&response) != response_hash {
                return Err("saved compaction request or response hash mismatch".into());
            }
            responses.insert(request, response);
        }
    } else {
        responses.insert(plan.bytes()?, response_bytes);
    }
    let transport = SavedResponse { responses };
    let started = Instant::now();
    let runner = ModelRunner {
        store: &store,
        snapshots: &snapshots,
        transport: &transport,
        llm: &config.llm,
        recovery_inputs: None,
        allow_new_calls: true,
        run_id: id,
        started_at: now,
        started,
        deadline: started + Duration::from_secs(30),
    };
    let output = ai_daily_report::model::personal::generate(&runner, plan)?;
    let mut report = PersonalReport::from_selection(&input.context, "work", &input.selection);
    report.items = output.items;
    report.retain_cited();
    let initial_items = report.items.len();
    let body = loop {
        report.evidence_attachment = None;
        report.evidence_attachment = report.render_attachment(&input.template)?;
        if report.output_bytes("") <= MAX_REPORT_BYTES {
            let body = report.render(&input.template)?;
            if report.output_bytes(&body) <= MAX_REPORT_BYTES {
                break body;
            }
        }
        let remove = if compaction_plan {
            report
                .items
                .iter()
                .rposition(|item| {
                    item.kind != ai_daily_report::reports::personal::ItemKind::NextStep
                })
                .ok_or("required next-step evidence exceeds report budget")?
        } else {
            report
                .items
                .len()
                .checked_sub(1)
                .ok_or("required evidence exceeds report budget")?
        };
        report.items.remove(remove);
        report.retain_cited();
    };
    if report.items.is_empty() {
        return Err("saved response has no publishable items".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status":"passed", "call_id":args.call_id, "production_read_only":true,
            "remote_model_calls":0, "isolated_fixture_calls":store.calls(id)?.len(),
            "input_sha256":input_hash, "response_sha256":response_hash,
            "initial_items":initial_items, "publishable_items":report.items.len(),
            "omitted_items":output.omitted, "body_chars":body_chars(&report.items),
            "uncertain_items":report.items.iter().filter(|i| i.statement_kind == StatementKind::Uncertain).count(),
            "output_bytes":body.len(), "sources":report.sources.len(),
            "references":report.evidence.values().map(Vec::len).sum::<usize>(),
        }))?
    );
    Ok(())
}
