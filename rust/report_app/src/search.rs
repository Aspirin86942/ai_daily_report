mod attachment;
mod revision;
mod terms;

use crate::{
    archive::cli::Format,
    cli::{SearchArgs, SearchKind},
    config::AppConfig,
    ReportError,
};
use rusqlite::{
    params, params_from_iter, Connection, OpenFlags, OptionalExtension, TransactionBehavior,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

const MAX_FRAGMENT_CHARS: usize = 4000;
const INDEX_VERSION: &str = "5";
const WEEK_FIELDS: &[(&str, &str)] = &[
    ("completed_work", "完成工作"),
    ("self_growth", "自我成长"),
    ("improvement_actions", "改进行动"),
    ("work_summary", "工作总结"),
    ("next_plan", "下步计划"),
    ("support_needed", "所需支持"),
    ("other_notes", "其他事项"),
];
const MONTH_FIELDS: &[(&str, &str)] = &[
    ("overview", "概览"),
    ("completed_work", "完成工作"),
    ("work_summary", "工作总结"),
    ("next_plan", "下步计划"),
];

struct ReportTable {
    name: &'static str,
    key: &'static str,
    kind: &'static str,
}

const TABLES: &[ReportTable] = &[
    ReportTable {
        name: "daily_reports",
        key: "date",
        kind: "daily",
    },
    ReportTable {
        name: "weekly_reports",
        key: "week_label",
        kind: "weekly",
    },
    ReportTable {
        name: "monthly_reports",
        key: "year_month",
        kind: "monthly",
    },
];

struct SourceState {
    tables: Vec<&'static ReportTable>,
    metadata: BTreeMap<String, String>,
    count: i64,
}

struct Fragment {
    title: String,
    body: String,
    sources: String,
    location: Option<Value>,
    literals: String,
}

fn index_error(error: impl std::fmt::Display) -> ReportError {
    ReportError::new("SEARCH_INDEX_ERROR", format!("检索索引操作失败：{error}"))
}

fn source_error(error: impl std::fmt::Display) -> ReportError {
    ReportError::new("SEARCH_DB_ERROR", format!("只读报告库读取失败：{error}"))
}

fn open_reports(path: &Path) -> Result<Connection, ReportError> {
    let reports = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(source_error)?;
    reports
        .busy_timeout(Duration::from_secs(5))
        .map_err(source_error)?;
    Ok(reports)
}

fn cached_revision(index: &Connection) -> rusqlite::Result<Option<String>> {
    let initialized: bool = index.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='index_meta' AND type='table')",
        [],
        |row| row.get(0),
    )?;
    if !initialized {
        return Ok(None);
    }
    index
        .query_row(
            "SELECT r.value FROM index_meta r, index_meta s
         WHERE r.key='source_revision' AND s.key='schema_version' AND s.value=?",
            [INDEX_VERSION],
            |row| row.get(0),
        )
        .optional()
}

pub fn execute(config: &AppConfig, args: &SearchArgs) -> Result<Value, ReportError> {
    let input = args.query.as_deref().unwrap_or("");
    let query = if args.phrase { input } else { input.trim() };
    if !args.reindex && query.trim().is_empty() {
        return Err(ReportError::new(
            "SEARCH_QUERY_EMPTY",
            "请输入非空检索关键词",
        ));
    }
    let report_path = config.paths.db_dir.join("reports.sqlite3");
    if !report_path.exists() {
        return Err(ReportError::new(
            "SEARCH_DB_MISSING",
            "报告库不存在，请先生成报告或检查配置中的数据库目录",
        ));
    }
    let mut index =
        Connection::open(config.paths.db_dir.join("report_search.sqlite3")).map_err(index_error)?;
    index
        .busy_timeout(Duration::from_secs(5))
        .map_err(index_error)?;
    if !args.reindex {
        let reader = index.transaction().map_err(index_error)?;
        if let Some(cached) = cached_revision(&reader).map_err(index_error)? {
            let mut reports = open_reports(&report_path)?;
            let snapshot = reports.transaction().map_err(source_error)?;
            if revision::read(&snapshot).as_ref() == Some(&cached) {
                drop(snapshot);
                return search(&reader, args, query, false, 0);
            }
        }
    }
    let transaction = index
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(index_error)?;
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)",
        )
        .map_err(index_error)?;
    let metadata: BTreeMap<String, String> = transaction
        .prepare("SELECT key, value FROM index_meta")
        .map_err(index_error)?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(index_error)?
        .collect::<Result<_, _>>()
        .map_err(index_error)?;
    let upgrade = metadata.get("schema_version").map(String::as_str) != Some(INDEX_VERSION);
    if upgrade {
        initialize(&transaction)?;
    }
    // Serialize index writers before opening the source. All source reads, including the
    // revision, then see the same committed snapshot (also in WAL mode).
    let mut reports = open_reports(&report_path)?;
    let snapshot = reports.transaction().map_err(source_error)?;
    let revision = revision::read(&snapshot);
    let force = args.reindex || upgrade;
    let summary = if !force
        && revision
            .as_ref()
            .is_some_and(|v| metadata.get("source_revision") == Some(v))
    {
        json!({"index_rebuilt": false, "source_rows_read": 0})
    } else {
        let state = source_state(&snapshot)?;
        let metadata_changed = state
            .metadata
            .iter()
            .any(|(k, v)| metadata.get(k) != Some(v));
        let summary = refresh(&snapshot, &transaction, &state, force, metadata_changed)?;
        if revision.as_ref() != metadata.get("source_revision") {
            transaction
                .execute("DELETE FROM index_meta WHERE key='source_revision'", [])
                .map_err(index_error)?;
            if let Some(revision) = revision {
                transaction
                    .execute(
                        "INSERT INTO index_meta(key,value) VALUES ('source_revision',?)",
                        [revision],
                    )
                    .map_err(index_error)?;
            }
        }
        summary
    };
    drop(snapshot);
    let result = if args.reindex {
        summary
    } else {
        search(
            &transaction,
            args,
            query,
            summary["index_rebuilt"] == true,
            summary["source_rows_read"].as_u64().unwrap_or_default(),
        )?
    };
    transaction.commit().map_err(index_error)?;
    Ok(result)
}

fn initialize(index: &Connection) -> Result<(), ReportError> {
    let options: Vec<String> = index
        .prepare("PRAGMA compile_options")
        .map_err(index_error)?
        .query_map([], |row| row.get(0))
        .map_err(index_error)?
        .collect::<Result<_, _>>()
        .map_err(index_error)?;
    if !options.iter().any(|option| option == "ENABLE_FTS5") {
        return Err(ReportError::new(
            "SEARCH_FTS5_UNAVAILABLE",
            "当前 SQLite 未启用 FTS5，无法建立历史报告检索索引",
        ));
    }
    index
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS indexed_reports(
                 kind TEXT NOT NULL, report_key TEXT NOT NULL, raw_sha256 TEXT NOT NULL,
                 fragments INTEGER NOT NULL, PRIMARY KEY(kind,report_key)
             ) WITHOUT ROWID;
             DROP TABLE IF EXISTS report_phrases;
             DROP TABLE IF EXISTS report_search;
             CREATE VIRTUAL TABLE report_phrases USING fts5(literals,tokenize='trigram');
             CREATE VIRTUAL TABLE report_search USING fts5(
                 report_key UNINDEXED,
                 kind UNINDEXED,
                 report_label UNINDEXED,
                 report_date UNINDEXED,
                 title,
                 body,
                 sources UNINDEXED,
                 location UNINDEXED,
                 title_terms,
                 body_terms,
                 tokenize='trigram'
             );",
        )
        .map_err(|error| index_error(format!("建立 FTS5 trigram 索引失败：{error}")))
}

fn source_state(reports: &Connection) -> Result<SourceState, ReportError> {
    let mut tables = Vec::new();
    let mut signature = Vec::new();
    let mut count = 0;
    let mut latest: Option<String> = None;
    for table in TABLES {
        let columns: BTreeSet<String> = reports
            .prepare(&format!("PRAGMA table_info({})", table.name))
            .map_err(source_error)?
            .query_map([], |row| row.get(1))
            .map_err(source_error)?
            .collect::<Result<_, _>>()
            .map_err(source_error)?;
        if !columns.contains(table.key) || !columns.contains("raw_json") {
            continue;
        }
        let max_expression = if columns.contains("updated_at") {
            "CAST(MAX(updated_at) AS TEXT)"
        } else {
            "NULL"
        };
        let (rows, updated_at): (i64, Option<String>) = reports
            .query_row(
                &format!("SELECT COUNT(*), {max_expression} FROM {}", table.name),
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(source_error)?;
        count += rows;
        latest = latest.max(updated_at.clone());
        signature.push((table.name, rows, updated_at));
        tables.push(table);
    }
    Ok(SourceState {
        tables,
        count,
        metadata: BTreeMap::from([
            ("schema_version".into(), INDEX_VERSION.into()),
            ("source_count".into(), count.to_string()),
            ("source_max_updated_at".into(), latest.unwrap_or_default()),
            ("source_tables".into(), json!(signature).to_string()),
        ]),
    })
}

fn refresh(
    reports: &Connection,
    index: &Connection,
    state: &SourceState,
    force: bool,
    metadata_changed: bool,
) -> Result<Value, ReportError> {
    if force {
        index
            .execute_batch("DELETE FROM report_phrases; DELETE FROM report_search; DELETE FROM indexed_reports;")
            .map_err(index_error)?;
    }
    let mut previous: BTreeMap<(String, String), String> = index
        .prepare("SELECT kind, report_key, raw_sha256 FROM indexed_reports")
        .map_err(index_error)?
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))
        .map_err(index_error)?
        .collect::<Result<_, _>>()
        .map_err(index_error)?;
    let mut changed = force || metadata_changed;
    let mut insert = index
        .prepare(
            "INSERT INTO report_search(report_key,kind,report_label,report_date,title,body,sources,location,title_terms,body_terms)
             VALUES (?1,?2,?1,?3,?4,?5,?6,?7,?8,?9)",
        )
        .map_err(index_error)?;
    let mut source_rows_read = 0_u64;
    let mut insert_phrases = index
        .prepare("INSERT INTO report_phrases(rowid,literals) VALUES(last_insert_rowid(),?)")
        .map_err(index_error)?;
    for table in &state.tables {
        let mut select = reports
            .prepare(&format!(
                "SELECT {}, raw_json FROM {} ORDER BY {}",
                table.key, table.name, table.key
            ))
            .map_err(source_error)?;
        let rows = select
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map_err(source_error)?;
        for row in rows {
            let (key, raw) = row.map_err(source_error)?;
            source_rows_read += 1;
            let raw = raw.as_deref().unwrap_or("");
            let sha = crate::snapshots::digest(raw.as_bytes());
            let old = previous.remove(&(table.kind.to_owned(), key.clone()));
            if old.as_ref() == Some(&sha) {
                continue;
            }
            changed = true;
            if old.is_some() {
                remove_fragments(index, table.kind, &key)?;
            }
            let fragments = serde_json::from_str(raw)
                .map(|report| fragments(&report, table.kind, &key))
                .unwrap_or_default();
            index.execute(
                "INSERT INTO indexed_reports(kind,report_key,raw_sha256,fragments) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(kind,report_key) DO UPDATE SET raw_sha256=excluded.raw_sha256,fragments=excluded.fragments",
                params![table.kind,key,sha,fragments.len() as i64],
            ).map_err(index_error)?;
            let date = (table.kind == "daily").then_some(key.as_str());
            for fragment in fragments {
                insert
                    .execute(params![
                        key,
                        table.kind,
                        date,
                        fragment.title,
                        fragment.body,
                        fragment.sources,
                        fragment.location.map(|v| v.to_string()),
                        terms::encode(&fragment.title),
                        terms::encode(&fragment.body)
                    ])
                    .map_err(index_error)?;
                insert_phrases
                    .execute([fragment.literals])
                    .map_err(index_error)?;
            }
        }
    }
    for (kind, key) in previous.into_keys() {
        changed = true;
        remove_fragments(index, &kind, &key)?;
        index
            .execute(
                "DELETE FROM indexed_reports WHERE kind=?1 AND report_key=?2",
                params![kind, key],
            )
            .map_err(index_error)?;
    }
    let (indexed_fragments,skipped_reports):(i64,i64) = index.query_row(
        "SELECT coalesce(sum(fragments),0),count(*) FILTER (WHERE fragments=0) FROM indexed_reports",
        [], |r| Ok((r.get(0)?,r.get(1)?)),
    ).map_err(index_error)?;
    if changed {
        let mut metadata = state.metadata.clone();
        metadata.insert("indexed_fragments".into(), indexed_fragments.to_string());
        metadata.insert("skipped_reports".into(), skipped_reports.to_string());
        for (key, value) in metadata {
            index.execute(
                "INSERT INTO index_meta(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value],
            ).map_err(index_error)?;
        }
    }
    Ok(json!({
        "index_rebuilt": changed,
        "source_rows_read": source_rows_read,
        "source_count": state.count,
        "indexed_fragments": indexed_fragments,
        "skipped_reports": skipped_reports,
    }))
}

fn remove_fragments(index: &Connection, kind: &str, key: &str) -> Result<(), ReportError> {
    index
        .execute(
            "DELETE FROM report_phrases WHERE rowid IN
         (SELECT rowid FROM report_search WHERE kind=?1 AND report_key=?2)",
            params![kind, key],
        )
        .map_err(index_error)?;
    index
        .execute(
            "DELETE FROM report_search WHERE kind=?1 AND report_key=?2",
            params![kind, key],
        )
        .map_err(index_error)?;
    Ok(())
}

fn text_parts<'a>(value: &'a Value, parts: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => parts.push(text),
        Value::Array(items) => {
            for item in items {
                text_parts(item, parts);
            }
        }
        Value::Object(object) => {
            for field in [
                "title",
                "text",
                "background",
                "details",
                "detail",
                "status",
                "message",
            ] {
                if let Some(text) = object.get(field) {
                    text_parts(text, parts);
                }
            }
        }
        _ => {}
    }
}

fn bounded(text: &str) -> String {
    const MARKER: &str = "…[已截断]";
    if text.chars().count() <= MAX_FRAGMENT_CHARS {
        text.to_owned()
    } else {
        let prefix: String = text
            .chars()
            .take(MAX_FRAGMENT_CHARS - MARKER.chars().count())
            .collect();
        format!("{prefix}{MARKER}")
    }
}

fn source_ids(value: &Value, ids: &mut BTreeSet<String>) {
    match value {
        Value::Array(items) => {
            for item in items {
                source_ids(item, ids);
            }
        }
        Value::Object(object) => {
            if let Some(items) = object.get("source_ids").and_then(Value::as_array) {
                ids.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            for (key, item) in object {
                if key != "source_ids" {
                    source_ids(item, ids);
                }
            }
        }
        _ => {}
    }
}

fn add_fragment(fragments: &mut Vec<Fragment>, report: &Value, item: &Value, title: &str) {
    let mut parts = Vec::new();
    text_parts(item, &mut parts);
    let mut body = parts
        .iter()
        .flat_map(|p| p.split_whitespace())
        .collect::<Vec<_>>()
        .join(" ");
    if body.is_empty() {
        return;
    }
    let mut ids = BTreeSet::new();
    source_ids(item, &mut ids);
    let mut labels = Vec::new();
    let mut hints = Vec::new();
    // Keep the display and ordinary BM25 corpus unchanged. A phrase is sourced
    // from one original string, never the spaces used to join fields for display.
    let mut literals = terms::encode(title);
    for part in parts {
        terms::append_literal(&mut literals, part);
    }
    for (position, source) in report["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if !ids.is_empty()
            && !source["source_id"]
                .as_str()
                .or_else(|| source.as_str())
                .is_some_and(|id| ids.contains(id))
        {
            continue;
        }
        let label = format!("S{:03}", position + 1);
        let path = source["relative_path"].as_str().unwrap_or("");
        terms::append_literal(&mut literals, &label);
        terms::append_literal(&mut literals, path);
        hints.push(format!("{label} {path}"));
        labels.push(label);
    }
    if !hints.is_empty() {
        body.push_str(" 来源：");
        body.push_str(&hints.join(" "));
    }
    let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    fragments.push(Fragment {
        title: bounded(&title.split_whitespace().collect::<Vec<_>>().join(" ")),
        // Detailed reports can carry long, source-validated items. Their tail is searchable too.
        body: if report["schema_version"] == 6 {
            body
        } else {
            bounded(&body)
        },
        sources: labels.join(" "),
        location: None,
        literals,
    });
}

fn fragments(report: &Value, kind: &str, key: &str) -> Vec<Fragment> {
    let mut result = Vec::new();
    if kind == "daily" {
        if matches!(report["schema_version"].as_u64(), Some(5 | 6)) {
            let Ok(personal) =
                serde_json::from_value::<crate::reports::personal::PersonalReport>(report.clone())
            else {
                return result;
            };
            if personal.validate().is_err() {
                return result;
            }
            for item in report["items"].as_array().into_iter().flatten() {
                add_fragment(
                    &mut result,
                    report,
                    item,
                    item["title"].as_str().unwrap_or("事项"),
                );
            }
            if result.is_empty() {
                add_fragment(&mut result, report, &report["user_notes"], "用户补充");
                add_fragment(&mut result, report, &report["gaps"], "材料范围");
            }
            if personal.date == key {
                attachment::append(&personal, &mut result);
            }
            return result;
        }
        let content = report.get("content").filter(|value| value.is_object());
        let body = content.unwrap_or(report);
        for (field, title) in [
            ("overview", "概览"),
            ("matters", "事项"),
            ("pending", "待确认"),
            ("summary_items", "概览"),
            ("verification_items", "待确认"),
        ] {
            if let Some(items) = body[field].as_array() {
                for item in items {
                    let title = item["title"].as_str().unwrap_or(title);
                    add_fragment(&mut result, report, item, title);
                }
            }
        }
        if content.is_none() {
            for (field, title) in [
                ("completed_work", "完成工作"),
                ("work_summary", "工作总结"),
                ("next_plan", "下步计划"),
            ] {
                add_fragment(&mut result, report, &report[field], title);
            }
        }
    } else {
        let fields = if kind == "weekly" {
            WEEK_FIELDS
        } else {
            MONTH_FIELDS
        };
        for (field, title) in fields {
            add_fragment(&mut result, report, &report[field], title);
        }
    }
    result
}

fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn search(
    index: &Connection,
    args: &SearchArgs,
    query: &str,
    index_rebuilt: bool,
    source_rows_read: u64,
) -> Result<Value, ReportError> {
    let short = !args.phrase && query.chars().nth(2).is_none();
    let original =
        !args.phrase && !short && !query.contains(char::is_whitespace) && !query.contains('\0');
    let (table, from, ranking) = if args.phrase {
        (
            "report_phrases",
            "report_phrases JOIN report_search ON report_search.rowid=report_phrases.rowid",
            "bm25(report_phrases)",
        )
    } else {
        (
            "report_search",
            "report_search",
            "bm25(report_search,0,0,0,0,4,1,0,0,4,1)",
        )
    };
    let mut parameters: Vec<rusqlite::types::Value> =
        vec![terms::expression(query, original, args.phrase).into()];
    let mut filter = format!("{table} MATCH ?1");
    let mut add_filter = |expression: &str, value: String| {
        parameters.push(value.into());
        filter.push_str(&format!(" AND {expression} ?{}", parameters.len()));
    };
    let kind = match args.kind {
        SearchKind::All => None,
        SearchKind::Daily => Some("daily"),
        SearchKind::Weekly => Some("weekly"),
        SearchKind::Monthly => Some("monthly"),
    };
    if let Some(kind) = kind {
        add_filter("kind =", kind.into());
    }
    if args.from.is_some() || args.to.is_some() {
        add_filter("kind =", "daily".into());
    }
    if let Some(from) = args.from {
        add_filter("report_date >=", from.to_string());
    }
    if let Some(to) = args.to {
        add_filter("report_date <=", to.to_string());
    }
    if let Some(source) = &args.source {
        add_filter(
            "(' ' || sources || ' ') LIKE",
            format!("% {} %", escape_like(&source.trim().to_ascii_uppercase())),
        );
        filter.push_str(" ESCAPE '\\'");
    }
    let total: i64 = index
        .query_row(
            &format!("SELECT COUNT(*) FROM {from} WHERE {filter}"),
            params_from_iter(parameters.iter()),
            |row| row.get(0),
        )
        .map_err(index_error)?;
    let snippet_tokens = if original { 40 } else { 64 };
    let native = format!("snippet({table},-1,'[',']','…',{snippet_tokens})");
    let excerpt = if short {
        parameters.push(query.to_owned().into());
        let n = parameters.len();
        // Ordinary short hits already have byte offsets in the original text.
        // Only folded matches need FTS to tokenize the attachment again for a snippet.
        format!("CASE WHEN instr(body,?{n}) OR instr(title,?{n}) THEN NULL ELSE {native} END")
    } else {
        native
    };
    parameters.push(args.limit.clamp(1, 500).into());
    let body_column = if short { "body" } else { "NULL" };
    let mut statement = index
        .prepare(&format!(
            "SELECT kind,report_label,report_date,title,{excerpt},sources,location,{body_column}
             FROM {from} WHERE {filter}
             ORDER BY {ranking}, report_date IS NULL, report_date DESC, report_key DESC, report_search.rowid
             LIMIT ?{}",
            parameters.len()
        ))
        .map_err(index_error)?;
    let results: Vec<Value> = statement
        .query_map(params_from_iter(parameters.iter()), |row| {
            let title: String = row.get(3)?;
            let native: Option<String> = row.get(4)?;
            // Keep the historical 40-character context for a single short literal.
            let snippet = if short {
                like_snippet(row.get_ref(7)?.as_str()?, query)
                    .or_else(|| like_snippet(&title, query))
            } else {
                None
            }
            .unwrap_or_else(|| {
                let native = native.unwrap_or_default();
                if original {
                    native
                } else {
                    terms::decode(&native)
                }
            });
            let sources: String = row.get(5)?;
            let mut hit = json!({
                "kind": row.get::<_, String>(0)?,
                "report_label": row.get::<_, String>(1)?,
                "report_date": row.get::<_, Option<String>>(2)?,
                "title": title,
                "snippet": snippet,
                "sources": sources.split_whitespace().collect::<Vec<_>>(),
            });
            if let Some(location) = row.get::<_, Option<String>>(6)? {
                hit["location"] = serde_json::from_str(&location).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
            }
            Ok(hit)
        })
        .map_err(index_error)?
        .collect::<Result<_, _>>()
        .map_err(index_error)?;
    Ok(
        json!({"query": query, "total": total, "index_rebuilt": index_rebuilt, "source_rows_read": source_rows_read, "results": results}),
    )
}

fn like_snippet(text: &str, query: &str) -> Option<String> {
    let start = if query.bytes().any(|b| b.is_ascii_alphabetic()) {
        text.as_bytes()
            .windows(query.len())
            .position(|part| part.eq_ignore_ascii_case(query.as_bytes()))
    } else {
        text.find(query)
    }?;
    let end = start + query.len();
    let left = text[..start]
        .char_indices()
        .rev()
        .nth(39)
        .map_or(0, |(i, _)| i);
    let right = text[end..]
        .char_indices()
        .nth(40)
        .map_or(text.len(), |(i, _)| end + i);
    Some(format!(
        "{}{}[{}]{}{}",
        if left > 0 { "…" } else { "" },
        &text[left..start],
        &text[start..end],
        &text[end..right],
        if right < text.len() { "…" } else { "" },
    ))
}

pub fn render(value: &Value, format: Format) -> String {
    if matches!(format, Format::Json) {
        return serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    }
    if value.get("results").is_none() {
        return format!(
            "索引重建完成：读取 {} 份报告，建立 {} 个片段，跳过 {} 份报告。",
            value["source_count"], value["indexed_fragments"], value["skipped_reports"]
        );
    }
    let mut output = Vec::new();
    if let Some(results) = value["results"].as_array() {
        for result in results {
            let kind = match result["kind"].as_str() {
                Some("daily") => "日报",
                Some("weekly") => "周报",
                Some("monthly") => "月报",
                _ => "报告",
            };
            let sources = result["sources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            output.push(format!(
                "{} · {kind} · {}\n  {}\n  来源：{}",
                result["report_label"].as_str().unwrap_or(""),
                result["title"].as_str().unwrap_or(""),
                result["snippet"].as_str().unwrap_or(""),
                if sources.is_empty() { "无" } else { &sources },
            ));
            if let Some(location) = result.get("location") {
                let line = output.last_mut().unwrap();
                line.push_str(&format!(
                    "\n  附件：{}（第 {}–{} 行）{}",
                    location["link"].as_str().unwrap_or(""),
                    location["line_start"],
                    location["line_end"],
                    location["range"]
                        .as_str()
                        .map(|r| format!(" · {r}"))
                        .unwrap_or_default()
                ));
                if let Some(original) = location["original_link"].as_str() {
                    line.push_str(&format!("\n  原文：{original}"));
                }
            }
        }
    }
    if output.is_empty() {
        "未找到匹配的报告片段。".into()
    } else {
        output.join("\n\n")
    }
}
