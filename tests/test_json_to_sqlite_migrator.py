"""Tests for JSON to SQLite migration."""

from __future__ import annotations

import json

from src.models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
from src.services.json_to_sqlite_migrator import JSONToSQLiteMigrator
from src.services.sqlite_store import SQLiteStore


def _daily_payload(report_date: str = "2026-02-09") -> dict:
    report = DailyReportData(
        date=report_date,
        completed_work="今天完成 JSON 到 SQLite 的文本化迁移。",
        work_summary="重点处理 schema 收敛。",
        next_plan="明天继续补齐历史兼容验证。",
    )
    return report.model_dump()


def _weekly_payload() -> dict:
    report = WeeklyReportData(
        week_label="2026-W06",
        date_range="2026-02-02 ~ 2026-02-08",
        overview="本周围绕迁移改造推进。",
        completed_work="完成周报文本结构迁移。",
        work_summary="处理了旧 JSON payload 到新模型的映射。",
        next_plan="下周验证迁移回归。",
    )
    return report.model_dump()


def _monthly_payload() -> dict:
    report = MonthlyReportData(
        year_month="2026-02",
        overview="本月集中推进历史数据迁移。",
        completed_work="完成月报文本字段迁移。",
        work_summary="确保迁移后模型可正常校验和落库。",
        next_plan="下月继续观察稳定性。",
    )
    return report.model_dump()


def test_migrate_all_report_types(tmp_path):
    json_db = tmp_path / "db"
    json_db.mkdir(parents=True, exist_ok=True)
    (json_db / "weekly").mkdir(parents=True, exist_ok=True)
    (json_db / "monthly").mkdir(parents=True, exist_ok=True)

    (json_db / "2026-02-09.json").write_text(
        json.dumps(_daily_payload(), ensure_ascii=False),
        encoding="utf-8",
    )
    (json_db / "weekly" / "2026-W06.json").write_text(
        json.dumps(_weekly_payload(), ensure_ascii=False),
        encoding="utf-8",
    )
    (json_db / "monthly" / "2026-02.json").write_text(
        json.dumps(_monthly_payload(), ensure_ascii=False),
        encoding="utf-8",
    )

    sqlite_path = tmp_path / "reports.sqlite3"
    migrator = JSONToSQLiteMigrator(json_db_dir=json_db, sqlite_db_path=sqlite_path)
    stats = migrator.migrate()

    assert stats.daily_found == 1
    assert stats.daily_migrated == 1
    assert stats.weekly_found == 1
    assert stats.weekly_migrated == 1
    assert stats.monthly_found == 1
    assert stats.monthly_migrated == 1
    assert stats.total_failed == 0

    store = SQLiteStore(db_path=sqlite_path)
    assert store.get_report("2026-02-09") is not None
    assert store.get_weekly_report("2026-W06") is not None
    assert store.get_monthly_report("2026-02") is not None


def test_migrate_skips_invalid_files(tmp_path):
    json_db = tmp_path / "db"
    json_db.mkdir(parents=True, exist_ok=True)

    (json_db / "2026-02-10.json").write_text(
        json.dumps(_daily_payload("2026-02-10"), ensure_ascii=False),
        encoding="utf-8",
    )
    (json_db / "2026-02-11.json").write_text("{invalid-json", encoding="utf-8")

    sqlite_path = tmp_path / "reports.sqlite3"
    migrator = JSONToSQLiteMigrator(json_db_dir=json_db, sqlite_db_path=sqlite_path)
    stats = migrator.migrate()

    assert stats.daily_found == 2
    assert stats.daily_migrated == 1
    assert stats.daily_failed == 1

    store = SQLiteStore(db_path=sqlite_path)
    assert store.get_report("2026-02-10") is not None
    assert store.get_report("2026-02-11") is None


def test_dry_run_does_not_write_sqlite(tmp_path):
    json_db = tmp_path / "db"
    json_db.mkdir(parents=True, exist_ok=True)
    (json_db / "2026-02-09.json").write_text(
        json.dumps(_daily_payload(), ensure_ascii=False),
        encoding="utf-8",
    )

    sqlite_path = tmp_path / "reports.sqlite3"
    migrator = JSONToSQLiteMigrator(json_db_dir=json_db, sqlite_db_path=sqlite_path)
    stats = migrator.migrate(dry_run=True)

    assert stats.daily_found == 1
    assert stats.daily_migrated == 1
    assert stats.daily_failed == 0
    assert not sqlite_path.exists()
