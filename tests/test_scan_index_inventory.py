import sqlite3
from datetime import date
from pathlib import Path

from src.services.scan_index_inventory import query_inventory, replace_inventory
from src.services.scan_index_schema import init_scan_index_schema
from src.services.scan_index_models import InventoryItem


def _connect(db_path: Path) -> sqlite3.Connection:
    conn = sqlite3.connect(db_path)
    conn.execute("PRAGMA foreign_keys = ON")
    conn.row_factory = sqlite3.Row
    init_scan_index_schema(conn)
    return conn


def test_replace_inventory_replaces_snapshot_and_normalizes_values(tmp_path: Path):
    db_path = tmp_path / "scan_index.sqlite3"
    with _connect(db_path) as conn:
        replace_inventory(
            conn,
            [
                {
                    "file_identity": "old",
                    "path": "/work/old.txt",
                    "extension": ".txt",
                    "modified_date": "2026-05-01",
                    "size_bytes": 1,
                    "source_version": "mtime=1:size=1",
                }
            ],
        )
        replace_inventory(
            conn,
            [
                {
                    "file_identity": "new",
                    "path": Path("/work/new.md"),
                    "extension": ".md",
                    "modified_date": date(2026, 5, 2),
                    "size_bytes": "2",
                }
            ],
        )

        rows = conn.execute(
            """
            SELECT file_identity, path, extension, modified_date, size_bytes, source_version
            FROM file_inventory
            ORDER BY path
            """
        ).fetchall()

    assert [dict(row) for row in rows] == [
        {
            "file_identity": "new",
            "path": "/work/new.md",
            "extension": ".md",
            "modified_date": "2026-05-02",
            "size_bytes": 2,
            "source_version": "",
        }
    ]


def test_query_inventory_returns_typed_items_in_stable_order(tmp_path: Path):
    db_path = tmp_path / "scan_index.sqlite3"
    with _connect(db_path) as conn:
        replace_inventory(
            conn,
            [
                {
                    "file_identity": "b",
                    "path": "/work/b.txt",
                    "extension": ".txt",
                    "modified_date": "2026-05-02",
                    "size_bytes": 2,
                    "source_version": "mtime=2:size=2",
                },
                {
                    "file_identity": "a",
                    "path": "/work/a.txt",
                    "extension": ".txt",
                    "modified_date": "2026-05-02",
                    "size_bytes": 1,
                    "source_version": "mtime=1:size=1",
                },
                {
                    "file_identity": "old",
                    "path": "/work/old.txt",
                    "extension": ".txt",
                    "modified_date": "2026-05-01",
                    "size_bytes": 9,
                    "source_version": "mtime=9:size=9",
                },
            ],
        )

        items = query_inventory(conn, date(2026, 5, 2), date(2026, 5, 2))

    assert items == [
        InventoryItem(
            file_identity="a",
            path=Path("/work/a.txt"),
            extension=".txt",
            modified_date=date(2026, 5, 2),
            size_bytes=1,
            source_version="mtime=1:size=1",
        ),
        InventoryItem(
            file_identity="b",
            path=Path("/work/b.txt"),
            extension=".txt",
            modified_date=date(2026, 5, 2),
            size_bytes=2,
            source_version="mtime=2:size=2",
        ),
    ]
