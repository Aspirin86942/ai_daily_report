"""JSON to SQLite migration utilities."""

from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Type, TypeVar

from ..core.config import config
from ..core.logger import setup_logger
from ..models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
from .sqlite_store import SQLiteStore

logger = setup_logger()

_T = TypeVar("_T", DailyReportData, WeeklyReportData, MonthlyReportData)

_DAILY_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
_WEEKLY_RE = re.compile(r"^\d{4}-W\d{2}$")
_MONTHLY_RE = re.compile(r"^\d{4}-\d{2}$")


@dataclass
class MigrationStats:
    """Migration counters grouped by report type."""

    daily_found: int = 0
    daily_migrated: int = 0
    daily_failed: int = 0
    weekly_found: int = 0
    weekly_migrated: int = 0
    weekly_failed: int = 0
    monthly_found: int = 0
    monthly_migrated: int = 0
    monthly_failed: int = 0

    @property
    def total_found(self) -> int:
        return self.daily_found + self.weekly_found + self.monthly_found

    @property
    def total_migrated(self) -> int:
        return self.daily_migrated + self.weekly_migrated + self.monthly_migrated

    @property
    def total_failed(self) -> int:
        return self.daily_failed + self.weekly_failed + self.monthly_failed


class JSONToSQLiteMigrator:
    """Migrate daily/weekly/monthly JSON report files into SQLite."""

    def __init__(
        self,
        json_db_dir: Path | None = None,
        sqlite_db_path: Path | None = None,
    ):
        self.json_db_dir = Path(json_db_dir or config.db_dir)
        self.sqlite_db_path = Path(sqlite_db_path or (config.db_dir / "reports.sqlite3"))
        self._store: SQLiteStore | None = None

    def migrate(self, dry_run: bool = False) -> MigrationStats:
        """Run migration and return migration stats."""
        stats = MigrationStats()

        self._migrate_daily(dry_run=dry_run, stats=stats)
        self._migrate_weekly(dry_run=dry_run, stats=stats)
        self._migrate_monthly(dry_run=dry_run, stats=stats)

        return stats

    def _get_store(self) -> SQLiteStore:
        if self._store is None:
            self._store = SQLiteStore(db_path=self.sqlite_db_path)
        return self._store

    def _migrate_daily(self, dry_run: bool, stats: MigrationStats) -> None:
        files = self._iter_json_files(self.json_db_dir, _DAILY_RE)
        for file_path in files:
            stats.daily_found += 1
            report = self._load_report(file_path, DailyReportData)
            if report is None:
                stats.daily_failed += 1
                continue
            if not dry_run:
                try:
                    self._get_store().save_report(report)
                except Exception as exc:
                    stats.daily_failed += 1
                    logger.warning("Failed to migrate daily file %s: %s", file_path, exc)
                    continue
            stats.daily_migrated += 1

    def _migrate_weekly(self, dry_run: bool, stats: MigrationStats) -> None:
        weekly_dir = self.json_db_dir / "weekly"
        files = self._iter_json_files(weekly_dir, _WEEKLY_RE)
        for file_path in files:
            stats.weekly_found += 1
            report = self._load_report(file_path, WeeklyReportData)
            if report is None:
                stats.weekly_failed += 1
                continue
            if not dry_run:
                try:
                    self._get_store().save_weekly_report(report)
                except Exception as exc:
                    stats.weekly_failed += 1
                    logger.warning("Failed to migrate weekly file %s: %s", file_path, exc)
                    continue
            stats.weekly_migrated += 1

    def _migrate_monthly(self, dry_run: bool, stats: MigrationStats) -> None:
        monthly_dir = self.json_db_dir / "monthly"
        files = self._iter_json_files(monthly_dir, _MONTHLY_RE)
        for file_path in files:
            stats.monthly_found += 1
            report = self._load_report(file_path, MonthlyReportData)
            if report is None:
                stats.monthly_failed += 1
                continue
            if not dry_run:
                try:
                    self._get_store().save_monthly_report(report)
                except Exception as exc:
                    stats.monthly_failed += 1
                    logger.warning("Failed to migrate monthly file %s: %s", file_path, exc)
                    continue
            stats.monthly_migrated += 1

    @staticmethod
    def _iter_json_files(base_dir: Path, stem_pattern: re.Pattern[str]) -> Iterable[Path]:
        if not base_dir.exists():
            return []
        return (
            file_path
            for file_path in sorted(base_dir.glob("*.json"))
            if stem_pattern.match(file_path.stem)
        )

    @staticmethod
    def _load_report(file_path: Path, model_cls: Type[_T]) -> _T | None:
        try:
            with open(file_path, "r", encoding="utf-8") as f:
                payload = json.load(f)
            return model_cls(**payload)
        except Exception as exc:
            logger.warning("Failed to parse %s as %s: %s", file_path, model_cls.__name__, exc)
            return None
