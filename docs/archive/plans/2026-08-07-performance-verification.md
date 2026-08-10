# 性能收口与量化验收实施计划（阶段 6 + 8）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 Plan 1–4 的结构基础上收口三项性能（CLI 启动、pytest 全量、扫描吞吐），并完成阶段 8 的量化验收对比。性能改动以「可测量、可回退」为原则，不做无依据的重构。

**Architecture:** 启动优化已由 Plan 3（main.py 轻量入口 + 函数内延迟 import）基本完成，本 plan 先测量验证、再下沉残留重 import；测试提速用 fixture 确认 + xdist 评估（不默认开启）；扫描外层做调用审查，只实施低风险项。最终跑 Plan 1 verification 文档的同一组测量命令，产出前后对比表并给出验收结论（达标/未达标如实记录）。

**Tech Stack:** Python 3.13、pytest、pytest-xdist（评估）、uv run、rich。

**前置：** Plan 1–4 已完成；`uv run pytest` 全绿。

## Global Constraints

- 前置：`uv run pytest` 全绿基线（当前约 245+ passed）。
- 修改范围：`src/cli/*`（仅 import 下沉）、`pyproject.toml`（仅评估 xdist 时临时加 dev 依赖）、`docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-verification.md`；**禁止改** `rust/`、`templates/`、`src/core/llm.py`、`src/services/sqlite_store.py`、`src/services/context_*` 的行为。
- 验收目标（设计规格）：启动耗时较迁移前 ≥30% 改善；pytest 全量较迁移前 ≥25% 改善；扫描吞吐记录实测（不虚标）。达不到目标时在验收文档如实记录原因，不降级为「方向性优化」。
- xdist 只评估，**不默认开启**（改 `addopts`）。
- 每 Task 结束 `uv run pytest` 全绿。

---

### Task 1: 启动延迟加载收口与验证

**Files:**
- Modify: `src/cli/daily.py`、`src/cli/weekly.py`、`src/cli/monthly.py`（若存在顶层重 import 则下沉到函数内）
- 测量记录：更新 verification 文档「阶段 6 启动」小节

**Interfaces:**
- Consumes: Plan 3 后的 `main.py`（轻量入口 + 函数内分派 import）
- Produces: `--help` / `doctor` / `list` 三条命令的启动耗时下降；重依赖仅在 daily/weekly/monthly 执行时加载

- [ ] **Step 1: 测量启动现状**

Run（重复 3 次取中位数）：
```bash
uv run python -c "import subprocess,sys,time; t=time.perf_counter(); subprocess.run([sys.executable,'main.py','--help'],capture_output=True,text=True); print(f'help={time.perf_counter()-t:.3f}s')"
uv run python -c "import subprocess,sys,time; t=time.perf_counter(); subprocess.run([sys.executable,'main.py','doctor'],capture_output=True,text=True); print(f'doctor={time.perf_counter()-t:.3f}s')"
uv run python -c "import subprocess,sys,time; t=time.perf_counter(); subprocess.run([sys.executable,'main.py','list'],capture_output=True,text=True); print(f'list={time.perf_counter()-t:.3f}s')"
```
Expected: `--help` 显著低于迁移前 2.424s（目标 ≤1.7s）；`doctor`/`list` 记录实测。

- [ ] **Step 2: 检查重依赖是否被提前触发**

Run:
```bash
uv run python -X importtime main.py --help 2>&1 | grep -i "openai\|jinja2\|pandas\|pptx\|docx\|pdfplumber" | head
```
Expected: 无输出（`--help` 不加载这些重依赖）。若有命中，说明某模块在 `--help` 链上被顶层 import，定位后下沉。

- [ ] **Step 3: 下沉 src/cli 报告模块的顶层重 import**

检查 `src/cli/daily.py` / `weekly.py` / `monthly.py`：把顶层 `from src.core.llm import LLMClient`、`from src.core.config import config`、`from src.services.report_gen import ReportGenerator` 等**仅被 `_default_runner` 用到的重 import** 下沉到 `_default_runner` 函数内，例如：
```python
def _default_runner(console: Any) -> ReportRunner:
    from src.core.llm import LLMClient
    from src.core.logger import setup_logger
    from src.services.context_scheduler import ContextScheduler
    from src.services.report_gen import ReportGenerator
    from src.services.report_runner.model_port import LLMModelPort
    from src.services.sqlite_store import SQLiteStore
    from .input_adapter import ConsoleDailyInputAdapter

    return ReportRunner(
        scheduler=ContextScheduler(),
        store=SQLiteStore(),
        renderer=ReportGenerator(),
        model_port=LLMModelPort(client_factory=LLMClient),
        daily_input=ConsoleDailyInputAdapter(console=console),
    )
```
保留模块顶层仅 `argparse` / `typing` 等轻 import。weekly/monthly 同理。

- [ ] **Step 4: 复测启动**

重跑 Step 1 三条命令，确认 `--help` 无回退；若仍 >1.7s，检查 `main.py` 顶层 `from src.cli.doctor import run_bootstrap_doctor` 是否触发 `src.cli.doctor` 的链上重 import（应只有 `sys`），并记录在 verification 文档。

- [ ] **Step 5: 跑全量 + Commit**

Run: `uv run pytest`
Expected: 全绿。
```bash
git add src/cli tests
git commit -m "perf: defer heavy imports in cli report modules"
```

---

### Task 2: 测试提速（fixture 确认 + xdist 评估）

**Files:**
- Modify: `pyproject.toml`（评估 xdist 时临时在 `[dependency-groups].dev` 加 `pytest-xdist`）
- 记录：更新 verification 文档「阶段 6 测试」小节

**Interfaces:**
- Consumes: Plan 1 的 `tests/conftest.py`（`rust_release_binaries` session fixture）
- Produces: pytest 全量耗时基线；xdist 可行性结论（记录，不默认开启）

- [ ] **Step 1: 记录 pytest 单进程基线**

Run: `uv run pytest`
Expected: 记录 `in XX.XXs`（应与 Plan 1 迁移后 34.4s 同量级；若因新测试增加变慢，记录原因）。

- [ ] **Step 2: 确认 session fixture 复用生效**

Run: `uv run pytest tests/test_windows_release_package.py -q --collect-only 2>&1 | tail -3`
Expected: 正常收集，无重复 fixture 初始化报错。`rust_release_binaries` 在 session 内只建一次（可临时在 fixture 内 `print` 验证，验证后移除）。

- [ ] **Step 3: 临时安装并评估 xdist**

在 `pyproject.toml` `[dependency-groups].dev` 加 `"pytest-xdist>=3.6,<4"`：
Run: `uv sync`
Run: `uv run pytest -n auto -q`
Expected 与判定：
- 若**全绿且耗时明显下降**（≤单进程 70%）：记录"xdist 可用但默认关（SQLite/Rust 子进程并发未做深度隔离验证）"。
- 若**出现失败/卡死**（如 SQLite 锁、Rust 子进程竞争）：记录"xdist 不启用（并发破坏）"，并从 dev 组移除。
无论结果，`addopts` 都**不**加 `-n`。

- [ ] **Step 4: 恢复并复验**

若 Step 3 判定不启用：把 `pytest-xdist` 从 dev 组移除，`uv sync` 后跑 `uv run pytest` 确认恢复全绿。若判定可用但默认关：保留 dev 组声明，`addopts` 不变。

- [ ] **Step 5: Commit**

```bash
git add pyproject.toml uv.lock
git commit -m "test: evaluate pytest-xdist and record test runtime baseline"
```

---

### Task 3: 扫描外层调用审查与低风险优化

**Files:**
- 记录：更新 verification 文档「阶段 6 扫描」小节
- Modify: （仅当发现明确低风险优化点时）`src/services/rust_context_client.py` 或 `src/services/context_scheduler.py`

**Interfaces:**
- Consumes: `RustContextClient.build_context`（一次子进程 = 一次完整扫描）、现有 cold/warm benchmark（Plan 1 的 9fb618f 门禁）
- Produces: 扫描外层调用的事实清单与结论；仅实施低风险优化

- [ ] **Step 1: 确认一次 build-context 无重复扫描**

Run:
```bash
uv run pytest tests/test_benchmark_scanner.py -v 2>&1 | tail -20
```
Expected: cold 一次扫描、warm 全量复用（reused=3 / reparsed=0），与 verification 文档记录一致（warm ≈ 73 files/s）。

- [ ] **Step 2: 审查 Python 外层是否有重复扫描点**

读 `src/services/context_scheduler.py` 与 `src/services/rust_context_client.py` 的 `build_context` 调用路径，确认：一次 CLI 运行只触发一次 `build-context` 子进程；`source=db` 路径不触发 scanner（Plan 2 已保证）。若发现"同一周期被扫描两次"或"超时预算不合理"，记为可优化点。

- [ ] **Step 3: 实施低风险项（若有）**

仅当 Step 2 发现明确问题（如重复调用、明显过大的超时预算），才做最小改动并配测试；否则**不修改**（YAGNI，Rust 侧已是最优通道，Python 外层无理由额外优化）。结论写入 verification 文档。

- [ ] **Step 4: 跑全量 + Commit**

Run: `uv run pytest`
Expected: 全绿（无改动则跳过本 Task 的 commit；有改动则按最小 diff 提交）。

---

### Task 4: 量化验收（阶段 8）

**Files:**
- Modify: `docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-verification.md`（最终对比表 + 验收结论）

**Interfaces:**
- Consumes: Task 1–3 的测量结果、Plan 1 的「迁移前基线」
- Produces: 最终验收对比表与达标/未达标结论（阶段 8 收尾）

- [ ] **Step 1: 跑最终四表测量**

用与 Plan 1 完全相同的命令口径重测（迁移后）：
```bash
uv run python -c "import subprocess,sys,time; t=time.perf_counter(); subprocess.run([sys.executable,'main.py','--help'],capture_output=True,text=True); print(f'{time.perf_counter()-t:.3f}s')"
uv run python -c "import subprocess,sys,time; t=time.perf_counter(); subprocess.run([sys.executable,'main.py','doctor'],capture_output=True,text=True); print(f'{time.perf_counter()-t:.3f}s')"
uv run pytest
uv run pytest tests/test_benchmark_scanner.py -v
```
各测 3 次取中位数（pytest/benchmark 记录输出中的耗时）。

- [ ] **Step 2: 填写最终对比表**

在 verification 文档追加「最终验收（阶段 8）」小节，表格含：启动 `--help` / `doctor` / `list`、pytest 全量、扫描吞吐（cold/warm），每行 = 迁移前值 → 迁移后值 → 变化 %。

- [ ] **Step 3: 对照设计目标给结论**

按设计规格 4.1/4.2 的目标逐项判定：
- 启动 `--help`：迁移前 2.424s → 目标 ≤1.697s（≥30%）。达标则写"达标"；未达标写"未达标（差 X s，原因：…）"。
- 测试全量：迁移前 36.0s → 目标 ≤27.0s（≥25%）。未达标如实记录（可能因 pwsh 后更多测试跑起来、或新增 ReportRunner 测试，属范围变化而非性能回退，需在文档说明口径差异）。
- 扫描吞吐：记录 cold/warm 实测，说明是否持平/提升（本阶段不做 Rust 侧优化，预期持平）。
- 明确"阶段 0–2 为等价迁移、阶段 6 为启动/测试收口、PDF 吞吐另由阶段 7 门禁决定"。

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-verification.md
git commit -m "docs: finalize performance acceptance comparison"
```

---

## Self-Review

- **Spec coverage**：阶段 6 三项（启动/测试/扫描）由 Task 1–3 覆盖；阶段 8 验收由 Task 4 覆盖。启动优化充分复用 Plan 3 的结构基础；扫描外层明确 YAGNI（不强行优化，Rust 主通道）。xdist 只评估不默认开（符合 spec）。
- **占位符**：无 TBD；每 Task 含具体命令与判定规则。
- **类型一致性**：verification 文档路径与 Plan 1 一致；测量命令口径一致；`_default_runner` 下沉后签名不变（Task 3 的 Plan 依赖它）。
