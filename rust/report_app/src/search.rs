use crate::{
    archive::cli::Format,
    cli::{SearchArgs, SearchKind},
    config::AppConfig,
    ReportError,
};
use rusqlite::{params, params_from_iter, Connection, OpenFlags, TransactionBehavior};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

const MAX_FRAGMENT_CHARS: usize = 4000;
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
}

fn index_error(error: impl std::fmt::Display) -> ReportError {
    ReportError::new("SEARCH_INDEX_ERROR", format!("检索索引操作失败：{error}"))
}

fn source_error(error: impl std::fmt::Display) -> ReportError {
    ReportError::new("SEARCH_DB_ERROR", format!("只读报告库读取失败：{error}"))
}

pub fn execute(config: &AppConfig, args: &SearchArgs) -> Result<Value, ReportError> {
    let query = args.query.as_deref().unwrap_or("").trim();
    if !args.reindex && query.is_empty() {
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
    let mut reports = Connection::open_with_flags(report_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(source_error)?;
    reports
        .busy_timeout(Duration::from_secs(5))
        .map_err(source_error)?;
    let mut index =
        Connection::open(config.paths.db_dir.join("report_search.sqlite3")).map_err(index_error)?;
    index
        .busy_timeout(Duration::from_secs(5))
        .map_err(index_error)?;
    initialize(&index)?;
    let transaction = index
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(index_error)?;
    let snapshot = reports.transaction().map_err(source_error)?;
    let state = source_state(&snapshot)?;
    let metadata: BTreeMap<String, String> = transaction
        .prepare("SELECT key, value FROM index_meta")
        .map_err(index_error)?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(index_error)?
        .collect::<Result<_, _>>()
        .map_err(index_error)?;
    let index_rebuilt = args.reindex
        || state
            .metadata
            .iter()
            .any(|(key, value)| metadata.get(key) != Some(value));
    let summary = if index_rebuilt {
        rebuild(&snapshot, &transaction, &state)?
    } else {
        Value::Null
    };
    let result = if args.reindex {
        summary
    } else {
        search(&transaction, args, query, index_rebuilt)?
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
            "CREATE TABLE IF NOT EXISTS index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE VIRTUAL TABLE IF NOT EXISTS report_search USING fts5(
                 report_key UNINDEXED,
                 kind UNINDEXED,
                 report_label UNINDEXED,
                 report_date UNINDEXED,
                 title,
                 body,
                 sources UNINDEXED,
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
            "MAX(updated_at)"
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
            ("schema_version".into(), "1".into()),
            ("source_count".into(), count.to_string()),
            ("source_max_updated_at".into(), latest.unwrap_or_default()),
            ("source_tables".into(), json!(signature).to_string()),
        ]),
    })
}

fn rebuild(
    reports: &Connection,
    index: &Connection,
    state: &SourceState,
) -> Result<Value, ReportError> {
    index
        .execute("DELETE FROM report_search", [])
        .map_err(index_error)?;
    let mut insert = index
        .prepare(
            "INSERT INTO report_search(report_key,kind,report_label,report_date,title,body,sources)
             VALUES (?1,?2,?1,?3,?4,?5,?6)",
        )
        .map_err(index_error)?;
    let mut indexed_fragments = 0;
    let mut skipped_reports = 0;
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
            let Ok(report) = serde_json::from_str::<Value>(raw.as_deref().unwrap_or("")) else {
                skipped_reports += 1;
                continue;
            };
            let fragments = fragments(&report, table.kind);
            if fragments.is_empty() {
                skipped_reports += 1;
                continue;
            }
            let date = (table.kind == "daily").then_some(key.as_str());
            for fragment in fragments {
                insert
                    .execute(params![
                        key,
                        table.kind,
                        date,
                        fragment.title,
                        fragment.body,
                        fragment.sources
                    ])
                    .map_err(index_error)?;
                indexed_fragments += 1;
            }
        }
    }
    index
        .execute("DELETE FROM index_meta", [])
        .map_err(index_error)?;
    let mut metadata = state.metadata.clone();
    metadata.insert("indexed_fragments".into(), indexed_fragments.to_string());
    metadata.insert("skipped_reports".into(), skipped_reports.to_string());
    for (key, value) in metadata {
        index
            .execute(
                "INSERT INTO index_meta(key,value) VALUES (?1,?2)",
                params![key, value],
            )
            .map_err(index_error)?;
    }
    Ok(json!({
        "index_rebuilt": true,
        "source_count": state.count,
        "indexed_fragments": indexed_fragments,
        "skipped_reports": skipped_reports,
    }))
}

fn text_parts(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::String(text) => parts.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                text_parts(item, parts);
            }
        }
        Value::Object(object) => {
            for field in ["title", "text", "background", "details", "detail", "status"] {
                if let Some(text) = object.get(field) {
                    text_parts(text, parts);
                }
            }
        }
        _ => {}
    }
}

fn plain_text(value: &Value) -> String {
    let mut parts = Vec::new();
    text_parts(value, &mut parts);
    parts
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
    let mut body = plain_text(item);
    if body.is_empty() {
        return;
    }
    let mut ids = BTreeSet::new();
    source_ids(item, &mut ids);
    let mut labels = Vec::new();
    let mut hints = Vec::new();
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
        hints.push(format!("{label} {path}"));
        labels.push(label);
    }
    if !hints.is_empty() {
        body.push_str(" 来源：");
        body.push_str(&hints.join(" "));
    }
    fragments.push(Fragment {
        title: bounded(&title.split_whitespace().collect::<Vec<_>>().join(" ")),
        body: bounded(&body.split_whitespace().collect::<Vec<_>>().join(" ")),
        sources: labels.join(" "),
    });
}

fn fragments(report: &Value, kind: &str) -> Vec<Fragment> {
    let mut result = Vec::new();
    if kind == "daily" {
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
) -> Result<Value, ReportError> {
    let fts = query.chars().count() >= 3;
    let mut parameters: Vec<rusqlite::types::Value> = vec![if fts {
        format!("\"{}\"", query.replace('"', "\"\"")).into()
    } else {
        format!("%{}%", escape_like(query)).into()
    }];
    let mut filter = if fts {
        "report_search MATCH ?1".to_owned()
    } else {
        "(title LIKE ?1 ESCAPE '\\' OR body LIKE ?1 ESCAPE '\\')".to_owned()
    };
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
            &format!("SELECT COUNT(*) FROM report_search WHERE {filter}"),
            params_from_iter(parameters.iter()),
            |row| row.get(0),
        )
        .map_err(index_error)?;
    let excerpt = if fts {
        "snippet(report_search, -1, '[', ']', '…', 40)"
    } else {
        "body"
    };
    parameters.push(args.limit.clamp(1, 500).into());
    let mut statement = index
        .prepare(&format!(
            "SELECT kind,report_label,report_date,title,{excerpt},sources
             FROM report_search WHERE {filter}
             ORDER BY report_date IS NULL, report_date DESC, report_key DESC, rowid
             LIMIT ?{}",
            parameters.len()
        ))
        .map_err(index_error)?;
    let results: Vec<Value> = statement
        .query_map(params_from_iter(parameters.iter()), |row| {
            let title: String = row.get(3)?;
            let body: String = row.get(4)?;
            let snippet = if fts {
                body
            } else {
                like_snippet(&body, query)
                    .or_else(|| like_snippet(&title, query))
                    .unwrap_or_default()
            };
            let sources: String = row.get(5)?;
            Ok(json!({
                "kind": row.get::<_, String>(0)?,
                "report_label": row.get::<_, String>(1)?,
                "report_date": row.get::<_, Option<String>>(2)?,
                "title": title,
                "snippet": snippet,
                "sources": sources.split_whitespace().collect::<Vec<_>>(),
            }))
        })
        .map_err(index_error)?
        .collect::<Result<_, _>>()
        .map_err(index_error)?;
    Ok(json!({"query": query, "total": total, "index_rebuilt": index_rebuilt, "results": results}))
}

fn like_snippet(text: &str, query: &str) -> Option<String> {
    let characters: Vec<char> = text.chars().collect();
    let needle: Vec<char> = query.chars().collect();
    let start = characters.windows(needle.len()).position(|window| {
        window
            .iter()
            .zip(&needle)
            .all(|(character, expected)| character.eq_ignore_ascii_case(expected))
    })?;
    let end = start + needle.len();
    let left = start.saturating_sub(40);
    let right = (end + 40).min(characters.len());
    Some(format!(
        "{}{}[{}]{}{}",
        if left > 0 { "…" } else { "" },
        characters[left..start].iter().collect::<String>(),
        characters[start..end].iter().collect::<String>(),
        characters[end..right].iter().collect::<String>(),
        if right < characters.len() { "…" } else { "" },
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
        }
    }
    if output.is_empty() {
        "未找到匹配的报告片段。".into()
    } else {
        output.join("\n\n")
    }
}
