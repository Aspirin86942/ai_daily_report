"""SQLite-based history storage layer."""

import json
import sqlite3
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import List, Optional, Tuple

from ..core.config import config
from ..core.logger import setup_logger
from ..models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData

logger = setup_logger()


class SQLiteStore:
    """History storage backed by SQLite.

    This store is used by ``HistoryManager`` as the default backend.
    """

    def __init__(self, db_path: Optional[Path] = None):
        if db_path is None:
            db_path = config.db_dir / "reports.sqlite3"
        self.db_path = Path(db_path)
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._init_schema()

    def _get_conn(self) -> sqlite3.Connection:
        conn = sqlite3.connect(self.db_path)
        conn.row_factory = sqlite3.Row
        return conn

    def _init_schema(self) -> None:
        with self._get_conn() as conn:
            conn.executescript(
                """
                CREATE TABLE IF NOT EXISTS daily_reports (
                    date TEXT PRIMARY KEY,
                    summary TEXT NOT NULL,
                    achievements_json TEXT NOT NULL,
                    risks_json TEXT NOT NULL,
                    plans_json TEXT NOT NULL,
                    yesterday_review TEXT,
                    raw_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS weekly_reports (
                    week_label TEXT PRIMARY KEY,
                    date_range TEXT NOT NULL,
                    summary TEXT NOT NULL,
                    category_summaries_json TEXT NOT NULL,
                    risks_json TEXT NOT NULL,
                    key_achievements_json TEXT NOT NULL,
                    next_week_plans_json TEXT NOT NULL,
                    missing_days_json TEXT NOT NULL,
                    data_source TEXT NOT NULL,
                    raw_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS monthly_reports (
                    year_month TEXT PRIMARY KEY,
                    summary TEXT NOT NULL,
                    category_summaries_json TEXT NOT NULL,
                    risks_json TEXT NOT NULL,
                    statistics_json TEXT NOT NULL,
                    key_achievements_json TEXT NOT NULL,
                    next_month_plans_json TEXT NOT NULL,
                    missing_days_json TEXT NOT NULL,
                    data_source TEXT NOT NULL,
                    raw_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE INDEX IF NOT EXISTS idx_daily_reports_date ON daily_reports(date);
                """
            )
            conn.commit()

    @staticmethod
    def _to_json(value) -> str:
        return json.dumps(value, ensure_ascii=False)

    @staticmethod
    def _from_json(raw: str, default):
        try:
            return json.loads(raw)
        except Exception:
            return default

    def save_report(self, report: DailyReportData) -> Path:
        payload = report.model_dump()
        with self._get_conn() as conn:
            conn.execute(
                """
                INSERT INTO daily_reports (
                    date, summary, achievements_json, risks_json, plans_json,
                    yesterday_review, raw_json, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))
                ON CONFLICT(date) DO UPDATE SET
                    summary=excluded.summary,
                    achievements_json=excluded.achievements_json,
                    risks_json=excluded.risks_json,
                    plans_json=excluded.plans_json,
                    yesterday_review=excluded.yesterday_review,
                    raw_json=excluded.raw_json,
                    updated_at=datetime('now')
                """,
                (
                    report.date,
                    report.summary,
                    self._to_json(payload.get("achievements", [])),
                    self._to_json(payload.get("risks", [])),
                    self._to_json(payload.get("plans", [])),
                    report.yesterday_review,
                    self._to_json(payload),
                ),
            )
            conn.commit()
        return self.db_path

    def get_report(self, report_date: str) -> Optional[DailyReportData]:
        with self._get_conn() as conn:
            row = conn.execute(
                "SELECT raw_json FROM daily_reports WHERE date = ?",
                (report_date,),
            ).fetchone()
        if row is None:
            return None

        try:
            return DailyReportData(**json.loads(row["raw_json"]))
        except Exception as exc:
            logger.warning("Failed to parse daily report %s: %s", report_date, exc)
            return None

    def get_yesterday_plan(self, target_date: Optional[datetime] = None) -> List[str]:
        if target_date is None:
            target_date = datetime.now()
        yesterday = (target_date - timedelta(days=1)).strftime("%Y-%m-%d")
        report = self.get_report(yesterday)
        return report.plans if report else []

    def get_month_reports(self, year_month: str) -> List[DailyReportData]:
        with self._get_conn() as conn:
            rows = conn.execute(
                "SELECT raw_json FROM daily_reports WHERE date LIKE ? ORDER BY date",
                (f"{year_month}-%",),
            ).fetchall()

        reports: List[DailyReportData] = []
        for row in rows:
            try:
                reports.append(DailyReportData(**json.loads(row["raw_json"])))
            except Exception as exc:
                logger.warning("Failed to parse daily report row: %s", exc)
        return reports

    def list_all_reports(self) -> List[str]:
        with self._get_conn() as conn:
            rows = conn.execute(
                "SELECT date FROM daily_reports ORDER BY date"
            ).fetchall()
        return [row["date"] for row in rows]

    def get_reports_in_range(
        self,
        start_date: date,
        end_date: date,
    ) -> Tuple[List[DailyReportData], List[str]]:
        with self._get_conn() as conn:
            rows = conn.execute(
                """
                SELECT date, raw_json
                FROM daily_reports
                WHERE date >= ? AND date <= ?
                ORDER BY date
                """,
                (start_date.isoformat(), end_date.isoformat()),
            ).fetchall()

        report_by_date: dict[str, DailyReportData] = {}
        for row in rows:
            try:
                parsed = DailyReportData(**json.loads(row["raw_json"]))
                report_by_date[row["date"]] = parsed
            except Exception as exc:
                logger.warning("Failed to parse report %s: %s", row["date"], exc)

        reports: List[DailyReportData] = []
        missing_dates: List[str] = []
        current = start_date
        while current <= end_date:
            if current.weekday() < 5:
                date_str = current.isoformat()
                if date_str in report_by_date:
                    reports.append(report_by_date[date_str])
                else:
                    missing_dates.append(date_str)
            current += timedelta(days=1)

        return reports, missing_dates

    def get_week_reports(
        self, year: int, week: int
    ) -> Tuple[List[DailyReportData], List[str]]:
        monday = date.fromisocalendar(year, week, 1)
        sunday = date.fromisocalendar(year, week, 7)
        return self.get_reports_in_range(monday, sunday)

    def save_weekly_report(self, report: WeeklyReportData) -> Path:
        payload = report.model_dump()
        with self._get_conn() as conn:
            conn.execute(
                """
                INSERT INTO weekly_reports (
                    week_label, date_range, summary, category_summaries_json,
                    risks_json, key_achievements_json, next_week_plans_json,
                    missing_days_json, data_source, raw_json, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))
                ON CONFLICT(week_label) DO UPDATE SET
                    date_range=excluded.date_range,
                    summary=excluded.summary,
                    category_summaries_json=excluded.category_summaries_json,
                    risks_json=excluded.risks_json,
                    key_achievements_json=excluded.key_achievements_json,
                    next_week_plans_json=excluded.next_week_plans_json,
                    missing_days_json=excluded.missing_days_json,
                    data_source=excluded.data_source,
                    raw_json=excluded.raw_json,
                    updated_at=datetime('now')
                """,
                (
                    report.week_label,
                    report.date_range,
                    report.summary,
                    self._to_json(payload.get("category_summaries", [])),
                    self._to_json(payload.get("risks", [])),
                    self._to_json(payload.get("key_achievements", [])),
                    self._to_json(payload.get("next_week_plans", [])),
                    self._to_json(payload.get("missing_days", [])),
                    report.data_source,
                    self._to_json(payload),
                ),
            )
            conn.commit()
        return self.db_path

    def get_weekly_report(self, week_label: str) -> Optional[WeeklyReportData]:
        with self._get_conn() as conn:
            row = conn.execute(
                "SELECT raw_json FROM weekly_reports WHERE week_label = ?",
                (week_label,),
            ).fetchone()
        if row is None:
            return None
        try:
            return WeeklyReportData(**json.loads(row["raw_json"]))
        except Exception as exc:
            logger.warning("Failed to parse weekly report %s: %s", week_label, exc)
            return None

    def save_monthly_report(self, report: MonthlyReportData) -> Path:
        payload = report.model_dump()
        with self._get_conn() as conn:
            conn.execute(
                """
                INSERT INTO monthly_reports (
                    year_month, summary, category_summaries_json, risks_json,
                    statistics_json, key_achievements_json, next_month_plans_json,
                    missing_days_json, data_source, raw_json, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))
                ON CONFLICT(year_month) DO UPDATE SET
                    summary=excluded.summary,
                    category_summaries_json=excluded.category_summaries_json,
                    risks_json=excluded.risks_json,
                    statistics_json=excluded.statistics_json,
                    key_achievements_json=excluded.key_achievements_json,
                    next_month_plans_json=excluded.next_month_plans_json,
                    missing_days_json=excluded.missing_days_json,
                    data_source=excluded.data_source,
                    raw_json=excluded.raw_json,
                    updated_at=datetime('now')
                """,
                (
                    report.year_month,
                    report.summary,
                    self._to_json(payload.get("category_summaries", [])),
                    self._to_json(payload.get("risks", [])),
                    self._to_json(payload.get("statistics", {})),
                    self._to_json(payload.get("key_achievements", [])),
                    self._to_json(payload.get("next_month_plans", [])),
                    self._to_json(payload.get("missing_days", [])),
                    report.data_source,
                    self._to_json(payload),
                ),
            )
            conn.commit()
        return self.db_path

    def get_monthly_report(self, year_month: str) -> Optional[MonthlyReportData]:
        with self._get_conn() as conn:
            row = conn.execute(
                "SELECT raw_json FROM monthly_reports WHERE year_month = ?",
                (year_month,),
            ).fetchone()
        if row is None:
            return None
        try:
            return MonthlyReportData(**json.loads(row["raw_json"]))
        except Exception as exc:
            logger.warning("Failed to parse monthly report %s: %s", year_month, exc)
            return None
