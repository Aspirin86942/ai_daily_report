"""Backward-compatible history manager backed by SQLite."""

from datetime import datetime
from pathlib import Path
from typing import Optional

from ..core.config import config
from .sqlite_store import SQLiteStore


class HistoryManager(SQLiteStore):
    """Compatibility facade over the SQLite storage backend.

    Existing callers can keep using ``HistoryManager`` while storage is now
    persisted in SQLite.
    """

    def __init__(
        self,
        db_dir: Optional[Path] = None,
        db_path: Optional[Path] = None,
    ):
        self.db_dir = Path(db_dir or config.db_dir)
        self.db_dir.mkdir(parents=True, exist_ok=True)

        sqlite_path = (
            Path(db_path) if db_path is not None else self.db_dir / "reports.sqlite3"
        )
        super().__init__(db_path=sqlite_path)

    def get_yesterday_plan(self, target_date: Optional[datetime] = None) -> str:
        # 兼容层保持与 SQLiteStore 一致：返回昨日日报中的 next_plan 文本。
        return super().get_yesterday_plan(target_date=target_date)
