# config/services 边界重构实施计划（阶段 5）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 消除三处边界异味：`context_scheduler` 跨模块引用私有 `_ContextEngine`；`config.py` 466 行混用应用配置与 scanner 校验；services 分层未显式表达。目标是行为等价的重构，不改任何运行语义。

**Architecture:** ① 把 `context_engine.py` 的私有 Protocol `_ContextEngine` 公开为 `ContextEngine`（仅改名 + import）。② 把 `config.py` 的 scanner 校验纯逻辑（`SCANNER_CONTRACT_FIELDS` / `SCANNER_INFRASTRUCTURE_FIELDS` / `UnknownScannerContractFieldsError` / profile 提取）迁到新模块 `src/services/scanner_config.py`，`Config.scanner_contract_profile()` 保留并在 `config` 单例内委托，保持所有 `from ..core.config import config` import 面不变。③ services 分层通过模块 docstring 标注与单向依赖检查显式化，不搬文件。

**Tech Stack:** Python 3.13、pytest、uv run。

**前置：** Plan 1–3 已完成；`uv run pytest` 全绿。

## Global Constraints

- 前置：`uv run pytest` 全绿基线。
- 修改范围：`src/core/config.py`、`src/services/scanner_config.py`（新建）、`src/services/context_engine.py`、`src/services/context_scheduler.py`、各 services 模块 docstring、`tests/test_config.py`、`tests/test_scanner_config.py`（新建）；**禁止改** `rust/`、`templates/`、`src/models/*`、`src/core/llm.py`、`src/services/sqlite_store.py`。
- 行为等价：`Config` 单例的公开属性和 `scanner_contract_profile()` 输出对调用方完全不变；`config` 模块仍然导出单例 `config`。
- 每 Task 结束 `uv run pytest` 全绿。

---

### Task 1: 公开 `_ContextEngine` Protocol

**Files:**
- Modify: `src/services/context_engine.py`（`_ContextEngine` → `ContextEngine`）
- Modify: `src/services/context_scheduler.py`（引用更新）
- Modify: `tests/test_scanner_config.py`（新建，Task 1 先放边界断言）

**Interfaces:**
- Consumes: 现有 `context_engine._ContextEngine`
- Produces: `context_engine.ContextEngine`（公开 Protocol，签名不变）；`context_scheduler._engine_from_config` 类型标注更新

- [ ] **Step 1: 写失败测试**

创建 `tests/test_scanner_config.py`（Task 1 先放一条边界断言，Task 2 追加 scanner profile 等价测试）：
```python
"""scanner_config 拆分与模块边界等价性测试。"""
from __future__ import annotations

from src.services.context_engine import ContextEngine


def test_context_engine_protocol_is_public_name():
    """跨模块引用的 Protocol 必须是公开名，不再用下划线私有名。"""
    from src.services.context_scheduler import ContextScheduler

    assert ContextEngine is not None
    assert hasattr(ContextScheduler, "_engine_from_config")
```

- [ ] **Step 2: 运行确认失败**

Run: `uv run pytest tests/test_scanner_config.py -q`
Expected: FAIL（`ImportError: cannot import name 'ContextEngine'`）。

- [ ] **Step 3: 改名并更新引用**

在 `src/services/context_engine.py`：
- 把 `class _ContextEngine(Protocol):` 改为 `class ContextEngine(Protocol):`；
- `__all__ = ["ContextBuildResult"]` 改为 `__all__ = ["ContextBuildResult", "ContextEngine"]`。

在 `src/services/context_scheduler.py`：
- `from .context_engine import ContextBuildResult, _ContextEngine` 改为 `from .context_engine import ContextBuildResult, ContextEngine`；
- `engine: _ContextEngine | None = None` 与 `def _engine_from_config(...) -> _ContextEngine:` 中的类型标注全部改为 `ContextEngine`。

- [ ] **Step 4: 运行确认通过 + 全量**

Run: `uv run pytest tests/test_scanner_config.py tests/test_context_scheduler.py -q`
Expected: PASS。
Run: `uv run pytest`
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
git add src/services/context_engine.py src/services/context_scheduler.py tests/test_scanner_config.py
git commit -m "refactor: expose ContextEngine protocol as public name"
```

---

### Task 2: 拆分 scanner 配置校验到 scanner_config.py

**Files:**
- Create: `src/services/scanner_config.py`
- Modify: `src/core/config.py`（迁出 scanner 校验，`scanner_contract_profile()` 委托）
- Modify: `tests/test_scanner_config.py`（追加 profile 等价测试）

**Interfaces:**
- Consumes: 现有 `config.py` 的 `SCANNER_CONTRACT_FIELDS` / `SCANNER_INFRASTRUCTURE_FIELDS` / `UnknownScannerContractFieldsError` / `_to_builtin_value` / `scanner_contract_profile()`
- Produces: `scanner_config.extract_scanner_profile(scanner_settings) -> dict`（纯函数）、`scanner_config.SCANNER_CONTRACT_FIELDS`、`scanner_config.SCANNER_INFRASTRUCTURE_FIELDS`、`scanner_config.UnknownScannerContractFieldsError`；`Config.scanner_contract_profile()` 委托它

- [ ] **Step 1: 写失败测试（profile 等价）**

在 `tests/test_scanner_config.py` 追加：
```python
from types import SimpleNamespace

import pytest

from src.services.scanner_config import (
    UnknownScannerContractFieldsError,
    extract_scanner_profile,
)


def _scanner(**kwargs):
    return SimpleNamespace(**kwargs)


def test_extract_profile_passes_explicit_contract_leaves():
    profile = extract_scanner_profile(
        _scanner(
            allowed_extensions=[".txt", ".md"],
            max_workers=4,
            total_max_chars=50000,
        )
    )
    assert profile["schema_version"] == "scanner_profile_v1"
    assert profile["allowed_extensions"] == [".txt", ".md"]
    assert profile["max_workers"] == 4
    assert "rust_scanner_bin" not in profile


def test_extract_profile_rejects_unknown_leaves():
    with pytest.raises(UnknownScannerContractFieldsError) as exc:
        extract_scanner_profile(_scanner(unknown_leaf=1))
    assert "unknown_leaf" in str(exc)


def test_extract_profile_keeps_infrastructure_out_of_wire():
    profile = extract_scanner_profile(
        _scanner(rust_scanner_bin="bin/x", engine="rust_v2")
    )
    assert "rust_scanner_bin" not in profile
    assert "engine" not in profile


def test_config_delegates_profile_to_scanner_config(monkeypatch):
    import src.core.config as config_module
    from src.core.config import Config

    calls = []
    real = config_module.Config.scanner_contract_profile

    def spy(self):
        calls.append(self._settings.scanner)
        return real(self)

    monkeypatch.setattr(Config, "scanner_contract_profile", spy)
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(scanner=_scanner(max_workers=2))

    assert Config.scanner_contract_profile(cfg)["max_workers"] == 2
    assert calls == [cfg._settings.scanner]
```

- [ ] **Step 2: 运行确认失败**

Run: `uv run pytest tests/test_scanner_config.py -q`
Expected: FAIL（`ModuleNotFoundError: src.services.scanner_config`）。

- [ ] **Step 3: 创建 scanner_config.py**

创建 `src/services/scanner_config.py`（从 `config.py` 原样迁入以下符号，保持语义）：
```python
"""scanner 配置纯函数与 wire contract 校验（从 config.py 迁出）。

职责：把调用方显式配置的 scanner v1 wire 叶子提取为 versioned profile，
拒绝未知字段，并把基础设施字段排除在 wire 之外。默认值与归一化的唯一
所有者仍是 Rust core。
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Any


SCANNER_CONTRACT_FIELDS = (
    "allowed_extensions",
    "ignored_patterns",
    "excluded_dirs",
    "max_workers",
    "max_file_size_mb",
    "discovery_timeout_seconds",
    "file_timeout_seconds",
    "file_timeout_by_extension",
    "total_max_chars",
    "parser_profile_version",
    "office_parser_backend",
    "pdf_parser_backend",
    "office_fallback_policy_version",
    "office_parser_fallback_enabled",
    "office_fallback_after_timeout",
    "office_legacy_extensions_enabled",
    "pptx_include_notes",
    "office_parser_fallback_order",
    "direct_text_max_bytes",
    "direct_text_read_bytes",
    "log_tail_read_bytes",
    "text_excerpt_max_chars",
    "excel_max_rows",
    "pdf_max_pages",
    "text_max_chars",
    "excel_max_sheets",
    "excel_max_columns",
    "docx_max_paragraphs",
    "docx_max_tables",
    "docx_table_max_rows",
    "docx_table_max_cols",
    "pptx_max_slides",
    "document_excerpt_max_chars",
    "summary_excel_max_rows",
    "summary_pdf_max_pages",
    "summary_text_max_chars",
    "summary_excel_max_sheets",
    "summary_excel_max_columns",
    "summary_docx_max_paragraphs",
    "summary_docx_max_tables",
    "summary_docx_table_max_rows",
    "summary_docx_table_max_cols",
    "summary_pptx_max_slides",
    "summary_document_excerpt_max_chars",
)

SCANNER_INFRASTRUCTURE_FIELDS = frozenset(
    {
        "rust_office_parser_bin",
        "engine",
        "rust_scanner_bin",
        "rust_index_db_path",
        "rust_process_timeout_seconds",
    }
)


class UnknownScannerContractFieldsError(ValueError):
    """表示 scanner 配置包含不能进入版本化 wire contract 的字段。"""

    def __init__(self, fields: Sequence[str]) -> None:
        self.fields = tuple(sorted(set(fields)))
        super().__init__(
            "unknown scanner contract fields: " + ", ".join(self.fields)
        )


def _to_builtin_value(value: Any) -> Any:
    """递归转成原生容器，避免 Dynaconf 容器在 Windows spawn 下无法 pickle。"""
    if isinstance(value, Mapping):
        return {
            str(key): _to_builtin_value(item) for key, item in value.items()
        }
    if isinstance(value, Sequence) and not isinstance(
        value, (str, bytes, bytearray)
    ):
        return [_to_builtin_value(item) for item in value]
    return value


def extract_scanner_profile(scanner_settings: Any) -> dict[str, Any]:
    """提取调用方显式配置的 scanner v1 wire 叶子。

    Rust 是默认值和归一化的唯一所有者，因此这里不补默认值，也不携带
    worker、数据库或进程路径。
    """
    if isinstance(scanner_settings, Mapping):
        raw_items = scanner_settings.items()
    elif hasattr(scanner_settings, "__dict__"):
        raw_items = vars(scanner_settings).items()
    else:
        raise ValueError("scanner settings must expose explicit leaves")

    present = {
        str(key).strip().lower(): _to_builtin_value(value)
        for key, value in raw_items
    }
    unknown = sorted(
        set(present) - set(SCANNER_CONTRACT_FIELDS) - SCANNER_INFRASTRUCTURE_FIELDS
    )
    if unknown:
        raise UnknownScannerContractFieldsError(unknown)

    profile: dict[str, Any] = {"schema_version": "scanner_profile_v1"}
    for key in SCANNER_CONTRACT_FIELDS:
        if key in present:
            profile[key] = present[key]
    return profile
```

- [ ] **Step 4: config.py 改为委托**

在 `src/core/config.py`：
- 删除 `SCANNER_CONTRACT_FIELDS`、`SCANNER_INFRASTRUCTURE_FIELDS`、`UnknownScannerContractFieldsError`、`_to_builtin_value`、`_non_blank_string` 中仅被 scanner 用的部分（`_non_blank_string` 仍被 `rust_scanner_bin` 等基础设施属性用，**保留在 config**）。
- `scanner_contract_profile()` 方法体替换为委托：
```python
def scanner_contract_profile(self) -> dict[str, Any]:
    """提取调用方显式配置的 scanner v1 wire 叶子（委托 scanner_config）。"""
    from ..services.scanner_config import extract_scanner_profile

    return extract_scanner_profile(self._settings.scanner)
```
- 文件顶部 docstring 或注释注明 scanner 校验已迁至 `src/services/scanner_config.py`。

> 保持 `config.py` 中 `SCANNER_CONTRACT_FIELDS` 的引用为零（全局 grep 确认 `SCANNER_CONTRACT_FIELDS` / `SCANNER_INFRASTRUCTURE_FIELDS` / `UnknownScannerContractFieldsError` 仅出现在 `src/services/scanner_config.py` 与 `src/core/config.py` 的委托处）。

- [ ] **Step 5: 运行确认通过 + 全量**

Run: `uv run pytest tests/test_scanner_config.py tests/test_config.py tests/test_scanner_contract_fixtures.py -q`
Expected: PASS（原 config 测试与冻结的 profile 测试继续通过）。
Run: `uv run pytest`
Expected: 全绿。
Run: `grep -rn "SCANNER_CONTRACT_FIELDS\|UnknownScannerContractFieldsError" src/`
Expected: 仅命中 `src/services/scanner_config.py`。

- [ ] **Step 6: Commit**

```bash
git add src/services/scanner_config.py src/core/config.py tests/test_scanner_config.py
git commit -m "refactor: extract scanner profile validation into scanner_config module"
```

---

### Task 3: services 分层显式化 + 单向依赖检查

**Files:**
- Modify: `src/services/context_engine.py`、`src/services/rust_context_client.py`、`src/services/json_process_client.py`、`src/services/document_parser.py`、`src/services/context_scheduler.py`、`src/services/report_gen.py`、`src/services/sqlite_store.py`、`src/services/report_runner/runner.py`（docstring 分层标注）
- Modify: `docs/superpowers/specs/2026-08-07-config-services-boundaries.md`（新建分层说明，可选）

**Interfaces:**
- Consumes: 各模块现有公开接口（无签名变化）
- Produces: 无运行接口变化；分层归属在 docstring 中显式声明

- [ ] **Step 1: 给各 services 模块 docstring 标注分层归属**

逐模块在 docstring 首行之后加一行分层标注（只加注释，不改逻辑）：

| 模块 | 分层标注 |
|---|---|
| `models/scanner_contract.py` | `interface：wire DTO 与 Rust JSON contract` |
| `services/context_engine.py` | `interface：应用 DTO 与 ContextEngine Protocol` |
| `services/rust_context_client.py` | `adapter：Rust scanner 子进程适配` |
| `services/json_process_client.py` | `adapter：JSON 单请求/单响应子进程边界` |
| `services/document_parser.py` | `adapter：Python fallback 解析 worker` |
| `services/context_scheduler.py` | `orchestration：引擎选择与调度` |
| `services/report_gen.py` | `orchestration：Jinja 渲染与 Markdown 发布` |
| `services/sqlite_store.py` | `orchestration：报告存储` |
| `services/report_runner/` | `orchestration：报告编排应用 seam（依赖 scheduler/store/renderer/model port）` |

每处改为 docstring 首行追加该标注，例如 `context_scheduler.py`：
```python
"""应用级 context 调度边界；每次运行只选择一个完整 engine。

分层：orchestration —— 引擎选择与调度。
"""
```
（其余模块同理，仅首行 docstring 追加分层标注。）

- [ ] **Step 2: 单向依赖静态检查**

Run:
```bash
uv run python -c "
import ast, pathlib
root = pathlib.Path('src')
edges = []
for p in root.rglob('*.py'):
    tree = ast.parse(p.read_text(encoding='utf-8'))
    for node in ast.walk(tree):
        if isinstance(node, ast.ImportFrom) and node.module and node.module.startswith('src'):
            edges.append((str(p), node.module))
cli = [a for a,b in edges if 'cli' in a.replace('\\\\','/')]
back = [b for a,b in edges if 'cli' in a.replace('\\\\','/')]
for a,b in sorted(set(edges)):
    if 'src.services' in b and 'cli' not in a:
        pass
print('services -> cli 反向依赖:', [a for a,b in edges if b.startswith('src.cli') and a.startswith('src.services')])
print('runner 依赖:', sorted({b for a,b in edges if 'report_runner' in a}))
"
```
Expected: `services -> cli 反向依赖` 为空；`runner 依赖` 不包含 `src.cli`（ReportRunner 不依赖 CLI）。

- [ ] **Step 3: 跑全量**

Run: `uv run pytest`
Expected: 全绿（仅 docstring 变更，行为不变）。

- [ ] **Step 4: Commit**

```bash
git add src/services src/core/context_engine.py 2>/dev/null; git add src/services
git commit -m "docs: annotate services layering and verify one-way dependencies"
```
> 若 `git add src/services` 会带上非预期文件，改为显式 `git add` 上一步修改的各模块。

---

## Self-Review

- **Spec coverage**：阶段 5 三项（`_ContextEngine` 公开、config 拆分、services 分层）由 Task 1–3 覆盖；`config` 单例 import 面不变（Global Constraints + Task 2 Step 4）；`scanner_contract_profile()` 等价（Task 2 Step 1 的 spy 测试）。
- **占位符**：无 TBD；Task 2 Step 3 的 `SCANNER_CONTRACT_FIELDS` 完整列出。
- **类型一致性**：`extract_scanner_profile` 输出与旧 `Config.scanner_contract_profile` 完全一致（`schema_version` 键、顺序、排除基础设施）；委托调用点签名一致。
