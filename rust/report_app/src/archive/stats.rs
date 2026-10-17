use super::store::ArchiveStore;
use crate::{config::AppConfig, ReportError};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};

pub fn query(
    config: &AppConfig,
    from: Option<&str>,
    to: Option<&str>,
    run_id: Option<&str>,
) -> Result<Value, ReportError> {
    let mut collections = vec![];
    let mut storage = Value::Null;
    let mut files = vec![];
    let mut slow = vec![];
    let mut summed = Value::Null;
    let mut file_count = 0i64;
    let mut gaps = vec![];
    let mut parse_coverage = Value::Null;
    let mut report_stages = vec![];
    let root = config.paths.data_dir.join("archive");
    if root.join("index.sqlite3").is_file() {
        let store = ArchiveStore::at(&root, false)?;
        let mut stage = store.conn.prepare("SELECT value FROM metadata WHERE key LIKE 'stage:%' AND (?1 IS NULL OR coalesce(substr(datetime(json_extract(value,'$.recorded_at'),'+8 hours'),1,10),json_extract(value,'$.date'))>=?1) AND (?2 IS NULL OR coalesce(substr(datetime(json_extract(value,'$.recorded_at'),'+8 hours'),1,10),json_extract(value,'$.date'))<=?2) AND (?3 IS NULL OR json_extract(value,'$.run_id')=?3)")?;
        for row in stage.query_map(params![from, to, run_id], |r| r.get::<_, String>(0))? {
            report_stages.push(serde_json::from_str::<Value>(&row?)?);
        }
        let mut stmt=store.conn.prepare("SELECT id,started_at,finished_at,status,metrics_json FROM collections WHERE (?1 IS NULL OR substr(datetime(started_at,'+8 hours'),1,10)>=?1) AND (?2 IS NULL OR substr(datetime(started_at,'+8 hours'),1,10)<=?2) AND (?3 IS NULL OR id=?3) ORDER BY started_at")?;
        for row in stmt.query_map(params![from, to, run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })? {
            let (id, started, finished, status, metrics) = row?;
            collections.push(json!({"run_id":id,"started_at":started,"finished_at":finished,"status":status,"metrics":serde_json::from_str::<Value>(&metrics)?}));
        }
        let counts:(i64,i64,i64,i64)=store.conn.query_row("SELECT (SELECT count(*) FROM versions WHERE pruned_at IS NULL),(SELECT COALESCE(sum(size_bytes),0) FROM versions WHERE pruned_at IS NULL),(SELECT COALESCE(sum(size_bytes),0) FROM objects o WHERE EXISTS(SELECT 1 FROM versions v WHERE v.sha256=o.sha256 AND v.pruned_at IS NULL)),(SELECT count(*) FROM extractions WHERE profile=? AND (status IN('queued','running') OR id IN(SELECT value FROM metadata WHERE key LIKE 'retry:%')))",[super::parse::profile(config)?],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        storage = json!({"versions":counts.0,"logical_bytes":counts.1,"object_bytes":counts.2,"deduplicated_bytes":counts.1.saturating_sub(counts.2),"parse_queue":counts.3});
        let mut statuses = std::collections::BTreeMap::<String, u64>::new();
        let mut parser_gaps = std::collections::BTreeMap::<String, u64>::new();
        let mut truncated = 0u64;
        // Count each immutable extraction once, including reused and partial results.
        let mut stmt = store.conn.prepare("SELECT e.status,e.error_code,e.manifest_json FROM extractions e WHERE EXISTS(SELECT 1 FROM version_extractions ve JOIN versions v ON v.id=ve.version_id JOIN collections c ON c.id=v.collection_id WHERE ve.extraction_id=e.id AND v.pruned_at IS NULL AND (?1 IS NULL OR substr(datetime(c.started_at,'+8 hours'),1,10)>=?1) AND (?2 IS NULL OR substr(datetime(c.started_at,'+8 hours'),1,10)<=?2) AND (?3 IS NULL OR c.id=?3))")?;
        for row in stmt.query_map(params![from, to, run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })? {
            let (status, error, manifest) = row?;
            *statuses.entry(status).or_default() += 1;
            if let Some(error) = error {
                *parser_gaps.entry(error).or_default() += 1;
            }
            if let Some(manifest) = manifest {
                let manifest: Value = serde_json::from_str(&manifest)?;
                truncated += u64::from(manifest["truncated"] == true);
                for code in manifest["gaps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    *parser_gaps.entry(code.to_owned()).or_default() += 1;
                }
            }
        }
        parse_coverage = json!({"counting_unit":"unique_extraction","statuses":statuses,"gap_counts":parser_gaps,"truncated_extractions":truncated});
        let mut stmt = store
            .conn
            .prepare(&(FILE_QUERY.to_owned() + " ORDER BY f.collection_id,f.path LIMIT 5000"))?;
        for row in stmt.query_map(params![from, to, run_id], file_row)? {
            files.push(row?);
        }
        let mut stmt = store.conn.prepare(&(FILE_QUERY.to_owned() + " ORDER BY coalesce(f.capture_ms,0)+coalesce(f.parse_ms,0)+coalesce(f.diff_ms,0) DESC LIMIT 20"))?;
        for row in stmt.query_map(params![from, to, run_id], file_row)? {
            slow.push(row?);
        }
        let totals = store.conn.query_row(&format!("SELECT count(*),sum(capture_ms),sum(parse_ms),sum(diff_ms),sum(queue_ms) FROM ({FILE_QUERY})"), params![from,to,run_id], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,Option<i64>>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,Option<i64>>(4)?)))?;
        file_count = totals.0;
        summed = json!({"capture_ms":totals.1,"parse_ms":totals.2,"diff_ms":totals.3,"queue_ms":totals.4});
        let mut stmt=store.conn.prepare("SELECT g.collection_id,g.path,g.code,g.detail,g.resolved_at FROM gaps g JOIN collections c ON c.id=g.collection_id WHERE (?1 IS NULL OR substr(datetime(c.started_at,'+8 hours'),1,10)>=?1) AND (?2 IS NULL OR substr(datetime(c.started_at,'+8 hours'),1,10)<=?2) AND (?3 IS NULL OR c.id=?3) ORDER BY g.id")?;
        for row in stmt.query_map(params![from,to,run_id],|r|Ok(json!({"run_id":r.get::<_,String>(0)?,"path":r.get::<_,Option<String>>(1)?,"code":r.get::<_,String>(2)?,"detail":r.get::<_,String>(3)?,"resolved_at":r.get::<_,Option<String>>(4)?})))?{gaps.push(row?);}
    }
    let model = model_statistics(config, from, to, run_id)?;
    let total_work = |field: &str| -> Option<u64> {
        if collections.is_empty() {
            return None;
        }
        collections.iter().try_fold(0u64, |sum, row| {
            row["metrics"][field]
                .as_u64()
                .map(|value| sum.saturating_add(value))
        })
    };
    let work = json!({"capture_ms":total_work("capture_work_ms"),"parse_ms":total_work("parse_work_ms"),"queue_ms":total_work("queue_work_ms"),"diff_ms":total_work("diff_ms")});
    Ok(
        json!({"version":1,"report_stages":report_stages,"collections":collections,"storage":storage,"model":model,"files":files,"file_rows_total":file_count,"file_rows_limit":5000,"slow_files":slow,"coverage_gaps":gaps,"parse_coverage":parse_coverage,"summed_parallel_work_ms":work,"summed_file_work_ms":summed,"new_model_calls":0,"timing_note":"实际解析任务耗时只计一次；文件耗时可能共享同一解析。并行任务之和与 collections 中实际完成耗时分开；null 表示历史未记录。"}),
    )
}

pub fn write_csv(
    config: &AppConfig,
    from: Option<&str>,
    to: Option<&str>,
    run_id: Option<&str>,
    path: &std::path::Path,
) -> Result<(), ReportError> {
    use std::io::Write;
    let fields = [
        "run_id",
        "path",
        "version_id",
        "extension",
        "size_bytes",
        "capture_ms",
        "bytes_read",
        "parse_ms",
        "diff_ms",
        "queue_ms",
        "parser_backend",
        "worker_lane",
        "status",
    ];
    let mut output = std::io::BufWriter::new(
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?,
    );
    writeln!(output, "{}", fields.join(","))?;
    let store = ArchiveStore::open(config, false)?;
    let mut statement = store
        .conn
        .prepare(&(FILE_QUERY.to_owned() + " ORDER BY f.collection_id,f.path"))?;
    for row in statement.query_map(params![from, to, run_id], file_row)? {
        let row = row?;
        let cells: Vec<_> = fields
            .iter()
            .map(|key| {
                let field = &row[*key];
                let text = if field.is_null() {
                    "unknown".into()
                } else {
                    field
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| field.to_string())
                };
                format!("\"{}\"", text.replace('"', "\"\""))
            })
            .collect();
        writeln!(output, "{}", cells.join(","))?;
    }
    output.flush()?;
    Ok(())
}

/// Append attempts separately so recovery does not rewrite earlier timings.
pub fn record_stage(
    config: &AppConfig,
    run: &str,
    date: &str,
    stage: &str,
    wall_ms: u64,
) -> Result<(), ReportError> {
    let store = ArchiveStore::open(config, true)?;
    let key = format!("stage:{run}:{stage}:{}", crate::types::new_run_id());
    store.conn.execute("INSERT INTO metadata(key,value) VALUES(?,?)", params![key, json!({"run_id":run,"date":date,"target_date":date,"stage":stage,"wall_ms":wall_ms,"recorded_at":chrono::Utc::now().to_rfc3339()}).to_string()])?;
    Ok(())
}

const FILE_QUERY: &str = "SELECT f.collection_id AS run_id,f.path,f.version_id,v.extension,v.size_bytes,f.capture_ms,f.bytes_read,f.parse_ms,f.diff_ms,f.queue_ms,f.backend,f.lane,f.status FROM file_metrics f JOIN collections c ON c.id=f.collection_id LEFT JOIN versions v ON v.id=f.version_id WHERE (?1 IS NULL OR substr(datetime(c.started_at,'+8 hours'),1,10)>=?1) AND (?2 IS NULL OR substr(datetime(c.started_at,'+8 hours'),1,10)<=?2) AND (?3 IS NULL OR c.id=?3)";
fn file_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"run_id":r.get::<_,String>(0)?,"path":r.get::<_,String>(1)?,"version_id":r.get::<_,Option<String>>(2)?,"extension":r.get::<_,Option<String>>(3)?,"size_bytes":r.get::<_,Option<i64>>(4)?,"capture_ms":r.get::<_,Option<i64>>(5)?,"bytes_read":r.get::<_,i64>(6)?,"parse_ms":r.get::<_,Option<i64>>(7)?,"diff_ms":r.get::<_,Option<i64>>(8)?,"queue_ms":r.get::<_,Option<i64>>(9)?,"parser_backend":r.get::<_,Option<String>>(10)?,"worker_lane":r.get::<_,Option<String>>(11)?,"status":r.get::<_,String>(12)?}),
    )
}

fn model_statistics(
    config: &AppConfig,
    from: Option<&str>,
    to: Option<&str>,
    run_id: Option<&str>,
) -> Result<Value, ReportError> {
    let db = config.paths.db_dir.join("reports.sqlite3");
    if !db.is_file() {
        return Ok(Value::Null);
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt=conn.prepare("SELECT * FROM report_model_calls WHERE (?1 IS NULL OR call_date>=?1) AND (?2 IS NULL OR call_date<=?2) AND (?3 IS NULL OR run_id=?3) ORDER BY call_id")?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut calls = stmt
        .query_map(params![from, to, run_id], |row| model_call_row(row, &names))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stages = std::collections::BTreeMap::<String, u64>::new();
    let mut tokens = 0u64;
    let mut unknown = 0;
    let mut retries = 0;
    for row in &mut calls {
        let usage: Value = serde_json::from_str(row["usage_json"].as_str().unwrap_or("{}"))?;
        let mut safe = serde_json::Map::new();
        for key in [
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "prompt_cache_hit_tokens",
            "prompt_cache_miss_tokens",
        ] {
            if let Some(value) = usage.get(key) {
                safe.insert(key.into(), value.clone());
            }
        }
        row["usage"] = Value::Object(safe);
        row.as_object_mut().unwrap().remove("usage_json");
        if let Some(total) = usage["total_tokens"].as_u64() {
            tokens += total;
        } else {
            unknown += 1;
        }
        if row["attempt"].as_u64().unwrap_or(0) > 1 {
            retries += 1;
        }
        let stage = if row["logical_step"]
            .as_str()
            .unwrap_or("")
            .starts_with("compose")
        {
            "composition"
        } else {
            "fact_extraction"
        };
        if let Some(duration) = row["duration_ms"].as_u64() {
            *stages.entry(stage.into()).or_default() += duration;
        }
    }
    Ok(
        json!({"calls":calls,"summed_parallel_work_ms":stages,"known_total_tokens":tokens,"calls_with_unknown_tokens":unknown,"retries":retries,"wall_ms":null}),
    )
}

fn model_call_row(row: &rusqlite::Row<'_>, names: &[String]) -> rusqlite::Result<Value> {
    let mut value = serde_json::Map::new();
    for (i, name) in names.iter().enumerate() {
        use rusqlite::types::ValueRef;
        let field = match row.get_ref(i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => json!(n),
            ValueRef::Real(n) => json!(n),
            ValueRef::Text(s) => json!(String::from_utf8_lossy(s)),
            ValueRef::Blob(_) => Value::Null,
        };
        // Statistics expose usage and timing, never prompts or raw responses.
        if [
            "run_id",
            "call_id",
            "call_date",
            "logical_step",
            "attempt",
            "status",
            "duration_ms",
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "usage_json",
            "error_code",
        ]
        .contains(&name.as_str())
        {
            value.insert(name.clone(), field);
        }
    }
    Ok(Value::Object(value))
}
