# uv + pytest 工具链迁移实施计划（阶段 0–2）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把项目从 pip + `requirements*.txt` 迁移到 uv（`pyproject.toml` + `uv.lock`），正式化 pytest 设施，记录迁移前后性能基线。本计划覆盖设计规格的阶段 0–2；阶段 3+（ReportRunner、CLI 拆分、config 拆分、性能收口、PDF 门禁）由后续独立 plan 承接。

**Architecture:** 根目录 `pyproject.toml` 声明依赖（12 项范围从 `requirements.txt` 原样搬入）与 `[dependency-groups].dev`（pytest），`[tool.uv] package = false` 保持"非打包、根路径 import"的现状；`uv sync` 生成 `uv.lock` 并重建 `.venv`；日常命令统一为 `uv run python main.py ...` / `uv run pytest`。pytest 经 `[tool.pytest.ini_options]` 配置 + `tests/conftest.py` session fixture 正式化，并清理历史 `.tmp` 堆积。

**Tech Stack:** uv 0.12、Python 3.13（`.venv`）、pytest 8.4、PowerShell 7 (pwsh)、Windows-first（Rust 组件由 cargo 管理，不归 uv）。

## Global Constraints

- `requires-python = ">=3.10"`；`dependencies` 范围从 `requirements.txt` 12 项**原样搬入，不升级**。
- Windows-first：pytest 门禁含 PowerShell 契约测试（`Test-Json` / `run_current_release.ps1`），**本机必须安装 PowerShell 7 (pwsh)**；只有 Windows PowerShell 5.1 会导致 2 个既有测试失败。
- Rust 组件不属于 uv：构建仍用 `cargo build --release`，本计划不触碰 `rust/`。
- CLI 行为 1:1 等价：参数、退出码（0/1/130）、提示、预览保持不变；`main.py` 的 `_run_bootstrap_doctor` 轻量入口分支不动。
- 每阶段结束必须 `uv run pytest` 全绿（Task 1 解决 pwsh 后，220 passed + 15 skipped + 0 failed）才进入下一阶段。
- 不修改 Rust CLI JSON contract、scanner DB、parser backend、fallback policy、LLM provider、报告模板与 schema。
- 性能基线用同一命令在迁移前后各测一次，写入 `docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-verification.md`。
- 已知迁移前基线（本会话实测，供参考）：`--help` 启动 3.154s；pytest 全量 23.83s、220 passed/15 skipped/2 failed（缺 pwsh）。

---

### Task 1: 安装 pwsh 并记录迁移前性能基线

**Files:**
- Create: `docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-verification.md`
- (环境) 安装 PowerShell 7

**Interfaces:**
- Consumes: 无（独立前置任务）
- Produces: `verification` 文档中的「迁移前基线」小节（后续 Task 5 填「迁移后基线」对比）；pwsh 可用（后续 Task 4 全绿门禁依赖）

- [ ] **Step 1: 确认 pwsh 是否已安装**

Run:
```bash
pwsh -NoProfile -Command '$PSVersionTable.PSVersion.ToString()'
```
Expected: 输出 `7.x.x`。若已装（输出 7.x），跳到 Step 3。

- [ ] **Step 2: 安装 PowerShell 7（若未安装）**

首选 winget（可能需要用户授权，subagent 无权限时让用户在终端执行）：
```bash
winget install --id Microsoft.PowerShell --source winget --accept-package-agreements --accept-source-agreements
```
验证（**新开终端**，让 PATH 刷新）：
```bash
pwsh -NoProfile -Command '$PSVersionTable.PSVersion.ToString()'
```
Expected: 输出 `7.x.x`。
Fallback（winget 失败时）：提示用户从 https://github.com/PowerShell/PowerShell/releases 下载 `PowerShell-7.x-win-x64.msi` 安装，装完再验证 `pwsh --version`。

- [ ] **Step 3: 验证既有 2 个 PowerShell 测试在 pwsh 下通过**

Run:
```bash
.venv/Scripts/python.exe -m pytest tests/test_scanner_contract_fixtures.py::test_valid_and_invalid_fixtures_match_draft_2020_12_schemas tests/test_windows_release_package.py::test_launcher_rejects_relative_root_pointer_escape_and_missing_shared_dirs -q
```
Expected: `2 passed`。若仍失败，**停止并报告**（说明这仍是环境问题，不是本计划可修复的代码问题），不要继续 Task 2。

- [ ] **Step 4: 记录迁移前性能基线**

创建 `docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-verification.md`，先写文档骨架 + 「迁移前基线」小节，内容用以下命令实测填写（迁移前用 `.venv` 的 python）：

```bash
# 1) 启动 --help 耗时（重复 3 次取中位数）
.venv/Scripts/python.exe -c "import subprocess,sys,time; t=time.perf_counter(); subprocess.run([sys.executable,'main.py','--help'],capture_output=True,text=True); print(f'{time.perf_counter()-t:.3f}s')"
# 2) 启动 doctor 耗时
.venv/Scripts/python.exe -c "import subprocess,sys,time; t=time.perf_counter(); subprocess.run([sys.executable,'main.py','doctor'],capture_output=True,text=True); print(f'{time.perf_counter()-t:.3f}s')"
# 3) pytest 全量耗时（读输出尾部 in XX.XXs）
.venv/Scripts/python.exe -m pytest tests/ -q
# 4) 扫描吞吐（读 benchmark 测试输出的吞吐指标）
.venv/Scripts/python.exe -m pytest tests/test_benchmark_scanner.py -v
```

文档骨架（后续各 Task 会往「迁移后基线」追加数据）：
```markdown
# uv + pytest 工具链迁移验收记录

> 日期：2026-08-07
> 对应设计：`docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-design.md`

## 迁移前基线（阶段 0）

| 指标 | 实测值 | 命令 |
|---|---|---|
| 启动 --help | <Step 4 填> | `.venv/Scripts/python.exe -c "…"` |
| 启动 doctor | <Step 4 填> | `.venv/Scripts/python.exe -c "…"` |
| pytest 全量 | <Step 4 填> | `.venv/Scripts/python.exe -m pytest tests/ -q` |
| 扫描吞吐 | <Step 4 填> | `.venv/Scripts/python.exe -m pytest tests/test_benchmark_scanner.py -v` |

## 迁移后基线（阶段 5 填）
```

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-verification.md
git commit -m "docs: record pre-migration performance baseline"
```

---

### Task 2: 创建 pyproject.toml 并执行 uv sync

**Files:**
- Create: `pyproject.toml`
- (生成) `uv.lock`；uv 会更新 `.venv`

**Interfaces:**
- Consumes: `requirements.txt` 的 12 项依赖（Task 3 删除前读取）
- Produces: `pyproject.toml`（依赖与 pytest 配置的单一事实源）、`uv.lock`；Task 3 依赖它的 `uv run` 入口与 `[dependency-groups]`

- [ ] **Step 1: 创建 pyproject.toml**

创建 `pyproject.toml`（依赖范围与 `requirements.txt` 完全一致；dev 组 pytest 版本与现有 `requirements-dev.txt` 一致）：
```toml
[project]
name = "ai-daily-report"
version = "5.0.0"
description = "审计报告生成器 v5.0"
requires-python = ">=3.10"
dependencies = [
    "pydantic>=2.0.0,<3",
    "dynaconf>=3.2.0,<4",
    "PyYAML>=6.0.0,<7",
    "rich>=13.0.0,<16",
    "pandas>=2.0.0,<3",
    "openpyxl>=3.1.0,<4",
    "python-pptx>=0.6.0,<2",
    "pdfplumber>=0.10.0,<1",
    "jinja2>=3.1.0,<4",
    "python-docx>=1.1.0,<2",
    "sharepoint-to-text>=1.1,<2",
    "openai>=1.0.0,<3",
]

[dependency-groups]
dev = [
    "pytest==8.4.2",
    "pytest-timeout>=2.3,<3",
]

[tool.uv]
package = false

[tool.pytest.ini_options]
testpaths = ["tests"]
addopts = "-q --tb=short"
```
说明：`package = false` 让 uv 不尝试打包/editable install，保持"根路径 import"现状（`import main` / `from src.core…` 依赖 cwd 与 pytest rootdir，与现在一致）。

- [ ] **Step 2: 执行 uv sync**

Run:
```bash
uv sync
```
Expected: 解析依赖并生成 `uv.lock`，更新/重建 `.venv`，输出 `Installed N packages` 或 `Audited N packages`。**若解析失败**（尤其 `sharepoint-to-text` 或某包在 Python 3.13 无可用版本），停止并报告具体报错，不要擅自改依赖范围。

- [ ] **Step 3: 验证 uv run 基本可运行**

Run:
```bash
uv run python -c "import src.core.config; print('config ok')"
uv run python main.py --help
```
Expected: 打印 `config ok`；`--help` 退出码 0 且输出 `usage:`。Rust 二进制路径无需变动。

- [ ] **Step 4: Commit**

```bash
git add pyproject.toml uv.lock
git commit -m "tool: introduce uv project with pyproject.toml"
```

---

### Task 3: 切换到 uv run 并删除 requirements 文件

**Files:**
- Delete: `requirements.txt`、`requirements-dev.txt`
- Modify: `.gitignore`（在 Python 段加 `.uv/` 保障，若需要）
- Modify: `CLAUDE.md`（Commands 与依赖说明改为 uv）

**Interfaces:**
- Consumes: Task 2 的 `pyproject.toml`/`uv.lock`
- Produces: 无外部接口；确立 `uv run` 为唯一运行/测试入口，供后续所有 Task 使用

- [ ] **Step 1: 用 uv run 跑全量测试确认等价**

Run:
```bash
uv run pytest
```
Expected: 与 Task 1 Step 3 相同，`220 passed, 15 skipped`，`0 failed`（pytest-timeout 是新增 dev 依赖，不改变现有断言）。

- [ ] **Step 2: 删除 requirements 文件**

```bash
git rm requirements.txt requirements-dev.txt
```

- [ ] **Step 3: 更新 .gitignore（保障 uv 与临时目录）**

读取 `.gitignore`，在 Python 段追加（若缺失）：
```
.uv/
.tmp/
```
（`data/` 与 `.venv/` 已在忽略列表，无需重复。）

- [ ] **Step 4: 更新 CLAUDE.md 的 Commands**

把 CLAUDE.md 的 Commands 段从 `pip install -r requirements.txt` 改为：
```markdown
# 安装依赖（uv）
uv sync

# 日报
uv run python main.py daily
uv run python main.py daily -i "今日工作内容"
uv run python main.py daily --no-save -i "预览模式"
uv run python main.py daily --date 2026-02-05 -i "..."

# 周报 / 月报 / 列表（逐条把 python main.py 前缀换成 uv run python main.py）
# 测试
uv run pytest
```
保留「Rust scanner helpers」段（`cargo test` / `cargo build --release`），并在 Commands 上方补一行注明"Python 依赖与测试统一走 uv；Rust 组件仍用 cargo"。

- [ ] **Step 5: 复验并 Commit**

Run: `uv run pytest`
Expected: 全绿。
```bash
git add -A
git commit -m "tool: switch to uv run entrypoints and drop requirements files"
```

---

### Task 4: pytest 正式化（tool.pytest 配置 + conftest）

**Files:**
- Modify: `pyproject.toml`（`[tool.pytest.ini_options]` 已是 Task 2 骨架，此处按需微调）
- Create: `tests/conftest.py`

**Interfaces:**
- Consumes: Task 2 的 `[tool.pytest.ini_options]` 与 dev 组
- Produces: `rust_release_binaries` session fixture（`dict[str, Path]`），供后续阶段（benchmark、ReportRunner、e2e）复用；本阶段为无害新增，现有测试不依赖它

- [ ] **Step 1: 创建 tests/conftest.py**

创建 `tests/conftest.py`（极简、无外部依赖，避免污染现有测试）：
```python
"""共享测试 fixtures：为后续阶段提供跨模块复用的测试设施。"""

from __future__ import annotations

from pathlib import Path

import pytest


@pytest.fixture(scope="session")
def rust_release_binaries() -> dict[str, Path]:
    """Rust release 二进制路径（Windows-first）。缺失时由依赖方决定跳过。"""
    release = Path(__file__).resolve().parents[1] / "rust" / "target" / "release"
    return {
        "scanner": release / "ai-daily-scanner.exe",
        "office_parser": release / "ai-daily-office-parser.exe",
    }
```

- [ ] **Step 2: 确认 pytest 配置生效**

Run:
```bash
uv run pytest --collect-only -q 2>&1 | tail -5
```
Expected: 输出 `220 tests collected` 且无 `PytestUnknownMarkWarning`（无未注册自定义 marker 则无需 `markers` 声明）。

- [ ] **Step 3: 全量验证**

Run: `uv run pytest`
Expected: `220 passed, 15 skipped`，无 `0 failed`，且因 `addopts = -q` 输出更紧凑、无告警噪音。若出现未知 marker 告警，把对应 marker 名加入 `pyproject.toml` 的 `[tool.pytest.ini_options].markers`。

- [ ] **Step 4: Commit**

```bash
git add tests/conftest.py pyproject.toml
git commit -m "test: formalize pytest config and shared conftest fixtures"
```

---

### Task 5: 清理历史临时文件并记录迁移后基线

**Files:**
- Delete: `data/.pytest-tmp/`、`.tmp/`（历史堆积，约 4000+ 文件）
- Modify: `.gitignore`（若 Task 3 已加 `.tmp/` 则确认；`data/` 已忽略）
- Modify: `docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-verification.md`（填「迁移后基线」）

**Interfaces:**
- Consumes: Task 1 的 verification 文档、Task 3 的 uv run 入口
- Produces: 干净的临时目录状态；「迁移后基线」数据；供后续阶段对照的性能起点

- [ ] **Step 1: 删除历史临时目录**

```bash
rm -rf data/.pytest-tmp .tmp
```
Expected: 无输出（成功删除）。删除前先用 `du -sh data/.pytest-tmp .tmp 2>/dev/null` 记录规模（写入 commit message 可选）。

- [ ] **Step 2: 确认 .gitignore 覆盖**

确认 `.gitignore` 含 `data/`（已有）与 `.tmp/`（Task 3 已加）；`git status --short` 不应显示任何 `.tmp` / `data/.pytest-tmp` 新文件。

- [ ] **Step 3: 记录迁移后基线**

把 Task 1 Step 4 的 4 条命令改为 `uv run python …` / `uv run pytest` 重跑，把结果填入 verification 文档「迁移后基线」小节，并把「迁移前」「迁移后」两行合并为对比表：

```markdown
## 对比（迁移前 → 迁移后）

| 指标 | 迁移前 | 迁移后 | 变化 |
|---|---|---|---|
| 启动 --help | <迁移前> | <uv 后实测> | 期望持平或略增（uv run 有解析开销），不作为本阶段验收项 |
| 启动 doctor | <迁移前> | <uv 后实测> | 同上 |
| pytest 全量 | <迁移前> | <uv 后实测> | 期望持平或略减 |
| 扫描吞吐 | <迁移前> | <uv 后实测> | 本阶段不预期变化 |
```
说明：阶段 0–2 的目标是**工具链等价迁移**，不追求启动/吞吐提升；启动优化（阶段 6）才以本表为对比基准。若 uv 后测试明显变慢（>1.5×），在文档记录原因并反馈。

- [ ] **Step 4: 更新 CLAUDE.md 测试命令**

确认 CLAUDE.md 测试命令已是 `uv run pytest`（Task 3 已改）；若测试相关段落还残留 `python -m pytest`，一并改为 `uv run pytest`。

- [ ] **Step 5: 复验并 Commit**

Run: `uv run pytest`
Expected: 全绿，且临时目录不再重建于 `data/.pytest-tmp`（pytest 默认 `tmp_path` 在系统临时区，测试不产生根目录堆积）。
```bash
git add -A
git commit -m "chore: clean historical pytest tmp dirs and record post-migration baseline"
```
