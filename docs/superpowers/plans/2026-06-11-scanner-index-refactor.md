# Scanner Index Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 收尾当前 scanner/index 拆分 WIP，让 scan index schema/inventory/models 与 scanner item/parse-cache helper 边界清楚，并用测试证明行为不变。

**Architecture:** 保留 `ScanIndexStore` 作为对外 facade；把 schema/migration、inventory SQL、共享 dataclass、scanner item 适配、parse-cache/reparse audit helper 放在独立小模块中。`FileScanner` 保留兼容 wrapper，内部委托给 helper，parser backend、worker lane、Office fallback、source version、cache freshness 语义不变。

**Tech Stack:** Python 3.10+、SQLite、pytest、现有 Rust CLI contract 测试；优先 `conda run -n test python -m pytest`，不可用时用 `python3 -m pytest`。

---

## File Structure

- Modify: `src/services/scan_index_models.py`
  - 只定义 `InventoryItem` 与 `CacheProbe`。
  - 不依赖 store、scanner、SQL helper。
- Modify: `src/services/scan_index_schema.py`
  - 只负责 schema 初始化、migration、表/列/主键 introspection helper。
- Modify: `src/services/scan_index_inventory.py`
  - 只负责 `file_inventory` 的 replace/query。
- Modify: `src/services/scan_index_store.py`
  - 继续作为 facade；管理 SQLite 连接、foreign keys、schema init、scan metrics、context audit、parse cache facade。
  - 委托 inventory 查询/替换给 `scan_index_inventory.py`。
- Modify: `src/services/scanner_items.py`
  - 负责 `Path | InventoryItem` 与 `DiscoveredFile` 的适配。
  - 必须从 `scan_index_models` 导入 `InventoryItem`。
- Modify: `src/services/scanner_parse_cache.py`
  - 负责 cache context 恢复、cache 写入、reparse audit 构造。
  - 必须从 `scan_index_models` 导入 `CacheProbe`，并用窄 `Protocol` 描述 store 方法。
- Modify: `src/services/file_scanner.py`
  - 保留 wrapper 方法，委托到 helper。
  - 导入 `InventoryItem` 应来自 `scan_index_models`。
- Modify: `src/services/cold_scanner_run.py`
  - 保留 `assert` 到显式 `RuntimeError` 的运行时错误处理改动。
- Modify: `tests/test_scanner_items.py`
  - 导入 `InventoryItem` 应来自 `scan_index_models`，并验证 Path 兼容规则。
- Modify: `tests/test_scanner_parse_cache.py`
  - 导入 `CacheProbe`、`InventoryItem` 应来自 `scan_index_models`，并补充失败 cache 恢复测试。
- Modify: `tests/test_scan_index_inventory.py`
  - 验证 inventory replace/query 语义。
- Modify: `tests/test_cold_scanner_run.py`
  - 验证处理数量不匹配时抛 `RuntimeError`，不依赖 `assert`。

---

## Task 1: Tighten model imports and scanner item boundaries

**Files:**
- Modify: `src/services/scanner_items.py`
- Modify: `src/services/file_scanner.py`
- Modify: `tests/test_scanner_items.py`

- [ ] **Step 1: Update the scanner item test import**

Change the top imports in `tests/test_scanner_items.py` so the model source is explicit:

```python
from datetime import datetime
from pathlib import Path

from src.services.scan_discovery import DiscoveredFile
from src.services.scan_index_models import InventoryItem
from src.services.scanner_items import (
    item_extension,
    item_identity,
    item_path,
    item_source_version,
    normalize_discovered_files,
)
```

- [ ] **Step 2: Run scanner item tests and verify current behavior**

Run:

```bash
conda run -n test python -m pytest tests/test_scanner_items.py -v
```

If the conda environment is unavailable, run:

```bash
python3 -m pytest tests/test_scanner_items.py -v
```

Expected after Step 1 and before implementation cleanup: tests may fail only if `scanner_items.py` still imports the model through the store facade or an import cycle is exposed. Any failure should mention import resolution or still pass if Python re-export currently masks it.

- [ ] **Step 3: Change `scanner_items.py` to depend on the model module directly**

In `src/services/scanner_items.py`, replace the store import with the model import:

```python
from .scan_discovery import DiscoveredFile
from .scan_index_models import InventoryItem
```

Keep the `ScannerItem` alias and helper implementations unchanged:

```python
ScannerItem = Path | InventoryItem
```

- [ ] **Step 4: Change `file_scanner.py` imports to separate facade from model**

In `src/services/file_scanner.py`, replace this style of import:

```python
from .scan_index_store import InventoryItem, ScanIndexStore
```

with:

```python
from .scan_index_models import InventoryItem
from .scan_index_store import ScanIndexStore
```

Do not change method signatures or parser selection logic.

- [ ] **Step 5: Run scanner item tests again**

Run:

```bash
conda run -n test python -m pytest tests/test_scanner_items.py -v
```

Fallback:

```bash
python3 -m pytest tests/test_scanner_items.py -v
```

Expected: all tests in `tests/test_scanner_items.py` pass.

- [ ] **Step 6: Commit Task 1**

```bash
git add src/services/scanner_items.py src/services/file_scanner.py tests/test_scanner_items.py
git commit -m "Refine scanner item model imports"
```

---

## Task 2: Narrow scanner parse-cache helper dependencies

**Files:**
- Modify: `src/services/scanner_parse_cache.py`
- Modify: `tests/test_scanner_parse_cache.py`

- [ ] **Step 1: Update parse-cache test imports**

Change the model imports in `tests/test_scanner_parse_cache.py` to come from `scan_index_models`:

```python
from datetime import date
from pathlib import Path

from src.models.schemas import FileContext
from src.services.office_parser import OfficeParseAudit
from src.services.scan_index_models import CacheProbe, InventoryItem
from src.services.scanner_parse_cache import (
    build_reparse_detail,
    build_reparse_exception_detail,
    get_cached_contexts,
    write_parse_cache,
)
```

- [ ] **Step 2: Add a regression test for cached error restoration**

Append this test to `tests/test_scanner_parse_cache.py`:

```python
def test_get_cached_contexts_restores_error_context_without_content():
    item = _inventory_item(Path("/work/bad.md"))
    store = FakeParseCacheStore(
        {
            (item.file_identity, "profile-key", item.source_version): {
                "content_excerpt": "",
                "parse_status": "error",
                "parse_error": "parse failed",
                "parser_backend": "not_parsed",
                "truncated": 0,
            }
        }
    )

    [context] = get_cached_contexts(store, [item], "profile-key")

    assert context == FileContext(
        file_path="/work/bad.md",
        file_type=".md",
        content="",
        error="parse failed",
        parser_backend="not_parsed",
        truncated=False,
    )
```

This locks the existing behavior that failed parse results restore `FileContext.error` and do not restore stale content.

- [ ] **Step 3: Run the new parse-cache test before helper cleanup**

Run:

```bash
conda run -n test python -m pytest tests/test_scanner_parse_cache.py::test_get_cached_contexts_restores_error_context_without_content -v
```

Fallback:

```bash
python3 -m pytest tests/test_scanner_parse_cache.py::test_get_cached_contexts_restores_error_context_without_content -v
```

Expected: pass if current helper behavior is already correct; fail only if error cache restoration regressed.

- [ ] **Step 4: Add a narrow store protocol and direct model import**

In `src/services/scanner_parse_cache.py`, replace:

```python
from collections.abc import Callable, Mapping
from typing import Any
```

with:

```python
from collections.abc import Callable, Mapping
from typing import Protocol
```

Replace:

```python
from .scan_index_store import CacheProbe
```

with:

```python
from .scan_index_models import CacheProbe
```

Add this protocol after `InferWorkerLane`:

```python
class ParseCacheStore(Protocol):
    """Narrow interface scanner cache helpers need from ScanIndexStore."""

    def load_parse_cache(
        self,
        file_identity: str,
        parser_profile: str,
        source_version: str = "",
    ) -> dict[str, object]:
        """Load one parse-cache row by identity, parser profile, and source version."""

    def upsert_parse_cache(self, **kwargs: object) -> None:
        """Persist one parse-cache row."""
```

Change the helper signatures from `Any` to `ParseCacheStore`:

```python
def get_cached_contexts(
    scan_index_store: ParseCacheStore,
    cached_files: list[ScannerItem],
    parser_profile: str,
) -> list[FileContext]:
```

```python
def write_parse_cache(
    scan_index_store: ParseCacheStore,
    item: ScannerItem,
    parser_profile: str,
    context: FileContext,
) -> None:
```

- [ ] **Step 5: Run all parse-cache helper tests**

Run:

```bash
conda run -n test python -m pytest tests/test_scanner_parse_cache.py -v
```

Fallback:

```bash
python3 -m pytest tests/test_scanner_parse_cache.py -v
```

Expected: all tests in `tests/test_scanner_parse_cache.py` pass.

- [ ] **Step 6: Commit Task 2**

```bash
git add src/services/scanner_parse_cache.py tests/test_scanner_parse_cache.py
git commit -m "Narrow scanner parse cache helper dependencies"
```

---

## Task 3: Clean ScanIndexStore facade imports and inventory delegation

**Files:**
- Modify: `src/services/scan_index_store.py`
- Modify: `tests/test_scan_index_inventory.py`
- Modify: `tests/test_scan_index_store.py`

- [ ] **Step 1: Normalize `scan_index_store.py` imports**

At the top of `src/services/scan_index_store.py`, keep imports grouped like this:

```python
from __future__ import annotations

import sqlite3
from datetime import date
from pathlib import Path

from .context_compressor import ContextDecision
from .scan_index_inventory import (
    query_inventory as query_file_inventory,
    replace_inventory as replace_file_inventory,
)
from .scan_index_models import CacheProbe, InventoryItem
from .scan_index_schema import init_scan_index_schema, list_table_names
from .scan_metrics import ExtensionMetrics, ScanRunMetrics
```

There should be one blank line between import groups and one blank line before `class ScanIndexStore:`.

- [ ] **Step 2: Verify facade methods still delegate inventory operations**

Ensure the bottom of `src/services/scan_index_store.py` contains these methods:

```python
    def replace_inventory(self, items: list[dict[str, object]]) -> None:
        """用一次 bootstrap 快照整体替换当前库存。"""
        with self._connect() as conn:
            replace_file_inventory(conn, items)

    def query_inventory(
        self,
        start_date: date,
        end_date: date,
    ) -> list[InventoryItem]:
        """按修改日期闭区间读取库存快照。"""
        with self._connect() as conn:
            return query_file_inventory(conn, start_date, end_date)
```

Do not move parse-cache SQL in this task.

- [ ] **Step 3: Run inventory helper tests**

Run:

```bash
conda run -n test python -m pytest tests/test_scan_index_inventory.py -v
```

Fallback:

```bash
python3 -m pytest tests/test_scan_index_inventory.py -v
```

Expected: all tests in `tests/test_scan_index_inventory.py` pass.

- [ ] **Step 4: Run ScanIndexStore compatibility tests**

Run:

```bash
conda run -n test python -m pytest tests/test_scan_index_store.py -v
```

Fallback:

```bash
python3 -m pytest tests/test_scan_index_store.py -v
```

Expected: all tests in `tests/test_scan_index_store.py` pass. If a test imports `InventoryItem` or `CacheProbe` from `scan_index_store.py`, it may continue to pass because `scan_index_store.py` imports those names; do not add explicit re-export code unless tests or callers require it.

- [ ] **Step 5: Commit Task 3**

```bash
git add src/services/scan_index_store.py tests/test_scan_index_inventory.py tests/test_scan_index_store.py
git commit -m "Delegate scan index inventory storage"
```

---

## Task 4: Lock ColdScannerRun runtime mismatch behavior

**Files:**
- Modify: `src/services/cold_scanner_run.py`
- Modify: `tests/test_cold_scanner_run.py`

- [ ] **Step 1: Verify or add the RuntimeError regression test**

Ensure `tests/test_cold_scanner_run.py` contains a test equivalent to this behavior. Use existing local test helpers in that file if they already create a scanner/run fixture; the assertion must be exact enough to prove `assert` is not the mechanism:

```python
import pytest


def test_scan_files_raises_runtime_error_when_processed_count_mismatches(monkeypatch):
    scanner = _make_scanner_for_cold_run(monkeypatch)
    run = ColdScannerRun(scanner)

    monkeypatch.setattr(
        run,
        "_plan_candidates",
        lambda *args, **kwargs: {
            "cached_files": [],
            "files_to_parse": [],
            "total_candidates": 1,
            "parser_profile": "profile-key",
        },
    )

    with pytest.raises(RuntimeError, match="文件处理数量不匹配: processed=0, expected=1"):
        run.scan_files(start_date=date(2026, 6, 10), end_date=date(2026, 6, 11))
```

If the file already has a differently named fixture/helper, adapt only the helper calls; keep the expected exception type and message.

- [ ] **Step 2: Run the targeted ColdScannerRun test**

Run:

```bash
conda run -n test python -m pytest tests/test_cold_scanner_run.py -k "processed_count or mismatches or mismatch" -v
```

Fallback:

```bash
python3 -m pytest tests/test_cold_scanner_run.py -k "processed_count or mismatches or mismatch" -v
```

Expected: the targeted test passes and failure mode is `RuntimeError`, not `AssertionError`.

- [ ] **Step 3: Ensure implementation uses explicit RuntimeError**

In `src/services/cold_scanner_run.py`, the aggregation guard should be:

```python
            processed_count = aggregator.success_count + aggregator.error_count
            expected_count = planned_candidates["total_candidates"]
            if processed_count != expected_count:
                raise RuntimeError(
                    "文件处理数量不匹配: "
                    f"processed={processed_count}, expected={expected_count}"
                )
```

Do not change aggregation success/error counting in this task.

- [ ] **Step 4: Run ColdScannerRun tests**

Run:

```bash
conda run -n test python -m pytest tests/test_cold_scanner_run.py -v
```

Fallback:

```bash
python3 -m pytest tests/test_cold_scanner_run.py -v
```

Expected: all tests in `tests/test_cold_scanner_run.py` pass.

- [ ] **Step 5: Commit Task 4**

```bash
git add src/services/cold_scanner_run.py tests/test_cold_scanner_run.py
git commit -m "Raise scanner aggregation mismatch explicitly"
```

---

## Task 5: Run scanner/index integration verification

**Files:**
- Verify only unless failures require a minimal fix within files already listed above.

- [ ] **Step 1: Run focused helper and store tests**

Run:

```bash
conda run -n test python -m pytest \
  tests/test_scan_index_inventory.py \
  tests/test_scanner_items.py \
  tests/test_scanner_parse_cache.py \
  tests/test_scan_index_store.py \
  -v
```

Fallback:

```bash
python3 -m pytest \
  tests/test_scan_index_inventory.py \
  tests/test_scanner_items.py \
  tests/test_scanner_parse_cache.py \
  tests/test_scan_index_store.py \
  -v
```

Expected: all selected tests pass.

- [ ] **Step 2: Run scanner main-chain tests**

Run:

```bash
conda run -n test python -m pytest \
  tests/test_file_scanner.py \
  tests/test_cold_scanner_run.py \
  tests/test_scan_planner.py \
  -v
```

Fallback:

```bash
python3 -m pytest \
  tests/test_file_scanner.py \
  tests/test_cold_scanner_run.py \
  tests/test_scan_planner.py \
  -v
```

Expected: all selected tests pass.

- [ ] **Step 3: Run backend invariant tests**

Run:

```bash
conda run -n test python -m pytest \
  tests/test_office_parser.py \
  tests/test_rust_cli_contract.py \
  tests/test_rust_discovery_contract.py \
  tests/test_schemas.py \
  -v
```

Fallback:

```bash
python3 -m pytest \
  tests/test_office_parser.py \
  tests/test_rust_cli_contract.py \
  tests/test_rust_discovery_contract.py \
  tests/test_schemas.py \
  -v
```

Expected: all selected tests pass and no evidence field collapses `parser_backend` with `worker_lane`.

- [ ] **Step 4: Run full Python test suite**

Run:

```bash
conda run -n test python -m pytest tests/ -v
```

Fallback:

```bash
python3 -m pytest tests/ -v
```

Expected: full test suite passes. If failures are unrelated to scanner/index WIP, record the exact failing tests and do not expand scope without user approval.

- [ ] **Step 5: Inspect final diff scope**

Run:

```bash
git status --short
git diff --stat
git diff --check
```

Expected:

- `git diff --check` exits 0.
- Diff is limited to scanner/index WIP, related tests, and already-present Rust/parser WIP files if they are required by existing worktree state.
- No secrets or local config files are staged.

---

## Task 6: Final commit and handoff summary

**Files:**
- Stage only files confirmed by Task 5.

- [ ] **Step 1: Review files to stage**

Run:

```bash
git status --short
```

Expected: modified/untracked files match scanner/index refactor scope. Do not stage `config/.secrets.yaml`, generated reports, logs, cache directories, or local benchmark outputs.

- [ ] **Step 2: Stage implementation files**

Run:

```bash
git add \
  src/services/scan_index_models.py \
  src/services/scan_index_schema.py \
  src/services/scan_index_inventory.py \
  src/services/scan_index_store.py \
  src/services/scanner_items.py \
  src/services/scanner_parse_cache.py \
  src/services/file_scanner.py \
  src/services/cold_scanner_run.py \
  tests/test_scan_index_inventory.py \
  tests/test_scanner_items.py \
  tests/test_scanner_parse_cache.py \
  tests/test_cold_scanner_run.py
```

If Task 5 proves the existing Rust/parser WIP files are part of this same scanner/backend state, stage them only after listing them in the handoff summary:

```bash
git add \
  rust/discovery/src/lib.rs \
  rust/office_parser/src/lib.rs \
  src/services/light_text_parser.py \
  src/services/scan_planner.py \
  tests/test_office_parser.py \
  tests/test_schemas.py
```

- [ ] **Step 3: Verify staged diff names**

Run:

```bash
git diff --cached --name-only
```

Expected: only files intentionally staged in Step 2 appear.

- [ ] **Step 4: Commit implementation**

Run:

```bash
git commit -m "Refactor scanner index helpers"
```

Expected: local commit succeeds. Do not push.

- [ ] **Step 5: Prepare final report**

Final report must include:

- 改了什么：列出 scan index models/schema/inventory、scanner items、scanner parse cache、FileScanner wrapper、ColdScannerRun RuntimeError。
- 为什么这样改：说明收尾当前 WIP、降低 `ScanIndexStore` / `FileScanner` 职责、保持行为冻结。
- 验证了什么：粘贴实际执行的 pytest 命令和结果摘要。
- 仍有哪些风险：如 Rust/parser WIP 是否一起提交、全量测试是否有环境阻塞。
- 建议下一步：是否继续拆薄 `file_scanner.py` 或整理大测试文件。

---

## Self-Review

### Spec coverage

- 模型边界：Task 1、Task 2、Task 3 覆盖。
- schema/inventory/store facade：Task 3 覆盖。
- scanner item adapter：Task 1 覆盖。
- parse cache/reparse audit：Task 2 覆盖。
- FileScanner 兼容 wrapper：Task 1、Task 2、Task 5 覆盖。
- ColdScannerRun explicit runtime error：Task 4 覆盖。
- 行为冻结与 backend invariant：Task 5 覆盖。
- 不 push、不碰 secrets：Task 6 覆盖。

### Placeholder scan

The plan contains no deferred-work markers, no unspecified file paths, and no steps that ask for generic error handling without code or commands.

### Type consistency

- `InventoryItem` and `CacheProbe` are consistently sourced from `src.services.scan_index_models`.
- `ParseCacheStore` protocol methods match the calls made by `get_cached_contexts` and `write_parse_cache`.
- `ScannerItem = Path | InventoryItem` is the shared item type for scanner helper functions.
