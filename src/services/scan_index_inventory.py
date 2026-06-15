"""SQLite file inventory helpers for scan index storage."""

from __future__ import annotations

import sqlite3
from datetime import date
from pathlib import Path

from .scan_index_models import InventoryItem


def replace_inventory(conn: sqlite3.Connection, items: list[dict[str, object]]) -> None:
    """用一次 bootstrap 快照整体替换当前库存。"""
    conn.execute("DELETE FROM file_inventory")
    conn.executemany(
        """
        INSERT INTO file_inventory (
            file_identity,
            path,
            extension,
            modified_date,
            size_bytes,
            source_version
        )
        VALUES (?, ?, ?, ?, ?, ?)
        """,
        [
            (
                str(item["file_identity"]),
                str(item["path"]),
                str(item["extension"]),
                _normalize_modified_date(item["modified_date"]),
                int(item["size_bytes"]),
                str(item.get("source_version", "")),
            )
            for item in items
        ],
    )


def query_inventory(
    conn: sqlite3.Connection,
    start_date: date,
    end_date: date,
) -> list[InventoryItem]:
    """按修改日期闭区间读取库存快照。"""
    rows = conn.execute(
        """
        SELECT
            file_identity,
            path,
            extension,
            modified_date,
            size_bytes,
            source_version
        FROM file_inventory
        WHERE modified_date >= ? AND modified_date <= ?
        ORDER BY path, file_identity
        """,
        (start_date.isoformat(), end_date.isoformat()),
    ).fetchall()

    return [
        InventoryItem(
            file_identity=str(row["file_identity"]),
            path=Path(str(row["path"])),
            extension=str(row["extension"]),
            modified_date=date.fromisoformat(str(row["modified_date"])),
            size_bytes=int(row["size_bytes"]),
            source_version=str(row["source_version"]),
        )
        for row in rows
    ]


def _normalize_modified_date(value: object) -> str:
    if isinstance(value, date):
        return value.isoformat()
    return str(value)
