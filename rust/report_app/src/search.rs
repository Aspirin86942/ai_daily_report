mod attachment;
mod query;
mod revision;
mod storage;
mod terms;

use crate::{
    archive::cli::Format,
    cli::{SearchArgs, SearchKind},
    config::AppConfig,
    ReportError,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

const MAX_FRAGMENT_CHARS: usize = 4000;
const INDEX_VERSION: &str = "6";
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
        .busy_timeout(Duration::from_secs(30))
        .map_err(index_error)?;
    index
        .execute_batch("PRAGMA mmap_size=134217728; PRAGMA synchronous=NORMAL")
        .map_err(index_error)?;
    if !args.reindex {
        let reader = index.transaction().map_err(index_error)?;
        if let Some(cached) = cached_revision(&reader).map_err(index_error)? {
            let mut reports = open_reports(&report_path)?;
            let snapshot = reports.transaction().map_err(source_error)?;
            if revision::read(&snapshot).as_ref() == Some(&cached) {
                drop(snapshot);
                return query::search(&reader, args, query, false, 0);
            }
        }
    }
    storage::enable_compaction(&index).map_err(index_error)?;
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
        storage::initialize(&transaction)?;
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
        query::search(
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
        storage::clear(index).map_err(index_error)?;
    }
    let mut previous: BTreeMap<(String, String), String> = index
        .prepare("SELECT kind, report_key, raw_sha256 FROM indexed_reports")
        .map_err(index_error)?
        .query_map([], |r| Ok(((r.get(0)?, r.get(1)?), r.get(2)?)))
        .map_err(index_error)?
        .collect::<Result<_, _>>()
        .map_err(index_error)?;
    let mut changed = force || metadata_changed;
    let mut source_rows_read = 0_u64;
    for table in &state.tables {
        let mut select = reports
            .prepare(&format!(
                "SELECT {}, raw_json FROM {} ORDER BY {}",
                table.key, table.name, table.key
            ))
            .map_err(source_error)?;
        let mut rows = select.query([]).map_err(source_error)?;
        while let Some(row) = rows.next().map_err(source_error)? {
            let key: String = row.get(0).map_err(source_error)?;
            let raw = row
                .get_ref(1)
                .map_err(source_error)?
                .as_str_or_null()
                .map_err(source_error)?
                .unwrap_or("");
            source_rows_read += 1;
            let sha = crate::snapshots::digest(raw.as_bytes());
            let old = previous.remove(&(table.kind.to_owned(), key.clone()));
            if old.as_ref() == Some(&sha) {
                continue;
            }
            changed = true;
            if old.is_some() {
                storage::sync_report(index, table.kind, &key, true).map_err(index_error)?;
            }
            let fragments = serde_json::from_str(raw)
                .map(|report| fragments(&report, table.kind, &key))
                .unwrap_or_default();
            index.execute(
                "INSERT INTO indexed_reports(kind,report_key,raw_sha256,fragments) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(kind,report_key) DO UPDATE SET raw_sha256=excluded.raw_sha256,fragments=excluded.fragments",
                params![table.kind,key,sha,fragments.len() as i64],
            ).map_err(index_error)?;
            for fragment in fragments {
                storage::insert(index, table.kind, &key, fragment).map_err(index_error)?;
            }
            if !force {
                storage::sync_report(index, table.kind, &key, false).map_err(index_error)?;
            }
        }
    }
    for (kind, key) in previous.into_keys() {
        changed = true;
        storage::sync_report(index, &kind, &key, true).map_err(index_error)?;
        index
            .execute(
                "DELETE FROM indexed_reports WHERE kind=?1 AND report_key=?2",
                params![kind, key],
            )
            .map_err(index_error)?;
    }
    if force {
        storage::rebuild(index).map_err(index_error)?;
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

fn source_ids<'a>(value: &'a Value, ids: &mut BTreeSet<&'a str>) {
    match value {
        Value::Array(items) => {
            for item in items {
                source_ids(item, ids);
            }
        }
        Value::Object(object) => {
            if let Some(items) = object.get("source_ids").and_then(Value::as_array) {
                ids.extend(items.iter().filter_map(Value::as_str));
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

struct ReportSources<'a> {
    paths: Vec<&'a str>,
    by_id: BTreeMap<&'a str, Vec<usize>>,
    detailed: bool,
}

impl<'a> ReportSources<'a> {
    fn new(report: &'a Value) -> Self {
        let mut paths = Vec::new();
        let mut by_id = BTreeMap::<_, Vec<_>>::new();
        for source in report["sources"].as_array().into_iter().flatten() {
            if let Some(id) = source["source_id"].as_str().or_else(|| source.as_str()) {
                by_id.entry(id).or_default().push(paths.len());
            }
            paths.push(source["relative_path"].as_str().unwrap_or(""));
        }
        Self {
            paths,
            by_id,
            detailed: report["schema_version"] == 6,
        }
    }

    fn positions(&self, item: &Value) -> Vec<usize> {
        let mut ids = BTreeSet::new();
        source_ids(item, &mut ids);
        if ids.is_empty() {
            return (0..self.paths.len()).collect();
        }
        let mut positions: Vec<_> = ids
            .into_iter()
            .filter_map(|id| self.by_id.get(id))
            .flatten()
            .copied()
            .collect();
        positions.sort_unstable();
        positions
    }
}

fn add_fragment(
    fragments: &mut Vec<Fragment>,
    sources: &ReportSources<'_>,
    item: &Value,
    title: &str,
) {
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
    let mut labels = Vec::new();
    let mut hints = Vec::new();
    // Keep the display and ordinary BM25 corpus unchanged. A phrase is sourced
    // from one original string, never the spaces used to join fields for display.
    let mut literals = terms::encode(title);
    for part in parts {
        terms::append_literal(&mut literals, part);
    }
    for position in sources.positions(item) {
        let label = format!("S{:03}", position + 1);
        let path = sources.paths[position];
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
        body: if sources.detailed {
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
    let sources = ReportSources::new(report);
    if kind == "daily" {
        if matches!(report["schema_version"].as_u64(), Some(5 | 6)) {
            let Ok(personal) =
                <crate::reports::personal::PersonalReport as serde::Deserialize>::deserialize(
                    report,
                )
            else {
                return result;
            };
            if personal.validate().is_err() {
                return result;
            }
            for item in report["items"].as_array().into_iter().flatten() {
                add_fragment(
                    &mut result,
                    &sources,
                    item,
                    item["title"].as_str().unwrap_or("事项"),
                );
            }
            if result.is_empty() {
                add_fragment(&mut result, &sources, &report["user_notes"], "用户补充");
                add_fragment(&mut result, &sources, &report["gaps"], "材料范围");
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
                    add_fragment(&mut result, &sources, item, title);
                }
            }
        }
        if content.is_none() {
            for (field, title) in [
                ("completed_work", "完成工作"),
                ("work_summary", "工作总结"),
                ("next_plan", "下步计划"),
            ] {
                add_fragment(&mut result, &sources, &report[field], title);
            }
        }
    } else {
        let fields = if kind == "weekly" {
            WEEK_FIELDS
        } else {
            MONTH_FIELDS
        };
        for (field, title) in fields {
            add_fragment(&mut result, &sources, &report[field], title);
        }
    }
    result
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
