# Project Cleanup and Storage Unification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 删除迁移遗留与兼容层，统一以 `SQLiteStore` 作为唯一存储入口，清理过时文档与本地运行产物，同时保留 SQLite 数据库与 Markdown 报表保存功能。

**Architecture:** 保持现有 `core / models / services / templates` 结构不变，只做小幅收口。CLI 直接依赖 `SQLiteStore`，迁移链路相关脚本与兼容壳全部移除，文档和配置说明只保留当前真实支持的存储与 LLM provider。

**Tech Stack:** Python 3.10+, pytest, SQLite, Dynaconf, Rich, Jinja2

---

### Task 1: Remove `HistoryManager` and make `SQLiteStore` the only storage entry

**Files:**
- Modify: `tests/test_sqlite_store.py`
- Modify: `src/services/__init__.py`
- Modify: `main.py`
- Delete: `src/services/history_mgr.py`
- Delete: `tests/test_history_mgr.py`
- Test: `tests/test_sqlite_store.py`

- [ ] **Step 1: Write the failing test**

Add a package-level import test and fold the `HistoryManager`-only coverage into `tests/test_sqlite_store.py`.

```python
from src.services import SQLiteStore


def test_services_exports_sqlite_store():
    assert SQLiteStore.__name__ == "SQLiteStore"


def test_get_month_reports(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(_make_daily_report("2026-01-27"))
    store.save_report(_make_daily_report("2026-01-28"))
    store.save_report(_make_daily_report("2026-02-01"))

    reports = store.get_month_reports("2026-01")

    assert [r.date for r in reports] == ["2026-01-27", "2026-01-28"]


def test_get_reports_in_range_skips_weekends(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")

    reports, missing = store.get_reports_in_range(
        date(2026, 1, 31),
        date(2026, 2, 1),
    )

    assert reports == []
    assert missing == []


def test_get_week_reports(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(_make_daily_report("2026-01-26"))
    store.save_report(_make_daily_report("2026-01-28"))

    reports, missing = store.get_week_reports(2026, 5)

    assert [r.date for r in reports] == ["2026-01-26", "2026-01-28"]
    assert "2026-01-27" in missing
    assert "2026-01-29" in missing
    assert "2026-01-30" in missing


def test_list_all_reports(tmp_path):
    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(_make_daily_report("2026-02-03"))
    store.save_report(_make_daily_report("2026-02-01"))

    assert store.list_all_reports() == ["2026-02-01", "2026-02-03"]
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest tests/test_sqlite_store.py -q`

Expected: FAIL with `ImportError: cannot import name 'SQLiteStore' from 'src.services'`

- [ ] **Step 3: Write minimal implementation**

Update the package export, switch the CLI to `SQLiteStore`, then remove the compatibility file and duplicate test file.

`src/services/__init__.py`

```python
"""服务模块"""

from .file_scanner import FileScanner
from .report_gen import ReportGenerator
from .sqlite_store import SQLiteStore

__all__ = ["FileScanner", "ReportGenerator", "SQLiteStore"]
```

`main.py`

```python
from src.services.file_scanner import FileScanner
from src.services.report_gen import ReportGenerator
from src.services.sqlite_store import SQLiteStore
```

```python
store = SQLiteStore()
```

Replace every `history_mgr = HistoryManager()` with `store = SQLiteStore()` and rename the downstream calls accordingly:

```python
yesterday_plan = store.get_yesterday_plan()
reports, missing_days = store.get_week_reports(year, week_num)
reports, missing_days = store.get_reports_in_range(start_date, end_date)
store.save_report(report_data)
store.save_weekly_report(report_data)
store.save_monthly_report(report_data)
dates = store.list_all_reports()
```

Delete the compatibility layer and duplicate test file:

```bash
git rm src/services/history_mgr.py tests/test_history_mgr.py
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest tests/test_sqlite_store.py -q`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add main.py src/services/__init__.py tests/test_sqlite_store.py
git commit -m "refactor: remove history manager wrapper"
```

### Task 2: Remove obsolete migration code and its dead tests

**Files:**
- Delete: `scripts/migrate_json_to_sqlite.py`
- Delete: `scripts/migrate_daily_schema.py`
- Delete: `src/services/json_to_sqlite_migrator.py`
- Delete: `tests/test_json_to_sqlite_migrator.py`
- Test: repository-wide reference search

- [ ] **Step 1: Confirm the migration path still exists before cleanup**

Run: `rg -n "migrate_json_to_sqlite|migrate_daily_schema|JSONToSQLiteMigrator|json_to_sqlite_migrator" .`

Expected: FINDS matches in the two scripts, the migrator service, and the dedicated migration tests.

- [ ] **Step 2: Remove the obsolete files**

Because the user confirmed both migration paths are no longer needed, delete the scripts, the dead service used only by those scripts, and the migration-only test module.

```bash
git rm scripts/migrate_json_to_sqlite.py
git rm scripts/migrate_daily_schema.py
git rm src/services/json_to_sqlite_migrator.py
git rm tests/test_json_to_sqlite_migrator.py
```

- [ ] **Step 3: Verify the migration code path is gone**

Run: `rg -n "migrate_json_to_sqlite|migrate_daily_schema|JSONToSQLiteMigrator|json_to_sqlite_migrator" .`

Expected: NO MATCHES

- [ ] **Step 4: Commit**

```bash
git commit -m "chore: drop obsolete migration path"
```

### Task 3: Sync docs and config guidance with the real architecture

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `AGENTS.md`
- Modify: `check_config.py`
- Modify: `src/core/config.py`
- Test: targeted grep checks

- [ ] **Step 1: Write the failing verification checks**

Run the stale-reference scan first so the cleanup has an explicit before-state.

Run: `rg -n "gemini|HistoryManager|history_mgr|migrate_json_to_sqlite|JSON 数据库|json_to_sqlite_migrator" README.md CLAUDE.md AGENTS.md check_config.py src/core/config.py`

Expected: FINDS outdated provider, migration, or compatibility-layer references.

- [ ] **Step 2: Update the docs and config validation**

Apply these concrete edits.

`README.md`

```md
## 数据来源（`--source`）

- `db`: 从 SQLite 历史库聚合（推荐）
- `scan`: 直接扫描工作目录文件

## 配置说明

### `config/settings.toml`

- `[llm]`
- `provider = "deepseek"`            `# deepseek | openai`
- `model_id = "deepseek-chat"`       `# OpenAI 示例: gpt-4o-mini`
- `temperature = 0.2`
- `max_tokens = 8192`
- `max_retries = 3`

### `config/.secrets.toml`

- `[api]`
- `deepseek_api_key = "your-deepseek-key"`
- `openai_api_key = "your-openai-key"`

## 存储与输出

- 历史存储：`data/db/reports.sqlite3`
- Markdown 报告输出：`data/reports/`
```

Delete the old migration section from `README.md`.

`CLAUDE.md`

```md
- LLM provider 可切换：`deepseek` / `openai`
- 历史数据默认存储：SQLite（`data/db/reports.sqlite3`）
```

Remove the following outdated structure entries and commands from `CLAUDE.md`:

```md
- `history_mgr.py   # 兼容入口（继承 SQLiteStore）`
- `json_to_sqlite_migrator.py`
- `python scripts/migrate_json_to_sqlite.py --dry-run`
- `python scripts/migrate_json_to_sqlite.py`
```

`AGENTS.md`

```md
- LLM backend is selectable via `llm.provider` (`deepseek` or `openai`). For OpenAI, set `OPENAI_API_KEY` (or `api.openai_api_key` in `config/.secrets.toml`) and use a model that supports JSON schema output (e.g., `gpt-4o-mini`).
```

`src/core/config.py`

```python
    def llm_provider(self) -> str:
        """LLM provider (deepseek/openai)"""
```

`check_config.py`

```python
        if config.llm_provider == "openai":
            api_key = config.openai_api_key
            missing_env = "OPENAI_API_KEY"
        elif config.llm_provider == "deepseek":
            api_key = config.deepseek_api_key
            missing_env = "DEEPSEEK_API_KEY"
        else:
            errors.append(
                f"不支持的 llm.provider: {config.llm_provider}，仅支持 deepseek / openai"
            )
            api_key = ""
            missing_env = ""

        if missing_env and (not api_key or api_key.startswith("${")):
            errors.append(f"未配置 {missing_env}")
```

- [ ] **Step 3: Run verification checks again**

Run: `rg -n "gemini|HistoryManager|history_mgr|migrate_json_to_sqlite|JSON 数据库|json_to_sqlite_migrator" README.md CLAUDE.md AGENTS.md check_config.py src/core/config.py`

Expected: NO MATCHES

- [ ] **Step 4: Commit**

```bash
git add README.md CLAUDE.md AGENTS.md check_config.py src/core/config.py
git commit -m "docs: align docs with sqlite architecture"
```

### Task 4: Remove stale spec and clear generated runtime artifacts

**Files:**
- Delete: `docs/superpowers/specs/2026-04-03-report-text-simplification-design.md`
- Delete contents: `logs/`
- Delete contents: `data/reports/`
- Delete caches: `.pytest_cache/`, `.ruff_cache/`, `__pycache__/`, `scripts/__pycache__/`
- Test: filesystem verification commands

- [ ] **Step 1: Inspect the artifact paths before deletion**

Run: `Get-ChildItem -Recurse data\\reports, logs | Select-Object FullName`

Expected: LISTS existing generated Markdown files and log files.

- [ ] **Step 2: Delete the stale spec and generated artifacts**

Keep `data/db/` untouched, including `reports.sqlite3`, because the user explicitly asked to retain the database.

```powershell
Remove-Item -LiteralPath "docs\superpowers\specs\2026-04-03-report-text-simplification-design.md" -Force
Get-ChildItem -LiteralPath "logs" -Force | Remove-Item -Recurse -Force
Get-ChildItem -LiteralPath "data\reports" -Force | Remove-Item -Recurse -Force
Remove-Item -LiteralPath ".pytest_cache" -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath ".ruff_cache" -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath "__pycache__" -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath "scripts\__pycache__" -Recurse -Force -ErrorAction SilentlyContinue
```

- [ ] **Step 3: Verify only the intended paths were cleared**

Run: `Get-ChildItem data, logs | Select-Object Name`

Expected:
- `data\db` still exists
- `data\reports` exists but is empty, or is recreated empty later by the app
- `logs` exists and is empty, or is recreated empty later by the app

Run: `Test-Path data\\db\\reports.sqlite3`

Expected: `True`

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "chore: clear generated artifacts"
```

### Task 5: Run final verification before closing the cleanup

**Files:**
- Test: entire repository

- [ ] **Step 1: Verify no compatibility or migration references remain**

Run: `rg -n "HistoryManager|history_mgr|migrate_json_to_sqlite|migrate_daily_schema|JSONToSQLiteMigrator|json_to_sqlite_migrator" .`

Expected: NO MATCHES

- [ ] **Step 2: Run the full test suite**

Run: `python -m pytest tests -q`

Expected: PASS

- [ ] **Step 3: Inspect the final worktree**

Run: `git status --short`

Expected: clean working tree

- [ ] **Step 4: Commit the final verification checkpoint**

```bash
git commit --allow-empty -m "chore: verify project cleanup"
```
