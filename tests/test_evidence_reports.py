from datetime import datetime
from hashlib import sha256
import json
from pathlib import Path
import sqlite3

import pytest

from src.models.evidence_report import EvidenceDailyReportData, EvidenceSummaryData, ReportSource
from src.models.schemas import DailyReportData, parse_daily_report
from src.services.report_gen import ReportGenerator
from src.services.sqlite_store import SQLiteStore
from src.utils.text_tools import format_period_report_context


def evidence_report(**overrides):
    source_id = "src_" + sha256("项目/方案.md".encode()).hexdigest()
    record = ReportSource(
        source_id=source_id, relative_path="项目/方案.md", extension=".md", size_bytes=60,
        modified_at_utc="2026-10-11T01:00:00Z", observed_at_utc="2026-10-12T01:00:00Z",
        content_sha256="a" * 64, excerpt_sha256="b" * 64, excerpt_chars=20, model_chars=20,
        parser_backend="light_text_v2", worker_lane="rust_core", parse_status="success",
    )
    data = dict(date="2026-10-11", window_start_utc="2026-10-10T16:00:00Z",
                window_end_utc="2026-10-11T16:00:00Z", generated_at="2026-10-12T01:00:00Z",
                quality="complete", sources=[record], summary_items=[{"text": "方案记载了接口安排。", "source_ids": [source_id]}],
                verification_items=[{"text": "实际交付状态待核实。", "source_ids": [source_id]}])
    data.update(overrides)
    return EvidenceDailyReportData.model_validate(data)


def test_report_contract_checks_window_sources_and_quality():
    report = evidence_report()
    assert isinstance(parse_daily_report(report.model_dump()), EvidenceDailyReportData)
    for changes in (
        {"window_end_utc": "2026-10-12T16:00:00Z"},
        {"quality": "no_changes"},
        {"summary_items": [{"text": "无法追溯", "source_ids": ["src_" + "f" * 64]}]},
        {"sources": []},
        {"quality": "complete", "gaps": [{"code": "unreadable", "message": "一个目录不可读", "stage": "discovery"}]},
    ):
        with pytest.raises(ValueError):
            evidence_report(**changes)


def test_model_response_must_reference_only_current_batch():
    summary = EvidenceSummaryData(summary_items=[{"text": "用户陈述", "source_ids": ["user_input"]}], verification_items=[])
    summary.validate_source_ids({"user_input"})
    with pytest.raises(ValueError):
        summary.validate_source_ids({"src_" + "a" * 64})


def test_render_and_period_context_keep_file_evidence_semantics(tmp_path):
    report = evidence_report(user_notes="手工记录：等待复核。")
    text = ReportGenerator(tmp_path).render_markdown(report)
    assert "2026-10-12 09:00:00 +08:00" in text
    assert "方案记载了接口安排。 [S001]" in text
    assert "项目/方案.md" in text
    assert "手工记录：等待复核。" in text
    assert text.endswith("\n") and "\r" not in text
    context = format_period_report_context([report])
    assert "文件内容证据" in context and "待核实事项" in context
    assert "今日工作完成内容" not in context


def test_empty_report_has_a_clear_zero_change_message(tmp_path):
    report = evidence_report(quality="no_changes", sources=[], summary_items=[], verification_items=[])
    text = ReportGenerator(tmp_path).render_markdown(report)
    assert "未发现符合扫描规则的修改文件" in text


@pytest.mark.parametrize("generated_at,historical", [
    ("2026-10-12T01:00:00Z", False),
    ("2026-10-12T15:59:59Z", False),
    ("2026-10-12T16:00:00Z", True),
])
def test_historical_report_notice_uses_shanghai_calendar_boundary(tmp_path, generated_at, historical):
    report = evidence_report(generated_at=generated_at, quality="no_changes", sources=[],
                             summary_items=[], verification_items=[])
    text = ReportGenerator(tmp_path).render_markdown(report)
    assert report.is_historical_reconstruction is historical
    assert ("**历史补录。**" in text) is historical
    if historical:
        assert "已被覆盖的旧版本无法还原" in text
        assert "不能据此认定当天没有工作" in text


def test_mixed_reports_include_weekends_and_only_count_gaps_since_activation(tmp_path):
    store = SQLiteStore(tmp_path / "reports.sqlite3")
    old = DailyReportData(date="2026-10-09", completed_work="历史事实", work_summary="历史小结", next_plan="历史计划")
    store.save_report(old)
    report = evidence_report()
    # Import a pre-existing v2 payload to exercise reading independently of the
    # publication service, which is tested with its own transaction boundary.
    with store._get_conn() as conn:
        conn.execute("INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json,schema_version,report_kind) VALUES (?,?,?,?,?,2,'file_evidence')",
                     (report.date, "", "", "", report.model_dump_json()))
        conn.execute("INSERT INTO report_metadata(key,value) VALUES ('first_target_date','2026-10-10')")
    loaded, missing = store.get_reports_in_range(datetime(2026, 10, 8).date(), datetime(2026, 10, 12).date())
    assert [r.date for r in loaded] == ["2026-10-09", "2026-10-11"]
    assert missing == ["2026-10-10", "2026-10-12"]
    assert store.get_yesterday_plan(datetime(2026, 10, 12)) == ""
    with pytest.raises(ValueError, match="RUN_PUBLICATION"):
        store.save_report(report)


def legacy_database(path):
    connection = sqlite3.connect(path)
    connection.execute("PRAGMA journal_mode=WAL")
    connection.execute("CREATE TABLE daily_reports(date TEXT PRIMARY KEY,completed_work TEXT NOT NULL,work_summary TEXT NOT NULL,next_plan TEXT NOT NULL,raw_json TEXT NOT NULL,created_at TEXT,updated_at TEXT)")
    report = DailyReportData(date="2026-10-09", completed_work="既有事实", work_summary="小结", next_plan="计划")
    connection.execute("INSERT INTO daily_reports VALUES (?,?,?,?,?,?,?)", (report.date, report.completed_work, report.work_summary, report.next_plan, report.model_dump_json(), "old", "old"))
    connection.commit()
    return connection


def test_migration_backs_up_wal_contents_and_is_idempotent(tmp_path):
    db_path = tmp_path / "reports.sqlite3"
    original = legacy_database(db_path)
    try:
        store = SQLiteStore(db_path)
        assert store.get_report("2026-10-09").completed_work == "既有事实"
        backups = list((tmp_path / "backups").glob("*.sqlite3"))
        assert len(backups) == 1
        with sqlite3.connect(backups[0]) as connection:
            assert connection.execute("SELECT completed_work FROM daily_reports").fetchone()[0] == "既有事实"
            assert connection.execute("PRAGMA integrity_check").fetchone()[0] == "ok"
            assert not connection.execute("SELECT name FROM sqlite_master WHERE name='report_runs'").fetchone()
        metadata = json.loads(backups[0].with_suffix(".json").read_text(encoding="utf-8"))
        assert metadata["backup_snapshot_sha256"] == sha256(backups[0].read_bytes()).hexdigest()
        SQLiteStore(db_path)
        assert len(list((tmp_path / "backups").glob("*.sqlite3"))) == 1
    finally:
        original.close()


def test_failed_migration_rolls_back_every_schema_change(monkeypatch, tmp_path):
    import src.services.sqlite_store as module

    db_path = tmp_path / "reports.sqlite3"
    legacy_database(db_path).close()
    original_migration = module.migrate_evidence_schema

    def fail(connection):
        original_migration(connection)
        raise RuntimeError("synthetic migration interruption")

    monkeypatch.setattr(module, "migrate_evidence_schema", fail)
    with pytest.raises(RuntimeError, match="synthetic"):
        SQLiteStore(db_path)
    with sqlite3.connect(db_path) as connection:
        assert [r[0] for r in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")] == ["daily_reports"]
        assert "schema_version" not in {r[1] for r in connection.execute("PRAGMA table_info(daily_reports)")}
        assert connection.execute("SELECT COUNT(*) FROM daily_reports").fetchone()[0] == 1
