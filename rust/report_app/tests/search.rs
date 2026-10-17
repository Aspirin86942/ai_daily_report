use ai_daily_report::{
    archive::cli::Format,
    cli::{Cli, Command, SearchArgs, SearchKind},
    config::AppConfig,
    search,
};
use clap::{CommandFactory, Parser};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, process};

fn configuration() -> (tempfile::TempDir, AppConfig) {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("config")).unwrap();
    fs::write(
        directory.path().join("config/settings.yaml"),
        format!(
            "paths:\n  work_dir: work\n  db_dir: {}\n",
            serde_json::to_string(&directory.path().join("db")).unwrap()
        ),
    )
    .unwrap();
    let config = AppConfig::load_with_env(directory.path(), None, &BTreeMap::new()).unwrap();
    (directory, config)
}

struct Fixture {
    directory: tempfile::TempDir,
    config: AppConfig,
}

impl Fixture {
    fn new() -> Self {
        let (directory, config) = configuration();
        fs::create_dir_all(&config.paths.db_dir).unwrap();
        let fixture = Self { directory, config };
        fixture
            .database()
            .execute_batch(
                "CREATE TABLE daily_reports(date TEXT PRIMARY KEY,raw_json TEXT,updated_at TEXT);
                 CREATE TABLE weekly_reports(week_label TEXT PRIMARY KEY,raw_json TEXT,updated_at TEXT);
                 CREATE TABLE monthly_reports(year_month TEXT PRIMARY KEY,raw_json TEXT,updated_at TEXT);",
            )
            .unwrap();
        fixture
    }

    fn sample() -> Self {
        let fixture = Self::new();
        fixture.put(
            "daily",
            "2026-10-15",
            json!({
                "schema_version": 2,
                "sources": [{"source_id": "old", "relative_path": "历史记录.md"}],
                "summary_items": [{"text": "历史审批记录归档", "source_ids": ["old"]}],
                "verification_items": ["核实预算"]
            }),
        );
        fixture.put(
            "daily",
            "2026-10-16",
            json!({
                "schema_version": 3,
                "sources": [
                    {"source_id": "design", "relative_path": "设计/流程方案.docx"},
                    {"source_id": "notes", "relative_path": "记录/讨论.md"}
                ],
                "content": {
                    "schema_version": 3,
                    "overview": [{"text": "事项已整理", "source_ids": ["design"]}],
                    "matters": [
                        {
                            "title": "采购审批方案",
                            "background": {"text": "已汇总流程现状", "source_ids": ["design"]},
                            "details": [
                                {"text": "集中呈现授权条件", "source_ids": ["design"]},
                                {"text": "确认执行责任", "source_ids": ["design"]}
                            ],
                            "status": {"text": "等待审批确认", "source_ids": ["design"]}
                        },
                        {
                            "title": "供应商资料核对",
                            "details": [{"text": "审批资料仍待补充", "source_ids": ["notes"]}]
                        }
                    ],
                    "pending": [{"text": "审批权限待负责人确认", "source_ids": ["notes"]}]
                }
            }),
        );
        fixture.put(
            "daily",
            "2026-10-17",
            json!({
                "schema_version": 4,
                "sources": [{"source_id": "archive", "relative_path": "归档/进展.md"}],
                "content": {
                    "overview": [{"text": "流程审批进展完成", "source_ids": ["archive"]}],
                    "matters": [], "pending": []
                }
            }),
        );
        fixture.put(
            "weekly",
            "2026-W42",
            json!({"completed_work": "本周审批流程梳理"}),
        );
        fixture.put(
            "monthly",
            "2026-10",
            json!({"overview": "本月审批情况概览"}),
        );
        fixture
    }

    fn database(&self) -> Connection {
        Connection::open(self.config.paths.db_dir.join("reports.sqlite3")).unwrap()
    }

    fn index(&self) -> Connection {
        Connection::open(self.config.paths.db_dir.join("report_search.sqlite3")).unwrap()
    }

    fn put(&self, kind: &str, key: &str, report: Value) {
        self.put_raw(kind, key, &report.to_string(), "2026-10-17T10:00:00Z");
    }

    fn put_raw(&self, kind: &str, key: &str, raw: &str, updated_at: &str) {
        let (table, column) = match kind {
            "daily" => ("daily_reports", "date"),
            "weekly" => ("weekly_reports", "week_label"),
            "monthly" => ("monthly_reports", "year_month"),
            _ => panic!("unexpected fixture kind"),
        };
        self.database()
            .execute(
                &format!(
                    "INSERT INTO {table}({column},raw_json,updated_at) VALUES (?1,?2,?3)
                     ON CONFLICT({column}) DO UPDATE SET raw_json=excluded.raw_json,updated_at=excluded.updated_at"
                ),
                params![key, raw, updated_at],
            )
            .unwrap();
    }

    fn execute(&self, args: &SearchArgs) -> Value {
        search::execute(&self.config, args).unwrap()
    }

    fn meta(&self) -> BTreeMap<String, String> {
        self.index()
            .prepare("SELECT key,value FROM index_meta")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn cli(&self, arguments: &[&str]) -> process::Output {
        let mut command = process::Command::new(env!("CARGO_BIN_EXE_ai-daily-report"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("DAILY_REPORT_") {
                command.env_remove(name);
            }
        }
        command
            .arg("--config-dir")
            .arg(self.directory.path().join("config"))
            .arg("search")
            .args(arguments)
            .output()
            .unwrap()
    }
}

fn arguments(query: &str) -> SearchArgs {
    SearchArgs {
        query: Some(query.into()),
        from: None,
        to: None,
        kind: SearchKind::All,
        source: None,
        limit: 20,
        format: Format::Text,
        reindex: false,
    }
}

fn reindex() -> SearchArgs {
    SearchArgs {
        query: None,
        reindex: true,
        ..arguments("")
    }
}

#[test]
fn chinese_fts_and_short_like_find_specific_matters() {
    let fixture = Fixture::sample();
    let result = fixture.execute(&arguments(" 采购审批 "));
    assert_eq!(result["query"], "采购审批");
    assert_eq!(result["total"], 1);
    assert_eq!(result["index_rebuilt"], true);
    let hit = &result["results"][0];
    assert_eq!(hit["title"], "采购审批方案");
    assert_eq!(hit["report_label"], "2026-10-16");
    assert!(hit["snippet"].as_str().unwrap().contains("[采购审批]"));
    assert_eq!(hit["sources"], json!(["S001"]));
    let result = fixture.execute(&arguments("审批"));
    assert_eq!(result["total"], 7);
    assert_eq!(result["index_rebuilt"], false);
    assert!(result["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|hit| { hit["snippet"].as_str().unwrap().contains("[审批]") }));
    assert_eq!(fixture.execute(&arguments("流程方案"))["total"], 2);
}

#[test]
fn dates_kinds_and_fragment_sources_filter_results() {
    let fixture = Fixture::sample();
    let mut args = arguments("审批");
    args.from = Some("2026-10-16".parse().unwrap());
    args.to = args.from;
    let result = fixture.execute(&args);
    assert_eq!(result["total"], 3);
    assert!(result["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|hit| { hit["kind"] == "daily" && hit["report_date"] == "2026-10-16" }));
    args.source = Some("s001".into());
    let result = fixture.execute(&args);
    assert_eq!(result["total"], 1);
    assert_eq!(result["results"][0]["title"], "采购审批方案");
    args.source = Some("S002".into());
    assert_eq!(fixture.execute(&args)["total"], 2);
    for source in ["S00", "%", "S001_"] {
        args.source = Some(source.into());
        assert_eq!(fixture.execute(&args)["total"], 0);
    }
    args.source = None;
    args.kind = SearchKind::Weekly;
    assert_eq!(fixture.execute(&args)["total"], 0);
    args.from = None;
    args.to = None;
    let weekly = fixture.execute(&args);
    assert_eq!(weekly["total"], 1);
    assert_eq!(weekly["results"][0]["kind"], "weekly");
    assert!(weekly["results"][0]["report_date"].is_null());
    args.kind = SearchKind::Monthly;
    assert_eq!(fixture.execute(&args)["results"][0]["kind"], "monthly");
    args.kind = SearchKind::All;
    args.from = Some("2026-10-17".parse().unwrap());
    assert_eq!(fixture.execute(&args)["total"], 1);
    args.from = None;
    args.to = Some("2026-10-15".parse().unwrap());
    assert_eq!(fixture.execute(&args)["total"], 1);
}

#[test]
fn ordering_limit_and_json_and_text_shapes_are_stable() {
    let fixture = Fixture::sample();
    let mut args = arguments("审批");
    args.limit = 2;
    let result = fixture.execute(&args);
    assert_eq!(result["total"], 7);
    assert_eq!(result["results"].as_array().unwrap().len(), 2);
    assert_eq!(result["results"][0]["report_date"], "2026-10-17");
    assert_eq!(result["results"][1]["title"], "采购审批方案");
    for field in ["query", "total", "index_rebuilt", "results"] {
        assert!(result.get(field).is_some());
    }
    for hit in result["results"].as_array().unwrap() {
        for field in [
            "kind",
            "report_label",
            "report_date",
            "title",
            "snippet",
            "sources",
        ] {
            assert!(hit.get(field).is_some(), "missing {field}");
        }
        assert!(hit["sources"].is_array());
    }
    assert_eq!(
        serde_json::from_str::<Value>(&search::render(&result, Format::Json)).unwrap(),
        result
    );
    let text = search::render(&result, Format::Text);
    assert!(text.contains("2026-10-17 · 日报 · 概览"));
    assert!(text.contains("[审批]"));
    assert!(text.contains("来源：S001"));
    let all = fixture.execute(&arguments("审批"));
    let hits = all["results"].as_array().unwrap();
    assert_eq!(hits[1]["title"], "采购审批方案");
    assert_eq!(hits[2]["title"], "供应商资料核对");
    assert_eq!(hits[3]["title"], "待确认");
    assert_eq!(hits[5]["kind"], "weekly");
    assert_eq!(hits[6]["kind"], "monthly");
    fixture.put("weekly", "2026-W41", json!({"completed_work": "审批事项"}));
    args.kind = SearchKind::Weekly;
    let weekly = fixture.execute(&args);
    assert_eq!(weekly["results"][0]["report_label"], "2026-W42");
    assert_eq!(weekly["results"][1]["report_label"], "2026-W41");
    let empty = fixture.execute(&arguments("从未出现的关键词"));
    assert_eq!(empty["total"], 0);
    assert_eq!(empty["results"], json!([]));
    assert!(search::render(&empty, Format::Text).contains("未找到"));
}

#[test]
fn reindex_updates_metadata_and_deleted_index_is_recoverable() {
    let fixture = Fixture::sample();
    let result = fixture.execute(&reindex());
    assert_eq!(result["index_rebuilt"], true);
    assert_eq!(result["source_count"], 5);
    assert_eq!(result["indexed_fragments"], 9);
    assert_eq!(result["skipped_reports"], 0);
    assert!(result.get("results").is_none());
    assert!(search::render(&result, Format::Text).contains("建立 9 个片段"));
    let meta = fixture.meta();
    assert_eq!(meta["schema_version"], "1");
    assert_eq!(meta["source_count"], "5");
    assert_eq!(meta["source_max_updated_at"], "2026-10-17T10:00:00Z");
    assert_eq!(meta["indexed_fragments"], "9");
    assert_eq!(meta["skipped_reports"], "0");
    fixture
        .index()
        .execute(
            "UPDATE index_meta SET value='0' WHERE key='schema_version'",
            [],
        )
        .unwrap();
    assert_eq!(fixture.execute(&arguments("审批"))["index_rebuilt"], true);
    assert_eq!(fixture.meta()["schema_version"], "1");
    assert_eq!(fixture.execute(&reindex())["index_rebuilt"], true);
    assert_eq!(fixture.execute(&arguments("审批"))["index_rebuilt"], false);
    let index = fixture.config.paths.db_dir.join("report_search.sqlite3");
    fs::remove_file(&index).unwrap();
    let result = fixture.execute(&arguments("审批"));
    assert_eq!(result["index_rebuilt"], true);
    assert_eq!(result["total"], 7);
    assert!(index.is_file());
}

#[test]
fn changed_row_counts_and_timestamps_automatically_rebuild() {
    let fixture = Fixture::sample();
    fixture.execute(&arguments("审批"));
    fixture.put(
        "daily",
        "2026-10-14",
        json!({"work_summary": "审批补充事项"}),
    );
    let result = fixture.execute(&arguments("审批"));
    assert_eq!(result["index_rebuilt"], true);
    assert_eq!(result["total"], 8);
    assert_eq!(fixture.meta()["source_count"], "6");
    fixture.put_raw(
        "monthly",
        "2026-10",
        &json!({"overview": "审批月度总结"}).to_string(),
        "2030-01-01T00:00:00Z",
    );
    fixture.execute(&arguments("审批"));
    fixture.put_raw(
        "daily",
        "2026-10-14",
        &json!({"work_summary": "重新整理流程"}).to_string(),
        "2026-10-18T00:00:00Z",
    );
    let result = fixture.execute(&arguments("审批"));
    assert_eq!(result["index_rebuilt"], true);
    assert_eq!(result["total"], 7);
    assert_eq!(
        fixture.meta()["source_max_updated_at"],
        "2030-01-01T00:00:00Z"
    );
    assert_eq!(fixture.execute(&arguments("审批"))["index_rebuilt"], false);
}

#[test]
fn malformed_empty_and_null_reports_are_skipped() {
    let fixture = Fixture::sample();
    fixture.put_raw("daily", "2026-10-13", "{broken", "2026-10-17T10:00:00Z");
    fixture.put("daily", "2026-10-12", json!({"content": {"overview": []}}));
    fixture
        .database()
        .execute(
            "INSERT INTO daily_reports(date,raw_json) VALUES ('2026-10-11',NULL)",
            [],
        )
        .unwrap();
    let summary = fixture.execute(&reindex());
    assert_eq!(summary["source_count"], 8);
    assert_eq!(summary["skipped_reports"], 3);
    assert_eq!(fixture.meta()["skipped_reports"], "3");
    assert_eq!(fixture.execute(&arguments("审批"))["total"], 7);
}

#[test]
fn missing_database_and_blank_queries_have_explicit_errors() {
    let (_directory, config) = configuration();
    let error = search::execute(&config, &arguments("审批")).unwrap_err();
    assert_eq!(error.code, "SEARCH_DB_MISSING");
    assert!(!config.paths.db_dir.join("report_search.sqlite3").exists());
    assert_eq!(
        search::execute(&config, &reindex()).unwrap_err().code,
        "SEARCH_DB_MISSING"
    );
    let fixture = Fixture::new();
    for query in ["", " \n\t", "　"] {
        let error = search::execute(&fixture.config, &arguments(query)).unwrap_err();
        assert_eq!(error.code, "SEARCH_QUERY_EMPTY");
    }
    assert!(!fixture
        .config
        .paths
        .db_dir
        .join("report_search.sqlite3")
        .exists());
}

#[test]
fn source_database_bytes_hash_length_and_schema_never_change() {
    let fixture = Fixture::sample();
    let path = fixture.config.paths.db_dir.join("reports.sqlite3");
    let before = fs::read(&path).unwrap();
    let digest = Sha256::digest(&before);
    fixture.execute(&arguments("采购审批"));
    fixture.execute(&arguments("审批"));
    fixture.execute(&reindex());
    let after = fs::read(&path).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(digest, Sha256::digest(&after));
    assert_eq!(before, after);
    let files: Vec<_> = fs::read_dir(&fixture.config.paths.db_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files.len(), 2, "{files:?}");
    let tables: Vec<String> = fixture
        .database()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        tables,
        ["daily_reports", "monthly_reports", "weekly_reports"]
    );
}

#[test]
fn missing_tables_and_legacy_columns_are_supported() {
    let fixture = Fixture::new();
    fixture
        .database()
        .execute_batch(
            "DROP TABLE daily_reports;
         DROP TABLE weekly_reports;
         DROP TABLE monthly_reports;
         CREATE TABLE daily_reports(date TEXT PRIMARY KEY,raw_json TEXT);
         CREATE TABLE weekly_reports(week_label TEXT PRIMARY KEY);",
        )
        .unwrap();
    fixture
        .database()
        .execute(
            "INSERT INTO daily_reports(date,raw_json) VALUES (?1,?2)",
            params![
                "2026-10-16",
                json!({"work_summary": "兼容旧表审批内容"}).to_string()
            ],
        )
        .unwrap();
    let result = fixture.execute(&arguments("审批"));
    assert_eq!(result["total"], 1);
    assert_eq!(fixture.meta()["source_count"], "1");
    assert_eq!(fixture.meta()["source_max_updated_at"], "");
    assert_eq!(fixture.execute(&arguments("审批"))["index_rebuilt"], false);
    fixture
        .database()
        .execute(
            "INSERT INTO daily_reports(date,raw_json) VALUES (?1,?2)",
            params![
                "2026-10-17",
                json!({"completed_work": "新增审批内容"}).to_string()
            ],
        )
        .unwrap();
    assert_eq!(fixture.execute(&arguments("审批"))["total"], 2);
}

#[test]
fn all_period_sections_v2_and_daily_fallback_are_indexed() {
    let fixture = Fixture::new();
    fixture.put(
        "weekly",
        "2026-W42",
        json!({
            "completed_work": "一节正文", "self_growth": "二节正文",
            "improvement_actions": "三节正文", "work_summary": "四节正文",
            "next_plan": "五节正文", "support_needed": "六节正文", "other_notes": "七节正文"
        }),
    );
    fixture.put(
        "monthly",
        "2026-10",
        json!({
            "overview": "月度正文", "completed_work": "完成正文",
            "work_summary": "总结正文", "next_plan": "计划正文"
        }),
    );
    fixture.put(
        "daily",
        "2026-10-15",
        json!({
            "schema_version": 2, "summary_items": ["旧版正文"],
            "verification_items": [{"text": "核实正文"}]
        }),
    );
    fixture.put(
        "daily",
        "2026-10-16",
        json!({
            "completed_work": "兼容正文", "work_summary": "工作正文", "next_plan": "后续正文"
        }),
    );
    let result = fixture.execute(&arguments("正文"));
    assert_eq!(result["total"], 16);
    assert_eq!(fixture.execute(&reindex())["indexed_fragments"], 16);
    let mut args = arguments("正文");
    args.kind = SearchKind::Weekly;
    assert_eq!(fixture.execute(&args)["total"], 7);
    args.kind = SearchKind::Monthly;
    assert_eq!(fixture.execute(&args)["total"], 4);
    assert_eq!(fixture.execute(&arguments("工作总结"))["total"], 3);
}

#[test]
fn literal_fts_phrases_and_like_wildcards_are_escaped() {
    let fixture = Fixture::new();
    fixture.put(
        "daily",
        "2026-10-16",
        json!({
            "work_summary": "符号 100% _ \\ %_ \\% 审批 OR 确认 引用\"审批\"流程 ABC"
        }),
    );
    fixture.put(
        "daily",
        "2026-10-17",
        json!({"work_summary": "其他审批内容"}),
    );
    for query in [
        "%",
        "_",
        "\\",
        "%_",
        "\\%",
        "审批 OR 确认",
        "\"审批\"流程",
        "abc",
    ] {
        let result = fixture.execute(&arguments(query));
        assert_eq!(result["total"], 1, "{query}");
        assert_eq!(
            result["results"][0]["report_label"], "2026-10-16",
            "{query}"
        );
        assert!(
            result["results"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains('['),
            "{query}"
        );
    }
    assert_eq!(fixture.execute(&arguments("审批 OR 不存在"))["total"], 0);
    assert_eq!(fixture.execute(&arguments("\"\"\""))["total"], 0);
}

#[test]
fn like_snippets_use_unicode_context_and_fragments_are_bounded() {
    let fixture = Fixture::new();
    fixture.put(
        "daily",
        "2026-10-16",
        json!({
            "work_summary": format!("{}审批{}", "前".repeat(60), "后".repeat(60))
        }),
    );
    let result = fixture.execute(&arguments("审批"));
    assert_eq!(
        result["results"][0]["snippet"],
        format!("…{}[审批]{}…", "前".repeat(40), "后".repeat(40))
    );
    fixture.put(
        "daily",
        "2026-10-17",
        json!({
            "content": {"overview": [format!("超长内容\n\t{}", "汉".repeat(4100))]}
        }),
    );
    assert_eq!(fixture.execute(&arguments("超长内容"))["total"], 1);
    let body: String = fixture
        .index()
        .query_row(
            "SELECT body FROM report_search WHERE report_key='2026-10-17'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(body.chars().count(), 4000);
    assert!(body.ends_with("…[已截断]"));
    assert!(!body.contains(['\n', '\t']));
}

#[test]
fn failed_rebuild_rolls_back_fragments_and_metadata() {
    let fixture = Fixture::sample();
    fixture.execute(&arguments("审批"));
    let meta = fixture.meta();
    let old_bodies: Vec<String> = fixture
        .index()
        .prepare("SELECT body FROM report_search ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    fixture
        .index()
        .execute_batch(
            "CREATE TRIGGER fail_reindex BEFORE INSERT ON index_meta
         BEGIN SELECT RAISE(ABORT,'fixture failure'); END;",
        )
        .unwrap();
    fixture.put(
        "daily",
        "2026-10-14",
        json!({"work_summary": "新增检索内容"}),
    );
    let error = search::execute(&fixture.config, &reindex()).unwrap_err();
    assert_eq!(error.code, "SEARCH_INDEX_ERROR");
    assert_eq!(fixture.meta(), meta);
    let bodies: Vec<String> = fixture
        .index()
        .prepare("SELECT body FROM report_search ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(bodies, old_bodies);
    fixture
        .index()
        .execute_batch("DROP TRIGGER fail_reindex")
        .unwrap();
    let result = fixture.execute(&arguments("新增检索"));
    assert_eq!(result["index_rebuilt"], true);
    assert_eq!(result["total"], 1);
}

#[test]
fn cli_arguments_enforce_search_contract() {
    Cli::command().debug_assert();
    let parsed = Cli::try_parse_from(["app", "search", "--query", "审批"]).unwrap();
    let Some(Command::Search(args)) = parsed.command else {
        panic!("expected search command")
    };
    assert_eq!(args.limit, 20);
    assert_eq!(args.kind, SearchKind::All);
    assert!(matches!(args.format, Format::Text));
    assert!(Cli::try_parse_from(["app", "search", "--reindex"]).is_ok());
    for arguments in [
        vec!["app", "search"],
        vec!["app", "search", "--query", "审批", "--reindex"],
        vec!["app", "search", "--query", "审批", "--limit", "0"],
        vec!["app", "search", "--query", "审批", "--limit", "501"],
        vec!["app", "search", "--query", "审批", "--from", "2026-1-1"],
        vec!["app", "search", "--query", "审批", "--kind", "other"],
    ] {
        assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
    }
    assert!(Cli::try_parse_from([
        "app",
        "search",
        "--query",
        "审批",
        "--limit",
        "500",
        "--kind",
        "daily",
        "--from",
        "2026-10-16",
        "--to",
        "2026-10-17",
        "--source",
        "S001",
        "--format",
        "json"
    ])
    .is_ok());
}

#[test]
fn cli_search_outputs_text_json_and_reindex_without_opening_app() {
    let fixture = Fixture::sample();
    let before = fs::read(fixture.config.paths.db_dir.join("reports.sqlite3")).unwrap();
    for arguments in [
        vec![],
        vec!["--query", "审批", "--limit", "0"],
        vec!["--query", "审批", "--reindex"],
        vec!["--query", "审批", "--from", "2026-1-1"],
    ] {
        let output = fixture.cli(&arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
    }
    assert!(fixture.cli(&["--help"]).status.success());
    for arguments in [
        vec!["--config-dir", "search", "daily", "--regenerate"],
        vec!["--config-dir=search", "daily", "--regenerate"],
    ] {
        let output = process::Command::new(env!("CARGO_BIN_EXE_ai-daily-report"))
            .args(&arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
    }
    let output = fixture.cli(&["--query", "采购审批", "--format", "json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["total"], 1);
    let output = fixture.cli(&["--query", "审批", "--limit", "1"]);
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().contains("[审批]"));
    let output = fixture.cli(&["--reindex", "--format", "json"]);
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["indexed_fragments"], 9);
    let output = fixture.cli(&["--query", "　"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("SEARCH_QUERY_EMPTY"));
    assert_eq!(
        before,
        fs::read(fixture.config.paths.db_dir.join("reports.sqlite3")).unwrap()
    );
    assert!(!fixture.config.paths.db_dir.join("runs.sqlite3").exists());
    assert!(!fixture.config.paths.db_dir.join("scanner.sqlite3").exists());
}
