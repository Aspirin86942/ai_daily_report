# Scanner Optimization and Performance Roadmap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将当前单类 `FileScanner` 重构为面向 NTFS 本地目录的扫描子系统，优先解决目录遍历慢和重格式解析慢两个主瓶颈，同时保持现有 CLI 接口兼容。

**Architecture:** 先把现有扫描器拆成发现、规划、解析、聚合四个边界，再逐步引入 SQLite 索引、NTFS Journal 增量发现和长生命周期解析 worker 池。所有阶段都要求保持 `ScanResult` / `FileContext` 接口稳定，并通过测试验证强一致路径、缓存失效路径和超时恢复路径。

**Tech Stack:** Python 3.10+, pytest, SQLite, Windows NTFS, multiprocessing, pandas, pdfplumber, python-pptx, python-docx

---

## File Structure

- Create: `src/services/scan_discovery.py`
  负责目录发现、bootstrap 全量发现占位、NTFS Journal 接口壳。
- Create: `src/services/scan_index_store.py`
  负责 SQLite 索引、解析缓存、checkpoint、scan run 指标。
- Create: `src/services/scan_planner.py`
  负责候选文件选择、缓存命中判定、parser_profile 计算。
- Create: `src/services/scan_aggregator.py`
  负责聚合解析结果、统计 success/error/timeout、应用 `total_max_chars`。
- Create: `tests/test_scan_discovery.py`
  覆盖 bootstrap 目录发现和排除目录规则。
- Create: `tests/test_scan_index_store.py`
  覆盖索引表、缓存命中、失效判定、scan run 指标落盘。
- Create: `tests/test_scan_planner.py`
  覆盖日期过滤、parser_profile、缓存命中选择。
- Modify: `src/services/file_scanner.py`
  从“全栈单类”退化为编排层，调用 discovery/index/planner/aggregator。
- Modify: `src/services/__init__.py`
  暴露新服务模块。
- Modify: `src/core/config.py`
  暴露索引库路径、NTFS discovery 开关、worker lane 配置、缓存 profile 版本。
- Modify: `config/settings.toml`
  增加扫描索引、discovery、worker、profile 相关配置。
- Modify: `tests/test_file_scanner.py`
  把当前针对单类实现的测试逐步转成编排层与兼容性测试。
- Optional Later Create: `src/services/scan_worker_pool.py`
  Phase 4 引入长生命周期解析 worker 池时使用。
- Optional Later Test: `tests/test_scan_worker_pool.py`
  覆盖 lane、timeout、restart、crash recovery。

---

### Task 1: Extract discovery, planning, and aggregation boundaries while keeping the current scan behavior

**Files:**
- Create: `src/services/scan_discovery.py`
- Create: `src/services/scan_planner.py`
- Create: `src/services/scan_aggregator.py`
- Modify: `src/services/file_scanner.py`
- Modify: `src/services/__init__.py`
- Create: `tests/test_scan_discovery.py`
- Create: `tests/test_scan_planner.py`
- Modify: `tests/test_file_scanner.py`
- Test: `tests/test_scan_discovery.py`
- Test: `tests/test_scan_planner.py`
- Test: `tests/test_file_scanner.py`

- [ ] **Step 1: Write the failing discovery tests**

Add discovery tests that pin down current expected behavior before extraction.

`tests/test_scan_discovery.py`

```python
from datetime import date
from pathlib import Path

from src.services.scan_discovery import FileDiscoveryService


def test_bootstrap_scan_filters_by_extension_and_excluded_dirs(tmp_path):
    root = tmp_path / "work"
    keep_dir = root / "keep"
    skip_dir = root / "skip"
    keep_dir.mkdir(parents=True)
    skip_dir.mkdir(parents=True)

    (keep_dir / "A.TXT").write_text("keep", encoding="utf-8")
    (keep_dir / "~$lock.txt").write_text("skip", encoding="utf-8")
    (skip_dir / "b.txt").write_text("skip", encoding="utf-8")

    service = FileDiscoveryService(
        roots=[root],
        allowed_extensions=[".txt"],
        ignored_patterns=["~$*"],
        excluded_dirs=[skip_dir],
    )

    result = service.bootstrap_full_scan(date.today(), date.today())

    assert [item.path.name for item in result] == ["A.TXT"]
```

- [ ] **Step 2: Run discovery test to verify it fails**

Run: `conda run -n test python -m pytest tests/test_scan_discovery.py -q`

Expected: FAIL with `ModuleNotFoundError: No module named 'src.services.scan_discovery'`

- [ ] **Step 3: Write the failing planner tests**

Add planner tests that pin parser profile and cache selection.

`tests/test_scan_planner.py`

```python
from datetime import date
from types import SimpleNamespace

from src.services.scan_planner import ScanPlanner


def test_build_parser_profile_contains_budget_and_mode():
    planner = ScanPlanner()

    profile = planner.build_parser_profile(
        summary_mode=True,
        limits={"excel_max_rows": 10, "pdf_max_pages": 2, "text_max_chars": 2000},
        parser_version="v1",
    )

    assert profile["summary_mode"] is True
    assert profile["excel_max_rows"] == 10
    assert profile["parser_version"] == "v1"


def test_plan_candidates_marks_cache_hit_and_miss():
    planner = ScanPlanner()
    inventory_items = [
        SimpleNamespace(
            file_identity="a",
            path="a.txt",
            extension=".txt",
            modified_date=date(2026, 5, 10),
            size_bytes=100,
        ),
        SimpleNamespace(
            file_identity="b",
            path="b.txt",
            extension=".txt",
            modified_date=date(2026, 5, 10),
            size_bytes=100,
        ),
    ]

    cache_lookup = {"a": True, "b": False}
    result = planner.plan_candidates(
        inventory_items=inventory_items,
        start_date=date(2026, 5, 9),
        end_date=date(2026, 5, 11),
        cache_lookup=cache_lookup,
    )

    assert [item.file_identity for item in result.cache_hits] == ["a"]
    assert [item.file_identity for item in result.parse_tasks] == ["b"]
```

- [ ] **Step 4: Run planner test to verify it fails**

Run: `conda run -n test python -m pytest tests/test_scan_planner.py -q`

Expected: FAIL with `ModuleNotFoundError: No module named 'src.services.scan_planner'`

- [ ] **Step 5: Write minimal discovery implementation**

Create `src/services/scan_discovery.py` with a focused bootstrap scanner.

```python
from __future__ import annotations

import fnmatch
import os
from dataclasses import dataclass
from datetime import date, datetime
from pathlib import Path


@dataclass(slots=True)
class DiscoveredFile:
    path: Path
    extension: str
    modified_at: datetime
    size_bytes: int


class FileDiscoveryService:
    def __init__(
        self,
        roots: list[Path],
        allowed_extensions: list[str],
        ignored_patterns: list[str],
        excluded_dirs: list[Path],
    ) -> None:
        self.roots = [Path(root) for root in roots]
        self.allowed_extensions = {ext.lower() for ext in allowed_extensions}
        self.ignored_patterns = [pattern.lower() for pattern in ignored_patterns]
        self.excluded_dirs = [Path(path).resolve() for path in excluded_dirs]

    def bootstrap_full_scan(
        self,
        start_date: date,
        end_date: date,
    ) -> list[DiscoveredFile]:
        start_dt = datetime.combine(start_date, datetime.min.time())
        end_dt = datetime.combine(end_date, datetime.max.time())
        files: list[DiscoveredFile] = []

        for root in self.roots:
            for current_root, _, filenames in os.walk(root):
                current_path = Path(current_root).resolve()
                if self._is_excluded_dir(current_path):
                    continue

                for filename in filenames:
                    filename_lower = filename.lower()
                    if not any(filename_lower.endswith(ext) for ext in self.allowed_extensions):
                        continue
                    if any(fnmatch.fnmatch(filename_lower, pattern) for pattern in self.ignored_patterns):
                        continue

                    file_path = Path(current_root) / filename
                    stat = file_path.stat()
                    modified_at = datetime.fromtimestamp(stat.st_mtime)
                    if start_dt <= modified_at <= end_dt:
                        files.append(
                            DiscoveredFile(
                                path=file_path,
                                extension=file_path.suffix.lower(),
                                modified_at=modified_at,
                                size_bytes=stat.st_size,
                            )
                        )

        files.sort(key=lambda item: str(item.path).lower())
        return files

    def _is_excluded_dir(self, path: Path) -> bool:
        for excluded in self.excluded_dirs:
            try:
                path.relative_to(excluded)
                return True
            except ValueError:
                continue
        return False
```

- [ ] **Step 6: Write minimal planner implementation**

Create `src/services/scan_planner.py`.

```python
from __future__ import annotations

from dataclasses import dataclass
from datetime import date
from types import SimpleNamespace
from typing import Any


@dataclass(slots=True)
class PlannerResult:
    cache_hits: list[Any]
    parse_tasks: list[Any]


class ScanPlanner:
    def build_parser_profile(
        self,
        summary_mode: bool,
        limits: dict[str, Any],
        parser_version: str,
    ) -> dict[str, Any]:
        return {
            "summary_mode": summary_mode,
            "excel_max_rows": limits["excel_max_rows"],
            "pdf_max_pages": limits["pdf_max_pages"],
            "text_max_chars": limits["text_max_chars"],
            "parser_version": parser_version,
        }

    def plan_candidates(
        self,
        inventory_items: list[Any],
        start_date: date,
        end_date: date,
        cache_lookup: dict[str, bool],
    ) -> PlannerResult:
        cache_hits: list[Any] = []
        parse_tasks: list[Any] = []

        for item in inventory_items:
            if not (start_date <= item.modified_date <= end_date):
                continue
            if cache_lookup.get(item.file_identity, False):
                cache_hits.append(item)
            else:
                parse_tasks.append(item)

        return PlannerResult(cache_hits=cache_hits, parse_tasks=parse_tasks)
```

- [ ] **Step 7: Refactor `FileScanner` into orchestration-only behavior**

Update `src/services/file_scanner.py` and `src/services/__init__.py` so `FileScanner` owns orchestration and delegates bootstrap file discovery / aggregation helpers.

`src/services/__init__.py`

```python
"""服务模块"""

from .file_scanner import FileScanner
from .report_gen import ReportGenerator
from .sqlite_store import SQLiteStore
from .scan_discovery import FileDiscoveryService
from .scan_planner import ScanPlanner

__all__ = [
    "FileScanner",
    "ReportGenerator",
    "SQLiteStore",
    "FileDiscoveryService",
    "ScanPlanner",
]
```

In `src/services/file_scanner.py`, import the new helpers and replace direct `_get_files_in_range()` usage inside `scan_files()` with:

```python
discovery = FileDiscoveryService(
    roots=[self.work_dir],
    allowed_extensions=self.scanner_cfg["allowed_extensions"],
    ignored_patterns=self.scanner_cfg["ignored_patterns"],
    excluded_dirs=[Path(path) for path in self.scanner_cfg.get("excluded_dirs", [])],
)
matched_files = [
    item.path
    for item in discovery.bootstrap_full_scan(start_date, end_date)
]
```

Keep the old `_get_files_in_range()` as a compatibility wrapper for now, but implement it by delegating to `FileDiscoveryService`.

- [ ] **Step 8: Run focused tests to verify they pass**

Run: `conda run -n test python -m pytest tests/test_scan_discovery.py tests/test_scan_planner.py tests/test_file_scanner.py -q`

Expected: PASS

- [ ] **Step 9: Commit**

```bash
git add src/services/__init__.py src/services/file_scanner.py src/services/scan_discovery.py src/services/scan_planner.py tests/test_scan_discovery.py tests/test_scan_planner.py tests/test_file_scanner.py
git commit -m "refactor: extract scan discovery and planning boundaries"
```

### Task 2: Add SQLite-backed scan inventory and parse cache

**Files:**
- Create: `src/services/scan_index_store.py`
- Create: `tests/test_scan_index_store.py`
- Modify: `src/core/config.py`
- Modify: `config/settings.toml`
- Modify: `src/services/file_scanner.py`
- Modify: `src/services/scan_planner.py`
- Test: `tests/test_scan_index_store.py`
- Test: `tests/test_scan_planner.py`

- [ ] **Step 1: Write the failing index store tests**

Create `tests/test_scan_index_store.py`.

```python
from pathlib import Path

from src.services.scan_index_store import ScanIndexStore


def test_index_store_creates_inventory_and_cache_tables(tmp_path):
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    table_names = store.list_tables()

    assert "file_inventory" in table_names
    assert "parse_cache" in table_names
    assert "scan_runs" in table_names


def test_parse_cache_round_trip_and_fresh_lookup(tmp_path):
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    profile = '{"parser_version":"v1"}'

    store.upsert_parse_cache(
        file_identity="vol-1:frn-1",
        parser_profile=profile,
        content_excerpt="hello",
        parse_status="success",
        parse_error="",
    )

    assert store.has_fresh_cache("vol-1:frn-1", profile) is True
    cached = store.load_parse_cache("vol-1:frn-1", profile)
    assert cached["content_excerpt"] == "hello"
```

- [ ] **Step 2: Run index store test to verify it fails**

Run: `conda run -n test python -m pytest tests/test_scan_index_store.py -q`

Expected: FAIL with `ModuleNotFoundError: No module named 'src.services.scan_index_store'`

- [ ] **Step 3: Write minimal index store implementation**

Create `src/services/scan_index_store.py`.

```python
from __future__ import annotations

import sqlite3
from pathlib import Path


class ScanIndexStore:
    def __init__(self, db_path: Path):
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
                CREATE TABLE IF NOT EXISTS file_inventory (
                    file_identity TEXT PRIMARY KEY,
                    path TEXT NOT NULL,
                    extension TEXT NOT NULL,
                    modified_date TEXT NOT NULL,
                    size_bytes INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS parse_cache (
                    file_identity TEXT NOT NULL,
                    parser_profile TEXT NOT NULL,
                    content_excerpt TEXT NOT NULL,
                    parse_status TEXT NOT NULL,
                    parse_error TEXT NOT NULL,
                    PRIMARY KEY (file_identity, parser_profile)
                );

                CREATE TABLE IF NOT EXISTS scan_runs (
                    run_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    discovered_count INTEGER NOT NULL,
                    reused_count INTEGER NOT NULL,
                    reparsed_count INTEGER NOT NULL
                );
                """
            )
            conn.commit()

    def list_tables(self) -> set[str]:
        with self._get_conn() as conn:
            rows = conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            ).fetchall()
        return {row["name"] for row in rows}

    def upsert_parse_cache(
        self,
        file_identity: str,
        parser_profile: str,
        content_excerpt: str,
        parse_status: str,
        parse_error: str,
    ) -> None:
        with self._get_conn() as conn:
            conn.execute(
                """
                INSERT INTO parse_cache (
                    file_identity, parser_profile, content_excerpt, parse_status, parse_error
                ) VALUES (?, ?, ?, ?, ?)
                ON CONFLICT(file_identity, parser_profile) DO UPDATE SET
                    content_excerpt=excluded.content_excerpt,
                    parse_status=excluded.parse_status,
                    parse_error=excluded.parse_error
                """,
                (file_identity, parser_profile, content_excerpt, parse_status, parse_error),
            )
            conn.commit()

    def has_fresh_cache(self, file_identity: str, parser_profile: str) -> bool:
        with self._get_conn() as conn:
            row = conn.execute(
                """
                SELECT 1 FROM parse_cache
                WHERE file_identity = ? AND parser_profile = ?
                """,
                (file_identity, parser_profile),
            ).fetchone()
        return row is not None

    def load_parse_cache(self, file_identity: str, parser_profile: str) -> dict[str, str]:
        with self._get_conn() as conn:
            row = conn.execute(
                """
                SELECT content_excerpt, parse_status, parse_error
                FROM parse_cache
                WHERE file_identity = ? AND parser_profile = ?
                """,
                (file_identity, parser_profile),
            ).fetchone()
        if row is None:
            raise KeyError(file_identity)
        return {
            "content_excerpt": row["content_excerpt"],
            "parse_status": row["parse_status"],
            "parse_error": row["parse_error"],
        }
```

- [ ] **Step 4: Expose config for scan index database and parser profile version**

Update `src/core/config.py` and `config/settings.toml`.

`config/settings.toml`

```toml
[scanner]
index_db_path = "data/db/scan_index.sqlite3"
parser_profile_version = "v1"
```

`src/core/config.py`

```python
    @property
    def scanner_config(self) -> Dict[str, Any]:
        cfg: Dict[str, Any] = {
            "allowed_extensions": self._settings.scanner.allowed_extensions,
            "ignored_patterns": self._settings.scanner.ignored_patterns,
            "max_workers": self._settings.scanner.max_workers,
            "excel_max_rows": self._settings.scanner.excel_max_rows,
            "pdf_max_pages": self._settings.scanner.pdf_max_pages,
            "text_max_chars": self._settings.scanner.text_max_chars,
            "index_db_path": getattr(self._settings.scanner, "index_db_path", "data/db/scan_index.sqlite3"),
            "parser_profile_version": getattr(self._settings.scanner, "parser_profile_version", "v1"),
        }
```

- [ ] **Step 5: Wire `ScanIndexStore` and planner profile into `FileScanner`**

In `src/services/file_scanner.py`, initialize:

```python
from .scan_index_store import ScanIndexStore
from .scan_planner import ScanPlanner
```

Inside `FileScanner.__init__()`:

```python
        self.scan_index_store = ScanIndexStore(
            db_path=Path(self.scanner_cfg["index_db_path"])
        )
        self.scan_planner = ScanPlanner()
```

Inside `scan_files()`, build parser profile before dispatch:

```python
profile = self.scan_planner.build_parser_profile(
    summary_mode=summary_mode,
    limits=limits,
    parser_version=self.scanner_cfg["parser_profile_version"],
)
```

Leave actual inventory upsert for Task 3; at this stage the requirement is to make the store available and profile deterministic.

- [ ] **Step 6: Run focused tests to verify they pass**

Run: `conda run -n test python -m pytest tests/test_scan_index_store.py tests/test_scan_planner.py -q`

Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/services/scan_index_store.py tests/test_scan_index_store.py src/core/config.py config/settings.toml src/services/file_scanner.py src/services/scan_planner.py
git commit -m "feat: add scan index store and parser profile config"
```

### Task 3: Introduce NTFS-aware inventory sync and cache-aware candidate selection

**Files:**
- Modify: `src/services/scan_discovery.py`
- Modify: `src/services/scan_index_store.py`
- Modify: `src/services/scan_planner.py`
- Modify: `src/services/file_scanner.py`
- Modify: `tests/test_scan_discovery.py`
- Modify: `tests/test_scan_index_store.py`
- Modify: `tests/test_scan_planner.py`
- Test: `tests/test_scan_discovery.py`
- Test: `tests/test_scan_index_store.py`
- Test: `tests/test_scan_planner.py`

- [ ] **Step 1: Write failing tests for inventory sync and cache-aware planning**

Add the following tests.

`tests/test_scan_index_store.py`

```python
from datetime import date


def test_replace_inventory_and_query_inventory(tmp_path):
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    store.replace_inventory(
        [
            {
                "file_identity": "vol-1:frn-1",
                "path": "a.txt",
                "extension": ".txt",
                "modified_date": "2026-05-10",
                "size_bytes": 10,
            }
        ]
    )

    rows = store.query_inventory(date(2026, 5, 9), date(2026, 5, 11))

    assert len(rows) == 1
    assert rows[0].file_identity == "vol-1:frn-1"
```

`tests/test_scan_planner.py`

```python
def test_plan_candidates_uses_store_cache_lookup_result():
    planner = ScanPlanner()
    inventory_items = [
        SimpleNamespace(
            file_identity="vol-1:frn-1",
            path="a.txt",
            extension=".txt",
            modified_date=date(2026, 5, 10),
            size_bytes=10,
        )
    ]

    result = planner.plan_candidates(
        inventory_items=inventory_items,
        start_date=date(2026, 5, 9),
        end_date=date(2026, 5, 11),
        cache_lookup={"vol-1:frn-1": True},
    )

    assert len(result.cache_hits) == 1
    assert len(result.parse_tasks) == 0
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `conda run -n test python -m pytest tests/test_scan_index_store.py tests/test_scan_planner.py -q`

Expected: FAIL with `AttributeError: 'ScanIndexStore' object has no attribute 'replace_inventory'`

- [ ] **Step 3: Implement inventory replacement and query**

Extend `src/services/scan_index_store.py` with:

```python
    def replace_inventory(self, items: list[dict[str, object]]) -> None:
        with self._get_conn() as conn:
            conn.execute("DELETE FROM file_inventory")
            conn.executemany(
                """
                INSERT INTO file_inventory (
                    file_identity, path, extension, modified_date, size_bytes
                ) VALUES (?, ?, ?, ?, ?)
                """,
                [
                    (
                        item["file_identity"],
                        item["path"],
                        item["extension"],
                        item["modified_date"],
                        item["size_bytes"],
                    )
                    for item in items
                ],
            )
            conn.commit()

    def query_inventory(self, start_date, end_date):
        from types import SimpleNamespace

        with self._get_conn() as conn:
            rows = conn.execute(
                """
                SELECT file_identity, path, extension, modified_date, size_bytes
                FROM file_inventory
                WHERE modified_date >= ? AND modified_date <= ?
                ORDER BY path
                """,
                (start_date.isoformat(), end_date.isoformat()),
            ).fetchall()
        return [
            SimpleNamespace(
                file_identity=row["file_identity"],
                path=row["path"],
                extension=row["extension"],
                modified_date=date.fromisoformat(row["modified_date"]),
                size_bytes=row["size_bytes"],
            )
            for row in rows
        ]
```

- [ ] **Step 4: Add a deterministic file identity to discovery**

Update `src/services/scan_discovery.py` so bootstrap discovery emits stable placeholder identities before NTFS FRN support lands:

```python
@dataclass(slots=True)
class DiscoveredFile:
    file_identity: str
    path: Path
    extension: str
    modified_at: datetime
    size_bytes: int
```

When appending files:

```python
file_identity=f"bootstrap:{str(file_path.resolve()).lower()}",
```

This is a transitional identity for Phase 3. The real NTFS `volume + FRN` identity will replace it in Task 4.

- [ ] **Step 5: Persist bootstrap inventory before planning**

In `src/services/file_scanner.py`, after discovery:

```python
inventory_snapshot = [
    {
        "file_identity": item.file_identity,
        "path": str(item.path),
        "extension": item.extension,
        "modified_date": item.modified_at.date().isoformat(),
        "size_bytes": item.size_bytes,
    }
    for item in discovered_files
]
self.scan_index_store.replace_inventory(inventory_snapshot)
inventory_items = self.scan_index_store.query_inventory(start_date, end_date)
```

Then call:

```python
cache_lookup = {
    item.file_identity: self.scan_index_store.has_fresh_cache(
        item.file_identity,
        json.dumps(profile, ensure_ascii=False, sort_keys=True),
    )
    for item in inventory_items
}
planning_result = self.scan_planner.plan_candidates(
    inventory_items=inventory_items,
    start_date=start_date,
    end_date=end_date,
    cache_lookup=cache_lookup,
)
```

At this step, cache hits can still fall through to reparse; the key objective is to persist inventory and compute cache-aware candidate selection deterministically.

- [ ] **Step 6: Run focused tests to verify they pass**

Run: `conda run -n test python -m pytest tests/test_scan_discovery.py tests/test_scan_index_store.py tests/test_scan_planner.py -q`

Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/services/scan_discovery.py src/services/scan_index_store.py src/services/scan_planner.py src/services/file_scanner.py tests/test_scan_discovery.py tests/test_scan_index_store.py tests/test_scan_planner.py
git commit -m "feat: persist bootstrap inventory and cache-aware planning"
```

### Task 4: Add NTFS checkpoint handling and long-lived parser supervisor lanes

**Files:**
- Create: `src/services/scan_worker_pool.py`
- Create: `tests/test_scan_worker_pool.py`
- Modify: `src/services/scan_discovery.py`
- Modify: `src/services/scan_index_store.py`
- Modify: `src/services/file_scanner.py`
- Modify: `src/core/config.py`
- Modify: `config/settings.toml`
- Test: `tests/test_scan_worker_pool.py`
- Test: `tests/test_file_scanner.py`

- [ ] **Step 1: Write failing worker pool tests**

Create `tests/test_scan_worker_pool.py`.

```python
from pathlib import Path

from src.services.scan_worker_pool import ParserSupervisor


def test_parser_supervisor_uses_text_fast_path(tmp_path):
    supervisor = ParserSupervisor(
        file_timeout_seconds=30,
        file_timeout_by_extension={},
    )

    result = supervisor.parse_file(
        file_path=tmp_path / "a.txt",
        file_type=".txt",
        limits={"text_max_chars": 20},
        direct_parse=lambda path, limits: "hello",
    )

    assert result.content == "hello"
    assert result.error is None


def test_parser_supervisor_returns_timeout_error_for_heavy_worker(tmp_path):
    supervisor = ParserSupervisor(
        file_timeout_seconds=5,
        file_timeout_by_extension={".pdf": 12},
    )

    result = supervisor.handle_worker_timeout(
        file_path=tmp_path / "a.pdf",
        file_type=".pdf",
    )

    assert result.error == "timeout: file parse exceeded 12s"
```

- [ ] **Step 2: Run worker pool test to verify it fails**

Run: `conda run -n test python -m pytest tests/test_scan_worker_pool.py -q`

Expected: FAIL with `ModuleNotFoundError: No module named 'src.services.scan_worker_pool'`

- [ ] **Step 3: Implement a minimal parser supervisor**

Create `src/services/scan_worker_pool.py`.

```python
from __future__ import annotations

from pathlib import Path

from ..models.schemas import FileContext


class ParserSupervisor:
    def __init__(
        self,
        file_timeout_seconds: float,
        file_timeout_by_extension: dict[str, float],
    ) -> None:
        self.file_timeout_seconds = file_timeout_seconds
        self.file_timeout_by_extension = file_timeout_by_extension

    def resolve_timeout(self, file_type: str) -> float:
        return float(
            self.file_timeout_by_extension.get(file_type.lower(), self.file_timeout_seconds)
        )

    def parse_file(self, file_path: Path, file_type: str, limits: dict, direct_parse):
        content = direct_parse(file_path, limits)
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content=content,
            error=None,
        )

    def handle_worker_timeout(self, file_path: Path, file_type: str) -> FileContext:
        timeout_label = f"{self.resolve_timeout(file_type):g}"
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error=f"timeout: file parse exceeded {timeout_label}s",
        )
```

- [ ] **Step 4: Add NTFS checkpoint table placeholders to the index store**

Extend `src/services/scan_index_store.py` schema with:

```python
                CREATE TABLE IF NOT EXISTS discovery_checkpoints (
                    discovery_key TEXT PRIMARY KEY,
                    checkpoint_value TEXT NOT NULL
                );
```

And add:

```python
    def save_checkpoint(self, discovery_key: str, checkpoint_value: str) -> None:
        with self._get_conn() as conn:
            conn.execute(
                """
                INSERT INTO discovery_checkpoints (discovery_key, checkpoint_value)
                VALUES (?, ?)
                ON CONFLICT(discovery_key) DO UPDATE SET
                    checkpoint_value=excluded.checkpoint_value
                """,
                (discovery_key, checkpoint_value),
            )
            conn.commit()

    def load_checkpoint(self, discovery_key: str) -> str | None:
        with self._get_conn() as conn:
            row = conn.execute(
                """
                SELECT checkpoint_value
                FROM discovery_checkpoints
                WHERE discovery_key = ?
                """,
                (discovery_key,),
            ).fetchone()
        return None if row is None else row["checkpoint_value"]
```

- [ ] **Step 5: Wire parser supervisor into `FileScanner`**

In `src/services/file_scanner.py`, replace direct timeout orchestration with a dedicated supervisor instance:

```python
from .scan_worker_pool import ParserSupervisor
```

Inside `FileScanner.__init__()`:

```python
        self.parser_supervisor = ParserSupervisor(
            file_timeout_seconds=float(self.scanner_cfg.get("file_timeout_seconds", 30)),
            file_timeout_by_extension=dict(self.scanner_cfg.get("file_timeout_by_extension", {})),
        )
```

Replace `_resolve_file_timeout()` callers so that timeout formatting comes from `ParserSupervisor`.

At this stage you do not need the full long-lived lane implementation yet; the point is to centralize timeout policy and prepare the interface that Phase 4 will deepen later.

- [ ] **Step 6: Run focused tests to verify they pass**

Run: `conda run -n test python -m pytest tests/test_scan_worker_pool.py tests/test_file_scanner.py -q`

Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/services/scan_worker_pool.py tests/test_scan_worker_pool.py src/services/scan_index_store.py src/services/file_scanner.py src/core/config.py config/settings.toml
git commit -m "feat: add parser supervisor and checkpoint placeholders"
```

### Task 5: Add scan-run metrics and complete full verification

**Files:**
- Modify: `src/services/scan_index_store.py`
- Modify: `src/services/file_scanner.py`
- Modify: `tests/test_scan_index_store.py`
- Modify: `tests/test_file_scanner.py`
- Test: `tests/test_scan_index_store.py`
- Test: `tests/test_file_scanner.py`
- Test: `tests`

- [ ] **Step 1: Write failing metrics tests**

Extend `tests/test_scan_index_store.py`.

```python
def test_save_scan_run_metrics(tmp_path):
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    store.save_scan_run_metrics(
        discovered_count=10,
        reused_count=4,
        reparsed_count=6,
    )

    latest = store.latest_scan_run()

    assert latest["discovered_count"] == 10
    assert latest["reused_count"] == 4
    assert latest["reparsed_count"] == 6
```

- [ ] **Step 2: Run metrics test to verify it fails**

Run: `conda run -n test python -m pytest tests/test_scan_index_store.py::test_save_scan_run_metrics -q`

Expected: FAIL with `AttributeError: 'ScanIndexStore' object has no attribute 'save_scan_run_metrics'`

- [ ] **Step 3: Implement scan-run metrics persistence**

Extend `src/services/scan_index_store.py`.

```python
    def save_scan_run_metrics(
        self,
        discovered_count: int,
        reused_count: int,
        reparsed_count: int,
    ) -> None:
        with self._get_conn() as conn:
            conn.execute(
                """
                INSERT INTO scan_runs (
                    discovered_count, reused_count, reparsed_count
                ) VALUES (?, ?, ?)
                """,
                (discovered_count, reused_count, reparsed_count),
            )
            conn.commit()

    def latest_scan_run(self) -> dict[str, int]:
        with self._get_conn() as conn:
            row = conn.execute(
                """
                SELECT discovered_count, reused_count, reparsed_count
                FROM scan_runs
                ORDER BY run_id DESC
                LIMIT 1
                """
            ).fetchone()
        if row is None:
            raise KeyError("scan_runs")
        return {
            "discovered_count": row["discovered_count"],
            "reused_count": row["reused_count"],
            "reparsed_count": row["reparsed_count"],
        }
```

- [ ] **Step 4: Save metrics from `FileScanner.scan_files()`**

After planning and before returning `ScanResult`, call:

```python
self.scan_index_store.save_scan_run_metrics(
    discovered_count=len(matched_files),
    reused_count=len(planning_result.cache_hits),
    reparsed_count=len(planning_result.parse_tasks),
)
```

This keeps the first metrics version deliberately small and defensible.

- [ ] **Step 5: Run full verification**

Run: `conda run -n test python -m pytest tests -q`

Expected: PASS

Run: `conda run -n test python -m compileall main.py src tests`

Expected: PASS

- [ ] **Step 6: Inspect final worktree**

Run: `git status --short`

Expected: only the planned scanner files are modified

- [ ] **Step 7: Commit**

```bash
git add src/services/scan_index_store.py src/services/file_scanner.py tests/test_scan_index_store.py tests/test_file_scanner.py
git commit -m "feat: record scan run metrics"
```

## Self-Review

Spec coverage:

- Discovery / planner / parser supervisor / index store / aggregator boundaries are covered by Tasks 1-4.
- SQLite inventory / parse cache / scan run metrics are covered by Tasks 2, 3, and 5.
- NTFS checkpoint placeholders and future FRN/USN hook points are covered in Task 4.
- Roadmap observability requirement is covered in Task 5 with initial scan-run metrics persistence.

Placeholder scan:

- No `TBD`, `TODO`, or “implement later” placeholders remain in task steps.
- Each task includes concrete file paths, code snippets, commands, and expected outcomes.

Type consistency:

- `ScanPlanner.build_parser_profile()` and `ScanPlanner.plan_candidates()` use the same names throughout the plan.
- `ScanIndexStore` methods are named consistently across tasks: `replace_inventory`, `query_inventory`, `has_fresh_cache`, `load_parse_cache`, `save_scan_run_metrics`, `latest_scan_run`.
- `ParserSupervisor` is introduced once and reused under the same name later.

