use ai_daily_report::{
    archive::cli::Format,
    cli::{Cli, Command, SearchArgs, SearchKind},
    config::AppConfig,
    search,
    store::Store,
};
use clap::{CommandFactory, Parser};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, process, time::Instant};

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

    fn managed() -> Self {
        let (directory, config) = configuration();
        fs::create_dir_all(&config.paths.db_dir).unwrap();
        Store::open(
            &config.paths.db_dir.join("reports.sqlite3"),
            chrono::Utc::now(),
        )
        .unwrap();
        Self { directory, config }
    }

    fn managed_daily(db: &Connection, key: &str, text: &str) {
        db.execute(
            "INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json,updated_at)
             VALUES (?1,'','','',?2,'fixed') ON CONFLICT(date) DO UPDATE SET raw_json=excluded.raw_json",
            params![key, json!({"work_summary":text}).to_string()],
        ).unwrap();
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
        command.current_dir(self.directory.path()).env_clear();
        for key in ["SystemRoot", "PATH", "TEMP", "TMP", "TMPDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
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

    fn cli_query(&self, query: &str) -> Value {
        let output = self.cli(&["--query", query, "--format", "json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn arguments(query: &str) -> SearchArgs {
    SearchArgs {
        query: Some(query.into()),
        phrase: false,
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
    assert_eq!(result["results"][0]["title"], "采购审批方案");
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
    assert!(text.contains("2026-10-16 · 日报 · 采购审批方案"));
    assert!(text.contains("[审批]"));
    assert!(text.contains("来源：S001"));
    let all = fixture.execute(&arguments("审批"));
    let hits = all["results"].as_array().unwrap();
    assert_eq!(&hits[..2], result["results"].as_array().unwrap());
    assert!(hits.iter().any(|hit| hit["kind"] == "weekly"));
    assert!(hits.iter().any(|hit| hit["kind"] == "monthly"));
    fixture.put("weekly", "2026-W41", json!({"completed_work": "审批事项"}));
    args.kind = SearchKind::Weekly;
    let weekly = fixture.execute(&args);
    assert_eq!(weekly["results"][0]["report_label"], "2026-W41");
    assert_eq!(weekly["results"][1]["report_label"], "2026-W42");
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
    assert_eq!(meta["schema_version"], "5");
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
    assert_eq!(fixture.meta()["schema_version"], "5");
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
fn older_row_replacement_and_deletion_refresh_only_its_fragments() {
    let fixture = Fixture::sample();
    fixture.execute(&reindex());
    let unchanged_rowids = || -> Vec<i64> {
        fixture
            .index()
            .prepare(
                "SELECT rowid FROM report_search WHERE report_key != '2026-10-15' ORDER BY rowid",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before = unchanged_rowids();
    let max = fixture.meta()["source_max_updated_at"].clone();
    fixture.put_raw(
        "daily",
        "2026-10-15",
        r#"{"work_summary":"此前日报补记实付差额"}"#,
        "2026-10-15T10:00:00Z",
    );
    let result = fixture.execute(&arguments("实付差额"));
    assert_eq!(result["total"], 1);
    assert_eq!(result["index_rebuilt"], true);
    assert_eq!(fixture.meta()["source_max_updated_at"], max);
    assert_eq!(unchanged_rowids(), before);
    assert_eq!(
        fixture.execute(&arguments("实付差额"))["index_rebuilt"],
        false
    );
    fixture
        .database()
        .execute("DELETE FROM daily_reports WHERE date='2026-10-15'", [])
        .unwrap();
    assert_eq!(fixture.execute(&arguments("实付差额"))["total"], 0);
    assert_eq!(unchanged_rowids(), before);
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
fn explicit_phrase_preserves_token_order_and_separators() {
    let fixture = Fixture::new();
    fixture.put(
        "daily",
        "2026-10-16",
        json!({"work_summary": "原记录写有预算 审批。"}),
    );
    fixture.put(
        "daily",
        "2026-10-17",
        json!({"work_summary": "审批后复核预算。"}),
    );
    assert_eq!(fixture.execute(&arguments("预算 审批"))["total"], 2);
    let mut phrase = arguments("预算 审批");
    phrase.phrase = true;
    let exact = fixture.execute(&phrase);
    assert_eq!(exact["total"], 1);
    assert_eq!(exact["results"][0]["report_date"], "2026-10-16");
}

#[test]
fn cli_phrase_requires_original_whitespace_and_one_contiguous_source_string() {
    let fixture = Fixture::new();
    for (day, separator) in [(1, " "), (2, "  "), (3, "　"), (4, "\t"), (5, "\n")] {
        fixture.put(
            "daily",
            &format!("2026-10-{day:02}"),
            json!({"work_summary":format!("原文预算{separator}审批。")}),
        );
    }
    fixture.put(
        "daily",
        "2026-10-06",
        json!({"content":{"matters":[{
            "background":{"text":"原文预算"},"details":[{"text":"审批。"}]
        }]}}),
    );
    fixture.put("daily", "2026-10-07", json!({"work_summary":"另有金额　"}));
    let phrase = |q: &str| {
        let output = fixture.cli(&["--query", q, "--phrase", "--format", "json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    assert_eq!(fixture.cli_query("预算　 审批")["total"], 6);
    fixture
        .index()
        .execute_batch(
            "DROP TABLE report_phrases; UPDATE index_meta SET value='4' WHERE key='schema_version'",
        )
        .unwrap();
    for (day, separator) in [(1, " "), (2, "  "), (3, "　"), (4, "\t"), (5, "\n")] {
        let literal = format!("预算{separator}审批");
        let hits = phrase(&literal);
        assert_eq!(hits["total"], 1, "{separator:?}");
        assert_eq!(
            hits["results"][0]["report_label"],
            format!("2026-10-{day:02}")
        );
        assert!(hits["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains(&format!("[{literal}]")));
        if day == 1 {
            assert_eq!(hits["index_rebuilt"], true);
            assert_eq!(fixture.meta()["schema_version"], "5");
        }
    }
    assert_eq!(phrase("预算\n\n审批")["total"], 0);
    assert_eq!(phrase(" 金额　")["total"], 0);
    let trailing = phrase("额　");
    assert_eq!(trailing["query"], "额　");
    assert_eq!(trailing["total"], 1);
    assert_eq!(fixture.cli_query("审批 预算")["total"], 6);
    fixture
        .database()
        .execute("DELETE FROM daily_reports WHERE date='2026-10-02'", [])
        .unwrap();
    assert_eq!(phrase("预算  审批")["total"], 0);
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
            .current_dir(fixture.directory.path())
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

#[test]
fn independent_cli_processes_reuse_a_read_only_snapshot() {
    let fixture = Fixture::managed();
    let mut db = fixture.database();
    let tx = db.transaction().unwrap();
    let first = chrono::NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
    for i in 0..240 {
        let date = first + chrono::Duration::days(i);
        Fixture::managed_daily(
            &tx,
            &date.to_string(),
            &format!("合成样本{}核查", "独立记录".repeat(512)),
        );
    }
    tx.commit().unwrap();
    drop(db);
    let path = fixture.config.paths.db_dir.join("reports.sqlite3");
    let original = fs::metadata(&path).unwrap().permissions();
    let mut ro = original.clone();
    ro.set_readonly(true);
    fs::set_permissions(&path, ro).unwrap();
    let cold = fixture.cli_query("核查");
    assert_eq!(cold["source_rows_read"], 240);
    for _ in 0..3 {
        let started = Instant::now();
        let warm = fixture.cli_query("核查");
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(warm["total"], 240);
        assert_eq!(warm["source_rows_read"], 0);
        assert_eq!(warm["index_rebuilt"], false);
        eprintln!("search warm CLI: 240 synthetic reports, 0 source rows read, {ms:.3} ms");
    }
    fs::set_permissions(path, original).unwrap();
}

#[test]
fn wal_revisions_follow_commits_rollbacks_all_tables_and_trigger_loss() {
    let fixture = Fixture::managed();
    let mut db = fixture.database();
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    Fixture::managed_daily(&db, "2026-10-10", "原有材料");
    assert_eq!(fixture.cli_query("原有")["total"], 1);
    let tx = db.transaction().unwrap();
    Fixture::managed_daily(&tx, "2026-10-10", "替换材料");
    assert_eq!(fixture.cli_query("替换")["total"], 0);
    tx.rollback().unwrap();
    assert_eq!(fixture.cli_query("原有")["source_rows_read"], 0);
    Fixture::managed_daily(&db, "2026-10-10", "替换材料");
    assert_eq!(fixture.cli_query("替换")["total"], 1);
    db.execute_batch(
        "INSERT INTO weekly_reports(week_label,date_range,completed_work,self_growth,improvement_actions,work_summary,next_plan,support_needed,other_notes,raw_json)
         VALUES ('2026-W41','','','','','','','','','{\"work_summary\":\"替换材料\"}');
         INSERT INTO monthly_reports(year_month,overview,completed_work,work_summary,next_plan,raw_json)
         VALUES ('2026-10','','','','','{\"work_summary\":\"替换材料\"}');",
    ).unwrap();
    assert_eq!(fixture.cli_query("替换")["total"], 3);
    db.execute_batch(
        "DELETE FROM weekly_reports; DELETE FROM monthly_reports; DELETE FROM daily_reports;",
    )
    .unwrap();
    assert_eq!(fixture.cli_query("替换")["total"], 0);
    Fixture::managed_daily(&db, "2026-10-10", "待核记录");
    fixture.cli_query("待核");
    let trigger: String = db
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='search_revision_daily_reports_update'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    db.execute_batch("DROP TRIGGER search_revision_daily_reports_update")
        .unwrap();
    Fixture::managed_daily(&db, "2026-10-10", "人工改写");
    db.execute_batch(&trigger).unwrap();
    assert_eq!(fixture.cli_query("人工改写")["total"], 1);
    db.execute_batch("DROP TRIGGER search_revision_daily_reports_update")
        .unwrap();
    let legacy = fixture.cli_query("人工改写");
    assert_eq!(legacy["source_rows_read"], 1);
    assert_eq!(fixture.cli_query("人工改写")["source_rows_read"], 1);
}

#[test]
fn divergent_database_copies_and_concurrent_cli_rebuilds_use_one_snapshot() {
    let fixture = Fixture::managed();
    let db_path = fixture.config.paths.db_dir.join("reports.sqlite3");
    let fork_path = fixture.config.paths.db_dir.join("fork.sqlite3");
    Fixture::managed_daily(&fixture.database(), "2026-10-10", "根本");
    fs::copy(&db_path, &fork_path).unwrap();
    Fixture::managed_daily(&fixture.database(), "2026-10-10", "甲稿");
    Fixture::managed_daily(&Connection::open(&fork_path).unwrap(), "2026-10-10", "乙稿");
    assert_eq!(fixture.cli_query("甲稿")["total"], 1);
    fs::rename(fork_path, &db_path).unwrap();
    let barrier = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        for _ in 0..3 {
            jobs.push(scope.spawn(|| {
                barrier.wait();
                let result = fixture.cli_query("乙稿");
                assert_eq!(result["total"], 1);
            }));
        }
        barrier.wait();
        let output = fixture.cli(&["--reindex", "--format", "json"]);
        assert!(output.status.success());
        for job in jobs {
            job.join().unwrap();
        }
    });
    let current = fixture.cli_query("甲稿");
    assert_eq!(current["total"], 0);
    assert_eq!(current["source_rows_read"], 0);
    let mut index = fixture.index();
    let pending = index
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    pending
        .execute("DELETE FROM index_meta WHERE key='source_revision'", [])
        .unwrap();
    let concurrent_reader = fixture.cli_query("乙稿");
    assert_eq!(concurrent_reader["source_rows_read"], 0);
    assert_eq!(concurrent_reader["total"], 1);
    pending.rollback().unwrap();
}

#[test]
fn short_and_spaced_literal_phrases_rank_by_bm25_before_date_and_limit() {
    let fixture = Fixture::new();
    for (date, text) in [
        ("2026-10-10", "核查金额。核查金额。核查金额。核查金额。"),
        ("2026-10-11", "核查原始凭证。金额需要再次整理并且等待复核。"),
        ("2026-10-12", "核查缺项，另行补齐。"),
    ] {
        fixture.put("daily", date, json!({"work_summary":text}));
    }
    for (q, count) in [
        ("额", 2),
        ("核查", 3),
        ("核查　 金额", 2),
        ("核查 金", 2),
        ("核查金额", 1),
    ] {
        let mut args = arguments(q);
        args.limit = 1;
        let result = fixture.execute(&args);
        assert_eq!(result["total"], count, "{q}");
        assert_eq!(result["results"][0]["report_label"], "2026-10-10", "{q}");
        assert!(result["results"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains('['));
    }
    fixture.put(
        "daily",
        "2026-10-13",
        json!({"completed_work":"分隔\u{1f}%_\u{1e}文\u{0000}尾"}),
    );
    for q in ["\u{1f}", "\u{1e}文", "文\u{0000}尾", "%_"] {
        assert_eq!(fixture.execute(&arguments(q))["total"], 1, "{q:?}");
    }
}

#[cfg(windows)]
#[test]
fn read_only_legacy_header_is_bound_to_the_open_windows_file_identity() {
    let fixture = Fixture::new();
    let source = fixture.config.paths.db_dir.join("reports.sqlite3");
    let fork = fixture.config.paths.db_dir.join("replacement.sqlite3");
    fixture.put("daily", "2026-10-10", json!({"work_summary":"样本甲"}));
    fs::copy(&source, &fork).unwrap();
    let replace = "UPDATE daily_reports SET raw_json=? WHERE date='2026-10-10'";
    fixture
        .database()
        .execute(replace, [json!({"work_summary":"样本乙"}).to_string()])
        .unwrap();
    Connection::open(&fork)
        .unwrap()
        .execute(replace, [json!({"work_summary":"样本丙"}).to_string()])
        .unwrap();
    let fixed = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for p in [&source, &fork] {
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(fixed)
            .unwrap();
    }
    let before = fs::read(&source).unwrap();
    let other = fs::read(&fork).unwrap();
    assert_eq!(&before[..100], &other[..100]);
    assert_eq!(before.len(), other.len());
    assert_eq!(fixture.cli_query("样本乙")["source_rows_read"], 1);
    assert_eq!(fixture.cli_query("样本乙")["source_rows_read"], 0);
    fs::rename(&fork, &source).unwrap();
    assert_eq!(fixture.cli_query("样本丙")["total"], 1);
    assert_eq!(fixture.cli_query("样本乙")["total"], 0);
    assert_eq!(fs::read(source).unwrap(), other);
}

#[test]
fn failed_v3_index_upgrade_preserves_the_old_schema_and_all_data() {
    let fixture = Fixture::sample();
    let index = fixture.index();
    index
        .execute_batch(
            "CREATE TABLE index_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
         INSERT INTO index_meta VALUES ('schema_version','3');
         CREATE VIRTUAL TABLE report_search USING fts5(
           report_key UNINDEXED,kind UNINDEXED,report_label UNINDEXED,report_date UNINDEXED,
           title,body,sources UNINDEXED,tokenize='trigram');
         INSERT INTO report_search(body) VALUES ('旧索引片段');
         CREATE TRIGGER fail_upgrade BEFORE INSERT ON index_meta
         BEGIN SELECT RAISE(ABORT,'fixture failure'); END;",
        )
        .unwrap();
    assert_eq!(
        search::execute(&fixture.config, &arguments("流程"))
            .unwrap_err()
            .code,
        "SEARCH_INDEX_ERROR"
    );
    let old: String = index
        .query_row(
            "SELECT body FROM report_search WHERE report_search MATCH '旧索引'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old, "旧索引片段");
    assert_eq!(fixture.meta()["schema_version"], "3");
    index.execute_batch("DROP TRIGGER fail_upgrade").unwrap();
    assert_eq!(fixture.execute(&arguments("采购审批"))["total"], 1);
    assert_eq!(fixture.meta()["schema_version"], "5");
}

#[test]
fn reopening_a_retained_unmarked_store_preserves_its_v1_schema_and_bytes() {
    let (directory, config) = configuration();
    fs::create_dir_all(&config.paths.db_dir).unwrap();
    let path = config.paths.db_dir.join("reports.sqlite3");
    let legacy = Connection::open(&path).unwrap();
    legacy
        .execute_batch(ai_daily_report::store::SCHEMA)
        .unwrap();
    legacy.execute_batch(
        "INSERT INTO report_schema_migrations VALUES (1,'fixture');
         INSERT INTO report_metadata VALUES ('scope_id','personal_work'),('timezone','Asia/Shanghai');",
    ).unwrap();
    Fixture::managed_daily(&legacy, "2026-10-10", "保留旧库");
    drop(legacy);
    let before = fs::read(&path).unwrap();
    Store::open(&path, chrono::Utc::now()).unwrap();
    let fixture = Fixture { directory, config };
    assert_eq!(fixture.cli_query("保留旧库")["total"], 1);
    assert_eq!(before, fs::read(path).unwrap());
}

fn attachment_report(
    quote: &str,
) -> (
    ai_daily_report::reports::personal::PersonalReport,
    ai_daily_worker_contract::SourceRange,
) {
    use ai_daily_report::{
        evidence::EvidenceRef,
        reports::{
            data::ReportSource,
            personal::{Material, PersonalReport, Selection},
        },
        snapshots::digest,
        types::{new_run_id, RunContext},
    };
    use ai_daily_worker_contract::{SourceRange, SourceRangeUnit};

    let context = RunContext::new(
        new_run_id(),
        chrono::Utc::now(),
        "2026-10-10".parse().unwrap(),
        "isolated".into(),
    )
    .unwrap();
    let mut source = ReportSource::user(quote, context.started_at);
    source.source_kind = "file".into();
    source.source_id = format!("src_{}", digest(b"test-block"));
    source.relative_path = Some("样本/合成表.xlsx".into());
    source.extension = Some(".xlsx".into());
    let mut range = SourceRange::new(SourceRangeUnit::SheetRow, 12, 14, Some("汇总".into()));
    range.column_start = Some(2);
    range.column_end = Some(4);
    let reference = EvidenceRef {
        source_id: source.source_id.clone(),
        version_id: "test-version".into(),
        extraction_id: "test-extraction".into(),
        unit_id: "sample-12".into(),
        side: "current".into(),
        raw_sha256: source.content_sha256.clone().unwrap(),
        unit_sha256: source.excerpt_sha256.clone(),
        quote_sha256: source.excerpt_sha256.clone(),
        quote: quote.into(),
        unit_char_start: 0,
        input_start: 0,
        input_end: quote.chars().count(),
        ranges: vec![range.clone()],
    };
    let mut selection = Selection::default();
    selection.summary.selected_chars = source.excerpt_chars;
    selection.summary.observed_files = 1;
    selection.summary.inspected_files = 1;
    selection.summary.selected_files = 1;
    selection.materials.push(Material {
        source,
        text: quote.into(),
        evidence: vec![reference],
        priority: 1,
    });
    selection.validate().unwrap();
    (
        PersonalReport::from_selection(&context, "work", &selection),
        range,
    )
}

#[test]
fn cli_finds_shared_heading_at_current_source_range_and_only_lends_its_own_text() {
    use ai_daily_report::{reports::personal::DETAILED_SCAN_TEMPLATE, snapshots::digest};
    let fixture = Fixture::new();
    let heading = "双人复核后才可付款";
    let cli = |q: &str, label: &str| {
        let output = fixture.cli(&[
            "--query", q, "--phrase", "--source", label, "--format", "json",
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    for first_quote in [
        "只在首份出现的余行",
        "双人复核后才可付款\n只在首份出现的余行",
    ] {
        let (mut report, first_range) = attachment_report(first_quote);
        let (mut second_report, mut second_range) = attachment_report("第二份的记录");
        report.schema_version = 6;
        report.sources[0].coverage_limits["personal"] =
            json!({"context":[{"text":heading,"ranges":[first_range]}]});
        let mut second = second_report.sources.remove(0);
        let mut refs = second_report.evidence.remove(&second.source_id).unwrap();
        second.source_id = format!("src_{}", digest(b"second-heading-source"));
        refs[0].source_id.clone_from(&second.source_id);
        refs[0].ranges[0].start = 22;
        refs[0].ranges[0].end = 24;
        second_range.start = 20;
        second_range.end = 21;
        second.coverage_limits["personal"] =
            json!({"context":[{"text":heading,"ranges":[second_range]}]});
        report.evidence.insert(second.source_id.clone(), refs);
        report.sources.push(second);
        report
            .render_detailed_output(DETAILED_SCAN_TEMPLATE)
            .unwrap();
        fixture.put("daily", &report.date, json!(report));
        let s1 = cli(heading, "S001");
        assert_eq!(s1["total"], 1);
        assert_eq!(
            s1["results"][0]["location"]["source_ranges"],
            json!([first_range])
        );
        let s2 = cli(heading, "S002");
        assert_eq!(s2["total"], 1);
        let loc = &s2["results"][0]["location"];
        assert_eq!(loc["anchor"], "s002");
        assert_eq!(loc["source_id"], report.sources[1].source_id);
        assert_eq!(loc["source_ranges"], json!([second_range]));
        assert!(loc["range"].as_str().unwrap().contains("第 20 行"));
        assert!(loc["original_link"].as_str().unwrap().ends_with("#s001-p1"));
        let attachment = report.evidence_attachment.as_mut().unwrap();
        let start = loc["line_start"].as_u64().unwrap() as usize;
        let end = loc["line_end"].as_u64().unwrap() as usize;
        assert!(attachment
            .markdown
            .lines()
            .skip(start - 1)
            .take(end - start + 1)
            .any(|s| s.contains("](#s001-p1)")));
        assert_eq!(cli("只在首份出现的余行", "S002")["total"], 0);
        assert!(attachment.markdown.contains("第 20 行"));
        attachment.markdown = attachment.markdown.replace("第 20 行", "第 99 行");
        attachment.sha256 = digest(attachment.markdown.as_bytes());
        fixture.put("daily", &report.date, json!(report));
        assert_eq!(cli(heading, "S002")["total"], 0);
    }
}

#[test]
fn only_published_attachment_text_is_searchable_with_its_path_anchor_and_range() {
    use ai_daily_report::{
        reports::personal::{DETAILED_SCAN_TEMPLATE, SCAN_TEMPLATE},
        snapshots::digest,
    };
    let fixture = Fixture::new();
    let quote = format!("{}<凭据>_尾部核差\n合成附注", "前置资料".repeat(1100));
    let (mut report, range) = attachment_report(&quote);
    let mut args = arguments("<凭据>_尾部核差");
    args.source = Some("s001".into());
    for (version, template, id) in [
        (5, SCAN_TEMPLATE, "s001"),
        (6, DETAILED_SCAN_TEMPLATE, "s001-p1"),
    ] {
        report.schema_version = version;
        report.evidence_attachment = None;
        report.evidence_attachment = report.render_attachment(template).unwrap();
        let attachment = report.evidence_attachment.as_ref().unwrap();
        fixture.put("daily", &report.date, json!(report));
        let result = fixture.execute(&args);
        assert_eq!(result["total"], 1, "v{version}");
        let hit = &result["results"][0];
        assert_eq!(hit["sources"], json!(["S001"]));
        let loc = &hit["location"];
        assert_eq!(loc["relative_path"], "样本/合成表.xlsx");
        assert_eq!(loc["source_ranges"], json!([range]));
        assert!(loc["range"].as_str().unwrap().contains("汇总"));
        assert_eq!(
            loc["link"],
            format!("{}#{id}", attachment.link(&report.date))
        );
        let start = loc["line_start"].as_u64().unwrap() as usize;
        let end = loc["line_end"].as_u64().unwrap() as usize;
        assert!(attachment
            .markdown
            .lines()
            .skip(start - 1)
            .take(end - start + 1)
            .any(|line| line.contains("尾部核差")));
        assert!(search::render(&result, Format::Text).contains(loc["link"].as_str().unwrap()));
    }
    let mut repeated = report.clone();
    let mut second = repeated.sources[0].clone();
    let mut second_refs = repeated.evidence[&second.source_id].clone();
    second.source_id = format!("src_{}", digest(b"second-test-block"));
    second_refs[0].source_id.clone_from(&second.source_id);
    second_refs[0].ranges[0].start = 22;
    second_refs[0].ranges[0].end = 24;
    repeated
        .evidence
        .insert(second.source_id.clone(), second_refs);
    repeated.sources.push(second);
    repeated.evidence_attachment = None;
    repeated
        .render_detailed_output(DETAILED_SCAN_TEMPLATE)
        .unwrap();
    fixture.put("daily", &repeated.date, json!(repeated));
    let mut alias_args = arguments("尾部核差");
    alias_args.source = Some("S002".into());
    let result = fixture.execute(&alias_args);
    assert_eq!(result["total"], 1);
    let alias = &result["results"][0]["location"];
    assert_eq!(alias["anchor"], "s002");
    assert_eq!(alias["source_ranges"][0]["start"], 22);
    assert!(alias["original_link"]
        .as_str()
        .unwrap()
        .ends_with("#s001-p1"));

    // A frozen custom attachment omitted the source but the validated quote still exists
    // in report.evidence. Only the bytes actually published under this new hash may match.
    let attachment = report.evidence_attachment.as_mut().unwrap();
    attachment.markdown = "冻结的独立说明\n另附定位文字\n".into();
    attachment.sha256 = digest(attachment.markdown.as_bytes());
    fixture.put("daily", &report.date, json!(report));
    assert_eq!(fixture.execute(&args)["total"], 0);
    let result = fixture.execute(&arguments("定位文字"));
    assert_eq!(result["total"], 1);
    assert_eq!(result["results"][0]["sources"], json!([]));
    assert_eq!(result["results"][0]["location"]["line_end"], 2);
    report
        .evidence_attachment
        .as_mut()
        .unwrap()
        .markdown
        .push('坏');
    fixture.put("daily", &report.date, json!(report));
    assert_eq!(fixture.execute(&arguments("定位文字"))["total"], 0);
}
