# 轻量清理实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 不动架构地整理项目——归档已完成的历史设计文档、删除经 grep 验证的死代码、给性能门禁测试加本地 skip 机制、核对依赖并做全量验证。

**Architecture:** 四阶段执行,每阶段独立 commit、可回退。P1 文档(P1a 归档 → P1b CLAUDE.md → P1c AGENTS.md → P1d CONTEXT.md)→ P2 死代码(P2a src/ → P2b tests+scripts → P2c 保留决策记录)→ P3 性能门禁 marker → P4 依赖核对+全量验证。零架构改动、零功能删除、Rust 代码零改动。

**Tech Stack:** Python 3.13.13(uv)、pytest、git、Rust(cargo,仅验证不动)。

## Global Constraints

- **不动架构**:不删功能、不删 Rust 代码、不删证据体系(`scripts/benchmark*`、`.artifacts/`、`docs/contracts/`、`docs/adr/` 全部保留)。
- **Rust 零改动**:`rust/` 下任何文件不得编辑;`cargo test --workspace --locked` 必须保持通过。
- **删除双闸**:每个删除候选必须先 grep 全仓确认零引用,再删;删除后相关 pytest 通过。拿不准的保留。
- **每阶段独立 commit**:提交信息简短祈使句,末尾附 `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。
- **本地性能门禁**:P3 完成后本地 `pytest` 应全绿(门禁测试显示 skipped);CI(`.github/workflows/windows-release.yml`)不改。
- **运行命令**:`uv run python ...`、`uv run pytest ...`;不激活 Conda base。
- **文档一致性**:改完的 CLAUDE.md/AGENTS.md 与 `docs/adr/0002`、`docs/scanner-backends.md`、实际目录结构一致。

---

### Task 1: 归档已完成的设计/计划文档到 docs/archive/

**Files:**
- Move: `docs/superpowers/specs/*.md`(25 份,含 2026-08-10-light-cleanup-design.md)
- Move: `docs/superpowers/plans/*.md`(19 份,含本计划文件 2026-08-10-light-cleanup.md)
- Create: `docs/archive/specs/`、`docs/archive/plans/`

**Interfaces:**
- Consumes: 无
- Produces: `docs/archive/{specs,plans}/` 目录结构;`docs/superpowers/` 目录清空后删除

**说明**:本计划文件与 spec 按 spec「自归档说明」随本次归档一并移入,不设例外。

- [ ] **Step 1: 建归档目录**

```powershell
New-Item -ItemType Directory -Force docs\archive\specs, docs\archive\plans
```

- [ ] **Step 2: git mv 全部文档**

```powershell
git mv docs/superpowers/specs/*.md docs/archive/specs/
git mv docs/superpowers/plans/*.md docs/archive/plans/
```

- [ ] **Step 3: 删除空目录并确认零残留**

```powershell
Remove-Item docs\superpowers\specs, docs\superpowers\plans -Force
git status --short
```

Expected: `docs/superpowers/` 完全消失;`docs/archive/{specs,plans}/` 下共 44 份 .md。

- [ ] **Step 4: 确认无活文档引用归档路径**

```powershell
Select-String -Path README.md, CLAUDE.md, AGENTS.md -Pattern "superpowers" -SimpleMatch
Select-String -Path docs\adr\*.md, docs\*.md -Pattern "superpowers" -SimpleMatch
```

Expected: 均无匹配(此前已核实零引用)。

- [ ] **Step 5: 跑测试确认文档移动无副作用**

```powershell
uv run pytest -q --tb=short
```

Expected: 与移动前一致(基线:非性能套件全绿约 585 个 + 基准 7 文件 35 通过,唯一可能失败是 corpus gate 偶发红;P3 之前允许它红)。

- [ ] **Step 6: Commit**

```powershell
git add -A
git commit -m "docs: archive completed design/plan documents to docs/archive"
```

提交信息末尾附加 Co-Authored-By 行。

---

### Task 2: 更新 CLAUDE.md 的过期结构描述

**Files:**
- Modify: `CLAUDE.md`(Project Overview 的 scan 一句、Project Structure 整节、Key Patterns 若漂移)

**Interfaces:**
- Consumes: 实际目录结构(`src/`、`rust/`)、`docs/adr/0002`、`docs/scanner-backends.md`
- Produces: 与现状一致的 CLAUDE.md;后续 Task 3 以它为基准对齐 AGENTS.md

**背景**:CLAUDE.md「Project Structure」仍列 4 个已删除文件(`file_scanner.py`/`scan_discovery.py`/`scan_planner.py`/`office_parser.py`),旧 Python scanner 链已在 ADR 0002 切线上时删除。Project Overview 的 scan 描述「Python fallback 保留用于正确性和可审计性」表述含糊,与 ADR 0002「顶层无静默 fallback」需对齐。

- [ ] **Step 1: 读当前 CLAUDE.md 与 ADR 0002**

```powershell
Get-Content CLAUDE.md
Get-Content docs\adr\0002-windows-first-rust-scanner-core.md
Get-Content docs\scanner-backends.md
```

- [ ] **Step 2: 替换 Project Overview 的 scan 句子**

旧句(「scan 路径默认使用 Rust discovery……Python fallback 保留用于正确性和可审计性」)替换为:

```markdown
- scan 路径默认使用 Rust scanner core(Rust discovery + Rust parser CLI);`.xlsx` 走 `rust_xlsx_bounded_v1` 有界预览,`.docx` / `.pptx` 走 `rust_office_oxide_v1`,PDF 与显式启用的 legacy 格式走 Python document worker。Office parser 超时默认不 fallback(除非显式开启 `office_fallback_after_timeout`),无顶层静默 fallback,详见 `docs/scanner-backends.md` 与 ADR 0002。
```

- [ ] **Step 3: 替换 Project Structure 整节**

旧节(src/ 下 file_scanner.py 等 4 个已删文件、rust/ 只有 discovery/ 与 office_parser/)替换为(外层用四反引号包裹,避免与树形图的 ```text 围栏冲突):

````markdown
```text
src/
├── cli/                 # CLI 子命令 (daily/weekly/monthly/list/doctor)
├── core/
│   ├── config.py        # 单例配置 (Dynaconf)
│   ├── healthcheck.py   # CLI 环境检查
│   ├── llm.py           # DeepSeek/OpenAI 客户端 + JSON 校验重试
│   └── logger.py
├── models/
│   ├── schemas.py       # 报告 Pydantic 模型 (日/周/月)
│   └── scanner_contract.py  # scanner/worker 契约 DTO 镜像
├── services/
│   ├── sqlite_store.py  # SQLite 存储实现（日/周/月）
│   ├── context_engine.py / context_scheduler.py   # 上下文构建编排
│   ├── rust_context_client.py  # Rust scanner CLI 适配 (build-context)
│   ├── scanner_config.py      # scanner profile 归一化/校验
│   ├── json_process_client.py # JSON 子进程契约执行
│   ├── document_parser.py     # Python document 解析 (PDF/legacy lane)
│   ├── report_gen.py
│   └── report_runner/    # 报告运行编排 (requests/outcomes/runner)
├── workers/             # crash-isolated Python worker (document/PDF classifier)
└── utils/
    └── text_tools.py

rust/                    # Cargo workspace
├── scanner_core/        # Rust scanner/context core (store/scheduler/session/…)
├── discovery/           # Rust discovery
└── office_parser/       # Rust Office parser worker CLI
```
````

- [ ] **Step 4: 核对 Key Patterns 是否漂移**

逐个核对 Key Patterns 条目与 `config/settings.example.yaml`、`docs/scanner-backends.md` 一致:

```powershell
Select-String -Path config\settings.example.yaml -Pattern "summary_mode|total_max_chars|office_fallback_after_timeout|parser_backend|worker_lane"
Get-Content docs\scanner-backends.md
```

Expected: 条目与实际配置键一致;若有漂移(如配置键已改名),以实际配置为准修订该条目,并在该条后附 `docs/scanner-backends.md` 引用。拿不准的保留原文。

- [ ] **Step 5: 核对 Commands 一节**

```powershell
uv run python main.py doctor
```

Expected: doctor 输出正常;Commands 一节无需改(uv 命令与 pyproject 一致)。

- [ ] **Step 6: Commit**

```powershell
git add CLAUDE.md
git commit -m "docs: refresh CLAUDE.md structure and scan description"
```

---

### Task 3: 更新 AGENTS.md 的过期命令

**Files:**
- Modify: `AGENTS.md`(Build/Test 一节)

**Interfaces:**
- Consumes: Task 2 定稿的 CLAUDE.md 命令基准
- Produces: 与 uv 工具链一致的 AGENTS.md

**背景**:AGENTS.md 第 15 行仍写 `pip install -r requirements.txt`;仓库无 `requirements.txt`(uv 项目,依赖在 `pyproject.toml` + `uv.lock`,锁定物为 `requirements.lock`)。其余结构段此前核对基本准确。

- [ ] **Step 1: 替换依赖安装命令**

第 15 行 `.\.venv\Scripts\python.exe -m pip install -r requirements.txt` 替换为:

```markdown
- `uv sync` installs dependencies from `pyproject.toml` / `uv.lock` into the project `.venv`.
```

- [ ] **Step 2: 对齐其余命令写法**

将 Build/Test 一节中 `.\.venv\Scripts\python.exe main.py ...` 各条改为 `uv run python main.py ...`;`python -m pytest tests/ -v` 改为 `uv run pytest`。与 CLAUDE.md Commands 一节保持一致(CLAUDE.md 已是 uv 写法)。

- [ ] **Step 3: 核对无其它过期引用**

```powershell
Select-String -Path AGENTS.md -Pattern "requirements.txt|pip install"
```

Expected: 无匹配(除 Step 1 已删的那处)。

- [ ] **Step 4: Commit**

```powershell
git add AGENTS.md
git commit -m "docs: align AGENTS.md commands with uv toolchain"
```

---

### Task 4: 核对 CONTEXT.md 术语与代码一致

**Files:**
- Modify: `CONTEXT.md`(仅当术语与代码漂移时)

**Interfaces:**
- Consumes: `src/services/context_scheduler.py`、`src/services/document_parser.py`、`src/services/json_process_client.py`、`src/services/rust_context_client.py`、`config/settings.example.yaml`
- Produces: 与代码一致的术语表(预期零改动或微改)

- [ ] **Step 1: 逐条核对 5 个术语的代码映射**

```powershell
# 1. Cold scanner run → context_scheduler 冷路径/缓存
Select-String -Path src\services\context_scheduler.py -Pattern "cold|缓存|cache" | Select-Object -First 5
# 2. Hybrid Office fallback policy → office_fallback_after_timeout 配置
Select-String -Path config\settings.example.yaml -Pattern "office_fallback"
# 3. 三类 failure 类 (Deterministic/Environment-unavailable/Contract)
Select-String -Path src\services\document_parser.py, src\services\json_process_client.py -Pattern "retryable|error_code" | Select-Object -First 10
# 4. Rust CLI JSON contract → json_process_client / rust_context_client
Select-String -Path src\services\json_process_client.py -Pattern "class |def " | Select-Object -First 10
```

Expected: 每个术语都能在代码中找到对应概念(术语表定义的是分类语义,不要求同名符号)。若有术语描述的机制已不存在,修订该条;若语义仍成立,零改动。

- [ ] **Step 2: Commit(或注明零改动)**

有改动:

```powershell
git add CONTEXT.md
git commit -m "docs: refresh CONTEXT.md glossary against current code"
```

无改动:跳过 commit,在 P1 汇总时口头说明。

---

### Task 5: 删除 src/ 层已确认未使用的 import

**Files:**
- Modify: `src/services/rust_context_client.py:21`(删 `Diagnostic`)
- Modify: `src/workers/pdf_classifier.py:14-22`(删 `CLASSIFIER_BUILD`、`CLASSIFIER_BUILD_INPUTS`、`CLASSIFIER_CONTRACT_VERSION`、`CLASSIFIER_PROTOCOL_VERSION`;**保留** `POLICY_VERSION`、`classifier_version_json`、`classifier_version_payload`)
- Modify: `src/workers/contracts.py:19-28`(删 `PYTHON_WORKER_VERSION`、`WORKER_CONTRACT_VERSION`;**保留** `PYTHON_WORKER_BUILD`、`PYTHON_WORKER_BUILD_INPUTS` 及其它)

**Interfaces:**
- Consumes: 各候选已验证的证据(import 行是全文件唯一出现处)
- Produces: 零未用 import 的 src/;保持 re-export 面不变

**已核实证据(无需重查,但删除后要跑对应测试)**:
- `rust_context_client.py:21`:`Diagnostic` 仅 import 行出现(全文件 grep 唯一命中)。
- `pdf_classifier.py`:测试经 re-export 消费 `POLICY_VERSION`、`classifier_version_json`(tests/test_pdf_classifier.py:14,122),故这三者保留;`CLASSIFIER_*` 4 个常量仅 import 行出现。
- `contracts.py`:tests/test_python_worker_build_fingerprint.py:8 经 re-export 消费 `PYTHON_WORKER_BUILD`/`PYTHON_WORKER_BUILD_INPUTS`,保留;`PYTHON_WORKER_VERSION`/`WORKER_CONTRACT_VERSION` 仅 import 行出现。

- [ ] **Step 1: 删除 rust_context_client.py 的 Diagnostic**

编辑 `src/services/rust_context_client.py` 第 21 行附近 import 块,移除 `Diagnostic,` 一行。

- [ ] **Step 2: 删除 pdf_classifier.py 的 4 个 CLASSIFIER_* 常量**

编辑 `src/workers/pdf_classifier.py` import 块,只留:

```python
from .pdf_classifier_identity import (
    POLICY_VERSION,
    classifier_version_json,
    classifier_version_payload,
)
```

- [ ] **Step 3: 删除 contracts.py 的 2 个常量**

编辑 `src/workers/contracts.py` import 块,移除 `PYTHON_WORKER_VERSION,` 与 `WORKER_CONTRACT_VERSION,` 两行;保留 `PYTHON_WORKER_BUILD`、`PYTHON_WORKER_BUILD_INPUTS`。

- [ ] **Step 4: 跑相关测试**

```powershell
uv run pytest tests/test_rust_context_client.py tests/test_pdf_classifier.py tests/test_document_parser_worker.py tests/test_python_worker_build_fingerprint.py -q --tb=short
```

Expected: 全部通过,0 failed。

- [ ] **Step 5: 复核删除点无残留引用**

```powershell
Select-String -Path src\**\*.py -Pattern "Diagnostic|CLASSIFIER_BUILD|CLASSIFIER_CONTRACT_VERSION|CLASSIFIER_PROTOCOL_VERSION|PYTHON_WORKER_VERSION|WORKER_CONTRACT_VERSION" | Where-Object { $_.Path -notmatch "identity|scanner_contract" }
```

Expected: 无命中(identity 定义文件与 scanner_contract 的 DTO 除外)。

- [ ] **Step 6: Commit**

```powershell
git add src/services/rust_context_client.py src/workers/pdf_classifier.py src/workers/contracts.py
git commit -m "refactor: remove verified-unused imports in src"
```

---

### Task 6: 删除 tests/ 与 scripts/ 层已确认未使用的 import

**Files:**
- Modify: `tests/test_main.py:3`(删 `Namespace`;**保留** `subprocess`——第 197/213/220/315 行在用)
- Modify: `tests/test_inspect_v2.py:24`(删 `ContextScheduler`;**保留** `SimpleNamespace`——第 49/56 行在用)
- Modify: `tests/test_timer_harness.py:2-4`(删 `subprocess`、`Path`、`BenchmarkResult`;**保留** `json`/`sys`/`time`/`wall_clock_ms`)
- Modify: `tests/test_worker_session.py:12`(删 `time`)
- Modify: `scripts/benchmark_harness.py:4`(删 `json`)
- Modify: `scripts/benchmark_seed_preparer.py:27`(删 `dataclass`)

**Interfaces:**
- Consumes: AST 扫描 + grep 验证结果
- Produces: 零未用 import 的 tests/ 与 scripts/

**注意**:`test_timer_harness.py:2` 是 `import json, subprocess, sys, time` 单行复合导入,删除时只删 `subprocess, ` 一段。`benchmark_harness.py:4` 与 `benchmark_seed_preparer.py:27` 两处删除前先跑一次 Step 2 的验证 grep(扫描器对这两处曾标记,需现场复核)。

- [ ] **Step 1: 现场复核 scripts/ 两处候选**

```powershell
Select-String -Path scripts\benchmark_harness.py -Pattern "json\.|json\b"
Select-String -Path scripts\benchmark_seed_preparer.py -Pattern "dataclass"
```

Expected: `json` 在 benchmark_harness.py 中除 import 行无使用;`dataclass` 在 benchmark_seed_preparer.py 中除 import 行无使用。若发现使用,保留该行并跳过对应删除。

- [ ] **Step 2: 删除 6 处 import**(test_main.py、test_inspect_v2.py、test_timer_harness.py、test_worker_session.py、benchmark_harness.py、benchmark_seed_preparer.py 各删对应未用名)

- [ ] **Step 3: 跑测试**

```powershell
uv run pytest tests/test_main.py tests/test_inspect_v2.py tests/test_timer_harness.py tests/test_worker_session.py -q --tb=short
```

Expected: 全部通过,0 failed。

- [ ] **Step 4: 记录「保留决策」并在 commit message 中说明**

以下 3 个零消费者导出**故意保留**(public export surface,删除属 API 变更,超出轻量整理范围):

- `src/models/schemas.py:12` `DataSource`(经 `src/models/__init__.py` re-export,全仓零消费)
- `src/models/scanner_contract.py:705` `EngineStatus`(在模块 `__all__`,全仓零消费)
- `src/models/scanner_contract.py:457` `NormalizedScannerProfileV2`(在模块 `__all__`,全仓零消费)

```powershell
git add tests/test_main.py tests/test_inspect_v2.py tests/test_timer_harness.py tests/test_worker_session.py scripts/benchmark_harness.py scripts/benchmark_seed_preparer.py
git commit -m "refactor: remove verified-unused imports in tests/scripts

Zero-consumer exports DataSource, EngineStatus, NormalizedScannerProfileV2
kept intentionally (public API surface)."
```

---

### Task 7: 性能门禁本地 skip 机制

**Files:**
- Modify: `pyproject.toml`(`[tool.pytest.ini_options]` 加 markers)
- Modify: `tests/conftest.py`(加 `pytest_collection_modifyitems`)
- Modify: 6 个性能门禁测试文件(加 `@pytest.mark.perf_gate`)

**Interfaces:**
- Consumes: 本地 pytest 实测失败集(1 个:corpus gate,见 Step 3)
- Produces: `perf_gate` marker;本地默认 skip、CI 与 `RUN_PERF_GATES=1` 全跑

**设计**(`docs/archive/specs/2026-08-10-light-cleanup-design.md` 第 3 节已批准):一处判定 + 一个 marker。CI 行为零改动。

- [ ] **Step 1: pyproject.toml 注册 marker**

在 `[tool.pytest.ini_options]` 下追加:

```toml
markers = [
    "perf_gate: 性能门禁测试,断言时间/性能预算,仅 CI 全跑;本地默认 skip",
]
```

- [ ] **Step 2: conftest.py 加环境判定钩子**

在 `tests/conftest.py` 文件顶部(`from pathlib import Path` 之后)追加:

```python
import os


def pytest_collection_modifyitems(config, items):
    """性能门禁仅在 CI(GitHub Actions 设 CI=true)或显式 RUN_PERF_GATES=1 时全跑;
    本地开发默认跳过,避免环境性能地板触发红牌。"""
    if os.environ.get("CI") or os.environ.get("RUN_PERF_GATES") == "1":
        return
    for item in items:
        if "perf_gate" in item.keywords:
            item.add_marker(
                pytest.mark.skip(
                    reason="性能门禁仅 CI 全跑(本地环境与验收预算不符);"
                    "如需本地全跑设 RUN_PERF_GATES=1"
                )
            )
```

- [ ] **Step 3: 跑全量测试,收集失败集**

```powershell
uv run pytest -q --tb=line
```

Expected: 1 个失败(可偶发,多跑一次可能绿),完整名记录为:

```
tests/test_corpus_gate.py::test_nine_cache_combo_semantic_output_identical
```

**已实测的失败根因**(2026-08-10 复现):corpus gate 的 calibration `build-context` 偶发
`status=partial`,warnings 为 `PARSER_TIMEOUT`:"pdf classification Unknown"
(backend=`pdf_classifier`,retryable=true)。PDF 分类走 Python worker(pypdfium2),
worker 冷启动超过分类预算即超时,`timeout_count=1` → partial。5 次循环复现 1 次,
属环境性能地板问题(与本机 python 冷启动 ~3.2s > 2s 分类预算同源),非语义缺陷:
同请求重跑即 `status=ok`(实测 total_duration_ms=3747,远低于 60s deadline)。
其余 6 个基准文件(含全部 `test_benchmark_*`)当前本地全部通过——历史记忆中的
"6 个必红"已在后续 bench commit 校准到机器地板,只剩此 1 个。

**打标理由**:该测试断言语义一致性(非时间预算),但其本地失败是环境性能驱动
(PARSER_TIMEOUT),属于「本地环境与验收预算不符」类;CI 仍完整运行该门禁,验收语义不变。

- [ ] **Step 4: 给失败集打 marker**

对 Step 3 收集的失败测试 `test_nine_cache_combo_semantic_output_identical`
(tests/test_corpus_gate.py:30),在测试函数定义行上方加:

```python
@pytest.mark.perf_gate
```

(该文件已 `import pytest`,无需补。)

- [ ] **Step 5: 验证本地全绿**

```powershell
uv run pytest -q --tb=short
```

Expected: 0 failed;corpus gate 测试显示 skipped,其余全部通过(1 个 skipped,
与 Step 3 失败集一致)。

- [ ] **Step 6: 验证 CI 语义不变(本地模拟)**

```powershell
$env:CI="true"; uv run pytest tests/test_benchmark_timer_baseline.py tests/test_benchmark_scanner.py -q --tb=line; Remove-Item Env:CI
```

Expected: 打标测试照常执行(不 skip)。

- [ ] **Step 7: Commit**

```powershell
git add pyproject.toml tests/conftest.py <6 个打标测试文件>
git commit -m "test: gate perf-boundary tests behind CI-only marker"
```

---

### Task 8: 依赖核对、README 核对与全量验证

**Files:**
- Modify: `README.md`(仅当发现过期路径/命令时)
- Modify: `pyproject.toml`(仅当发现确认未用依赖时;预期不改)

**Interfaces:**
- Consumes: `pyproject.toml` dependencies、全仓 import
- Produces: 依赖使用核对结论 + 全量验证绿

- [ ] **Step 1: 逐个核对依赖有使用**

```powershell
$deps = @("pydantic","dynaconf","yaml","rich","pandas","openpyxl","pptx","pdfplumber","jinja2","docx","sharepoint_to_text","openai","pypdfium2")
foreach ($d in $deps) {
  $hits = (Select-String -Path src\**\*.py, tests\**\*.py, scripts\**\*.py -Pattern $d -ErrorAction SilentlyContinue | Measure-Object).Count
  "{0}: {1} hits" -f $d, $hits
}
```

Expected: 每个依赖 ≥1 hit(`sharepoint_to_text` 在 `src/workers/python_worker_identity.py` 等身份/契约处)。零 hit 的依赖先人工确认(可能是字符串后端名),确认无用才删——拿不准的保留。

- [ ] **Step 2: 核对 README 无过期路径/命令**

```powershell
Select-String -Path README.md -Pattern "requirements.txt|superpowers|file_scanner|scan_discovery|scan_planner|office_parser.py"
```

Expected: 无匹配。有匹配则改为实际路径(`docs/archive/`、`docs/scanner-backends.md`)。

- [ ] **Step 3: 全量验证**

```powershell
uv sync
uv run python main.py doctor --strict
uv run pytest -q --tb=short
cd rust; cargo test --workspace --locked; cd ..
```

Expected: `doctor --strict` 通过;`pytest` 0 failed(6 个 perf_gate skipped);`cargo test` 全绿。

- [ ] **Step 4: Commit(仅当有 README/pyproject 改动)**

```powershell
git add README.md
git commit -m "docs: fix stale paths in README"
```

无改动则跳过 commit,全量验证即本阶段产出。

---

## 执行顺序总览

| 任务 | 阶段 | 产出 | 验证 |
|---|---|---|---|
| 1 | P1a | `docs/archive/{specs,plans}/` 44 份文档 | pytest 与基线一致 |
| 2 | P1b | CLAUDE.md 结构与 scan 描述准确 | doctor 通过 |
| 3 | P1c | AGENTS.md 命令对齐 uv | 无 requirements.txt 残留 |
| 4 | P1d | CONTEXT.md 术语与代码一致 | 术语↔代码映射核对 |
| 5 | P2a | src/ 零未用 import | 4 个相关测试通过 |
| 6 | P2b | tests/ scripts/ 零未用 import | 4 个相关测试通过 |
| 7 | P3 | perf_gate marker,本地 corpus gate skip | 本地全绿;CI 模拟不 skip |
| 8 | P4 | 依赖核对 + 全量验证绿 | doctor/pytest/cargo 全绿 |

## Self-Review 记录

- **Spec 覆盖**:归档(§清理清单 A)→ Task 1;CLAUDE.md/AGENTS.md/CONTEXT.md(§A)→ Task 2/3/4;死代码(§B,双闸规则)→ Task 5/6 + 保留决策;门禁 marker(§性能门禁)→ Task 7;依赖核对+全量验证(§C、§P4)→ Task 8。P1-P4 各阶段 commit、Rust 零改动均落在 Global Constraints。
- **无占位符**:全部步骤含具体命令与预期输出;P3 失败集以"实测收集"步骤定义,判据明确。
- **类型/名称一致**:re-export 消费方(POLICY_VERSION、classifier_version_json、PYTHON_WORKER_BUILD 等)在 Task 5 保留名单与测试文件行号一致。
