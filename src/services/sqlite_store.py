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

    This store is the single storage entry for report persistence.
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

    @staticmethod
    def _get_table_columns(conn: sqlite3.Connection, table_name: str) -> set[str]:
        return {
            row[1] for row in conn.execute(f"PRAGMA table_info({table_name})").fetchall()
        }

    @staticmethod
    def _is_legacy_weekly_schema(columns: set[str]) -> bool:
        return "overview" in columns and "self_growth" not in columns

    def _migrate_weekly_schema(self, conn: sqlite3.Connection) -> None:
        # 周报已从 overview 四段结构升级为七段正文；这里原地迁移列结构，
        # 同时保留 raw_json 作为真实来源，避免历史数据在迁移阶段被强制重写。
        conn.executescript(
            """
            ALTER TABLE weekly_reports RENAME TO weekly_reports_legacy;

            CREATE TABLE weekly_reports (
                week_label TEXT PRIMARY KEY,
                date_range TEXT NOT NULL,
                completed_work TEXT NOT NULL,
                self_growth TEXT NOT NULL,
                improvement_actions TEXT NOT NULL,
                work_summary TEXT NOT NULL,
                next_plan TEXT NOT NULL,
                support_needed TEXT NOT NULL,
                other_notes TEXT NOT NULL,
                raw_json TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            INSERT INTO weekly_reports (
                week_label,
                date_range,
                completed_work,
                self_growth,
                improvement_actions,
                work_summary,
                next_plan,
                support_needed,
                other_notes,
                raw_json,
                created_at,
                updated_at
            )
            SELECT
                week_label,
                date_range,
                completed_work,
                '',
                '',
                work_summary,
                next_plan,
                '',
                '',
                raw_json,
                created_at,
                updated_at
            FROM weekly_reports_legacy;

            DROP TABLE weekly_reports_legacy;
            """
        )

    @staticmethod
    def _load_weekly_report_payload(payload: dict, fallback_row: sqlite3.Row | None = None) -> WeeklyReportData:
        if "self_growth" in payload:
            return WeeklyReportData(**payload)

        overview = str(payload.get("overview") or "")
        completed_work = str(payload.get("completed_work") or "")
        merged_completed_work = (
            f"{overview}\n\n{completed_work}" if overview and completed_work else overview or completed_work
        )
        fallback = dict(fallback_row) if fallback_row is not None else {}

        # 旧版周报没有七段字段，这里按评审要求做保守映射：
        # 保留已有事实字段，把 overview 并入 completed_work，其余新增段落置空。
        return WeeklyReportData(
            week_label=str(payload.get("week_label") or fallback.get("week_label") or ""),
            date_range=str(payload.get("date_range") or fallback.get("date_range") or ""),
            completed_work=merged_completed_work,
            self_growth="",
            improvement_actions="",
            work_summary=str(payload.get("work_summary") or fallback.get("work_summary") or ""),
            next_plan=str(payload.get("next_plan") or fallback.get("next_plan") or ""),
            support_needed="",
            other_notes="",
        )

    def _init_schema(self) -> None:
        with self._get_conn() as conn:
            conn.executescript(
                """
                CREATE TABLE IF NOT EXISTS daily_reports (
                    date TEXT PRIMARY KEY,
                    completed_work TEXT NOT NULL,
                    work_summary TEXT NOT NULL,
                    next_plan TEXT NOT NULL,
                    raw_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS weekly_reports (
                    week_label TEXT PRIMARY KEY,
                    date_range TEXT NOT NULL,
                    completed_work TEXT NOT NULL,
                    self_growth TEXT NOT NULL,
                    improvement_actions TEXT NOT NULL,
                    work_summary TEXT NOT NULL,
                    next_plan TEXT NOT NULL,
                    support_needed TEXT NOT NULL,
                    other_notes TEXT NOT NULL,
                    raw_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS monthly_reports (
                    year_month TEXT PRIMARY KEY,
                    overview TEXT NOT NULL,
                    completed_work TEXT NOT NULL,
                    work_summary TEXT NOT NULL,
                    next_plan TEXT NOT NULL,
                    raw_json TEXT NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE INDEX IF NOT EXISTS idx_daily_reports_date ON daily_reports(date);
                """
            )
            conn.commit()

            weekly_columns = self._get_table_columns(conn, "weekly_reports")
            if self._is_legacy_weekly_schema(weekly_columns):
                self._migrate_weekly_schema(conn)
                conn.commit()

            # Verify schema is up-to-date to catch stale tables from prior versions
            cols = self._get_table_columns(conn, "daily_reports")
            required = {"completed_work", "work_summary", "next_plan"}
            if not required.issubset(cols):
                actual = self._get_table_columns(conn, "daily_reports")
                raise RuntimeError(
                    f"daily_reports schema is outdated (columns: {sorted(actual)}). "
                    "Create a backup of the existing SQLite database before you review the schema and rebuild the file if needed."
                )

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
                    date, completed_work, work_summary, next_plan,
                    raw_json, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, datetime('now'), datetime('now'))
                ON CONFLICT(date) DO UPDATE SET
                    completed_work=excluded.completed_work,
                    work_summary=excluded.work_summary,
                    next_plan=excluded.next_plan,
                    raw_json=excluded.raw_json,
                    updated_at=datetime('now')
                """,
                (
                    report.date,
                    report.completed_work,
                    report.work_summary,
                    report.next_plan,
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

    def get_yesterday_plan(self, target_date: Optional[datetime] = None) -> str:
        if target_date is None:
            target_date = datetime.now()
        yesterday = (target_date - timedelta(days=1)).strftime("%Y-%m-%d")
        report = self.get_report(yesterday)
        return report.next_plan if report else ""

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
                    week_label, date_range, completed_work, self_growth,
                    improvement_actions, work_summary, next_plan, support_needed,
                    other_notes, raw_json, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))
                ON CONFLICT(week_label) DO UPDATE SET
                    date_range=excluded.date_range,
                    completed_work=excluded.completed_work,
                    self_growth=excluded.self_growth,
                    improvement_actions=excluded.improvement_actions,
                    work_summary=excluded.work_summary,
                    next_plan=excluded.next_plan,
                    support_needed=excluded.support_needed,
                    other_notes=excluded.other_notes,
                    raw_json=excluded.raw_json,
                    updated_at=datetime('now')
                """,
                (
                    report.week_label,
                    report.date_range,
                    report.completed_work,
                    report.self_growth,
                    report.improvement_actions,
                    report.work_summary,
                    report.next_plan,
                    report.support_needed,
                    report.other_notes,
                    self._to_json(payload),
                ),
            )
            conn.commit()
        return self.db_path

    def get_weekly_report(self, week_label: str) -> Optional[WeeklyReportData]:
        with self._get_conn() as conn:
            row = conn.execute(
                """
                SELECT week_label, date_range, completed_work, work_summary, next_plan, raw_json
                FROM weekly_reports
                WHERE week_label = ?
                """,
                (week_label,),
            ).fetchone()
        if row is None:
            return None
        try:
            payload = json.loads(row["raw_json"])
            return self._load_weekly_report_payload(payload, row)
        except Exception as exc:
            logger.warning("Failed to parse weekly report %s: %s", week_label, exc)
            return None

    def save_monthly_report(self, report: MonthlyReportData) -> Path:
        payload = report.model_dump()
        with self._get_conn() as conn:
            conn.execute(
                """
                INSERT INTO monthly_reports (
                    year_month, overview, completed_work, work_summary, next_plan,
                    raw_json, created_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, datetime('now'), datetime('now'))
                ON CONFLICT(year_month) DO UPDATE SET
                    overview=excluded.overview,
                    completed_work=excluded.completed_work,
                    work_summary=excluded.work_summary,
                    next_plan=excluded.next_plan,
                    raw_json=excluded.raw_json,
                    updated_at=datetime('now')
                """,
                (
                    report.year_month,
                    report.overview,
                    report.completed_work,
                    report.work_summary,
                    report.next_plan,
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
