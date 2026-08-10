# uv + pytest 工具链迁移与结构/性能优化 设计规格

> 状态：Ready for implementation
> 日期：2026-08-07
> 决策范围：依赖工具链（uv）、测试基础设施（pytest 正式化）、应用结构（ReportRunner + CLI 拆分 + config/services 边界）、性能（启动 / 测试 / 扫描 + PDF 基准门禁）
> 首要目标：把项目迁移到 uv 与正式化 pytest 工具链，落地既有 ReportRunner 设计收敛报告编排，并以量化基线验收性能
> 引用既有设计：`docs/superpowers/specs/2026-07-17-deep-report-run-module-design.md`（ReportRunner deep module，本规格的「抽公共流程」直接落地该设计，不再另建 flow）
> 不涉及：Rust scanner 内核算法、LLM provider/报告模板/JSON schema、报告数据库 schema、新业务功能

## Problem Statement

### 当前问题

1. **依赖工具链落后**：项目用 pip + `requirements.txt` / `requirements-dev.txt` 管理，无 `pyproject.toml`、无锁文件、无统一项目元数据；Rust 组件独立于 Python 工具链管理。
2. **pytest 未正式化**：`requirements-dev.txt` 只有一行 `pytest==8.4.2`，无 `conftest.py`、无 `[tool.pytest]` 配置、无插件；测试临时文件在 `data/.pytest-tmp/` 与 `.tmp/` 大量堆积（现存 4000+ 文件），无统一清理策略。
3. **CLI 层过重**：`main.py` 580 行同时承担 argparse 定义、daily/weekly/monthly 三条报告编排、list 与 doctor，任何子命令都触发整条 import 链（rich/openai/jinja2/…）。
4. **报告编排三套重复**：daily/weekly/monthly 重复 source gate、LLM 调用、render、publish、preview、warning 与退出码规则；已有批准但未实现的 `ReportRunner` spec（`2026-07-17-deep-report-run-module-design.md`）正是要收敛这一点。
5. **配置与边界异味**：`config.py` 466 行混用路径/应用配置与 scanner 校验；`context_scheduler.py` 跨模块引用私有名 `_ContextEngine`。

### 目标

1. 用 **uv** 管理依赖与运行：`pyproject.toml` + `uv.lock`，`uv run python main.py` 保持现有 CLI 调用方式，删除 `requirements*.txt`。
2. **正式化 pytest**：`[tool.pytest.ini_options]` + `tests/conftest.py` 分层 fixtures + 按需插件，`uv run pytest` 为唯一测试入口，统一临时目录策略并清理历史堆积。
3. **落地 ReportRunner**：三条报告路径收敛到 `ReportRunner.run` seam，CLI 只做参数映射、交互适配、结果展示与退出码。
4. **拆分与收口**：`main.py` 拆为 `src/cli/` 包；`config.py` 拆出 scanner 校验到 `scanner_config`；services 分层显式化；修复 `_ContextEngine` 等边界。
5. **性能量化提升**：CLI 启动、pytest 全量耗时、扫描吞吐三项，以迁移前基线对比验收；PDF 是否 Rust 化由基准门禁决定。

### 成功边界

- CLI 参数、退出码、语义提示与 Markdown 预览保持兼容；
- Rust CLI JSON contract、scanner DB 所有权、cache identity 与 Hybrid Office fallback 不变；
- LLM 调用次数与 scanner 调用次数不增加；
- 迁移与重构分开验证：每阶段结束 `uv run pytest` 全绿；
- 启动、测试、吞吐以**表格式前后对比**呈现，不虚标。

## Solution

### 1. 工具链迁移：uv

#### 1.1 pyproject.toml

在项目根新建 `pyproject.toml`：

- `[project]`：name（`ai-daily-report`）、version、requires-python `>=3.10`、description 等元数据；`dependencies` 从 `requirements.txt` 的 12 项原样搬入（范围保持不变）。
- `[tool.uv]`：默认 `dev-dependencies` 放 pytest 与所选插件。
- `[tool.pytest.ini_options]`：见第 2 节。
- 其余 `[tool.*]`（ruff 等）按实施时实际引入的工具声明，不强求一次性配齐。

#### 1.2 uv 运行与锁文件

- 用 `uv sync` 生成/更新 `.venv` 与 `uv.lock`；现有 `.venv` 是 pip 建的，允许 uv 重建。
- 日常命令统一为 `uv run python main.py ...`、`uv run pytest`、`uv run python -c ...`。
- 删除 `requirements.txt` / `requirements-dev.txt`；`CLAUDE.md` / `AGENTS.md` 的 Commands 与依赖说明同步更新。
- Rust 组件不属于 uv：构建命令保持 `cargo build --release`，在文档注明。

**依赖风险**：旧 `requirements.txt` 是宽松区间，uv 解析可能升级版本（pandas/openai/pptx 等）带来隐性破坏 → 顺序上先 `uv sync` + `uv run pytest` 全绿验证，再动结构；任一依赖破坏立即回退或 pin 版本。

### 2. 测试基础设施：pytest 正式化

- `[tool.pytest.ini_options]`：`testpaths = ["tests"]`、`addopts`（`-q`、`--tb=short`）、`filterwarnings`（收敛第三方库告警）、`markers` 声明。
- `tests/conftest.py` 分层 fixtures：
  - session 级：Rust 二进制路径（`rust/target/release/ai-daily-scanner(.exe)` 等）、统一临时数据根、config 隔离（设置 `DAILY_REPORT_*` 环境变量指向临时目录，避免触碰本机 `settings.yaml`）。
  - function 级：`tmp_path` 驱动的临时目录、SQLite store、ReportRunner/CLI 相关构造器。
- 统一临时目录策略：以 `tmp_path` 为准，删除散落手写 `.tmp` 逻辑；迁移时**一次性清理**历史 `data/.pytest-tmp/` 与根 `.tmp/`，并在 `.gitignore` 加 `data/.pytest-tmp/` 保障。
- 插件：核心加 **pytest-timeout**（防 Rust 子进程挂死）。**pytest-xdist** 仅评估：先确认 SQLite/Rust 子进程 fixture 的并发安全，安全才在 `addopts` 默认开启，否则文档注明可用但默认关。覆盖率（pytest-cov）不纳入本次默认（YAGNI，需要时再加）。

### 3. 结构设计

#### 3.1 main.py → src/cli/ 包

```
main.py                    # 只留入口：main() + 轻量 bootstrap doctor + 分派
src/cli/
├── __init__.py
├── parser.py              # build_parser（argparse 定义，从 main.py 原样搬）
├── daily.py               # daily 命令：构造 DailyReportRunRequest → ReportRunner → outcome 展示
├── weekly.py              # weekly 命令：构造 WeeklyReportRunRequest → ReportRunner → outcome 展示
├── monthly.py             # monthly 命令：构造 MonthlyReportRunRequest → ReportRunner → outcome 展示
├── doctor.py              # run_doctor_cmd
└── list_reports.py        # list_reports
```

约束：

- 保留 `main.py` 现有 `_run_bootstrap_doctor` 行为（rich/业务依赖不可用时先行诊断），该分支不迁移到重依赖路径。
- `main.py` 顶层只 import argparse/sys 等轻模块；子命令模块在执行时再加载重依赖（兼作启动优化，见 4.1）。
- `list` / `doctor` 是非报告命令，不进入 ReportRunner（对应既有 spec 的 Out of Scope 第 16 条），独立成模块即可。

#### 3.2 报告编排收敛：落地 ReportRunner

直接落地 `docs/superpowers/specs/2026-07-17-deep-report-run-module-design.md`，不再另建 `flow.py`。本规格引用其 interface、pipeline、error model 与验收标准，并按该 spec 建议的 characterization-first 顺序实施：

1. 先补 characterization tests，冻结现有 CLI 参数、退出码、source policy、日期范围、warning、preview 与 publication 顺序。
2. 建立 `ReportRunRequest` / `ReportRunOutcome` 类型与空的 `ReportRunner.run` interface 测试。
3. 迁移 daily（scan 固定、deferred input、date override、no-save）。
4. 迁移 weekly 的 db/scan 两条 recipe。
5. 迁移 monthly 的 db/scan 两条 recipe。
6. 收窄 CLI 为 request mapping + daily input adapter + outcome presentation + exit code。
7. 删除旧三套 orchestration 与重复 internal-order 测试。

关键约束（摘自既有 spec，实施必须遵守）：封闭 request union、typed success/failure outcome、稳定 error_code + phase + retryable、lazy LLM factory、`--no-save` 严格零报告发布副作用、SQLite 先于 Markdown 的非原子发布 + publication receipt、最终不保留新旧双轨 shim。

#### 3.3 模块边界修复

- `context_scheduler.py` 引用 `context_engine._ContextEngine`（私有名跨模块）→ 改为公开 `ContextEngine`（保持 Protocol 注入，只改名字与 import）。
- 实施时复查 `config.py` / `sqlite_store.py` / `report_gen.py` 与 `src/cli/` 之间是否形成循环依赖；若有则收口。

#### 3.4 拆分 config.py

`config.py` 466 行混用三类职责，拆为：

```
src/core/config.py         # 保持 config 单例与既有 import 面不变：
                           #   路径解析、installed mode、llm/api key、代理（应用配置 + 基础设施）
src/services/scanner_config.py   # scanner 配置纯函数/校验：
                           #   SCANNER_CONTRACT_FIELDS、scanner_contract_profile()、
                           #   UnknownScannerContractFieldsError 等，从 config 迁出
```

约束：`config` 单例对所有模块的 `from ..core.config import config` import 面不变，内部把 scanner 校验委托给 `scanner_config`；`scanner_contract_profile()` 等属性仍经 config 暴露。纯搬移 + 委托，行为等价，配等价性单测。

#### 3.5 services 分层显式化

显式化现有自然分层，不推倒重建、不引入依赖注入框架：

- **interface 层**：`models/scanner_contract.py`（wire DTO）+ `context_engine.py`（应用 DTO + `ContextEngine` Protocol）
- **adapter 层**：`rust_context_client.py`、`json_process_client.py`（Rust↔Python 边界）、`document_parser.py`（Python fallback worker）
- **orchestration 层**：`context_scheduler.py`（引擎选择）、`report_gen.py`、`sqlite_store.py`（存储）；新增 `report_runner/`（ReportRunner 实现，见既有 spec）
- 单向依赖：CLI → ReportRunner → scheduler/store/renderer/model port；adapter 不得反向依赖 CLI；ReportRunner 不读取 scanner DB。

### 4. 性能设计

#### 4.1 CLI 启动速度

- `main.py` 顶层只保留 argparse/sys；rich、openai、jinja2、report_gen、ReportRunner 等全部下沉到子命令模块，**仅对应命令执行时 import**。
- `--help` 与 `doctor` 路径不加载业务栈 → 启动最快。
- 目标：启动耗时 **≥30% 改善**；`--help`/`doctor` 绝对值降到轻量级（几百 ms 级，以基线上限换算具体目标）。

#### 4.2 测试运行速度

- conftest session 级 fixtures 复用 Rust 二进制路径、临时数据根、config 隔离，只建一次。
- 统一 `tmp_path` + 一次性清理历史堆积，消除噪音。
- pytest-xdist 评估后按结论决定默认开关。
- 目标：pytest 全量耗时 **≥25% 改善**（含清理噪音 + fixture 复用）。

#### 4.3 扫描解析吞吐

- 主通道已是 Rust，Python 外层优化范围：避免重复扫描（复用 inventory/cache 命中）、超时预算收口、减少无谓子进程往返。
- 复用现有 `tests/test_benchmark_scanner.py` / `test_benchmark_context_scheduler.py` 作为吞吐基线工具。

#### 4.4 PDF 基准门禁（先基准再定）

- 阶段 6 建基准：用现有语料对 **pdfplumber vs 候选 Rust 提取**做 质量（文本/字符保真、乱码率）+ 速度（P50/P90、吞吐）对比。
- 门禁判据：Rust 提取质量**不低于** pdfplumber 且速度**明显占优**（≥2×），才进入迁移设计；否则保持 pdfplumber，并在证据文档记录"维持现状"及理由。
- 该阶段产出独立证据文档，不阻塞工具链/pytest/结构主线；迁不迁都要在验收记录里写明结论。

### 5. 量化验收（前后对比）

迁移前在现 git 状态记录基线，迁移后跑同命令对比：

| 指标 | 迁移前命令 | 迁移后命令 |
|---|---|---|
| 启动 | `python -c "…测 main.py --help 耗时…"` | `uv run python -c "…"` |
| doctor | `python main.py doctor` | `uv run python main.py doctor` |
| 测试 | `python -m pytest tests/ -q` | `uv run pytest` |
| 扫描吞吐 | `python -m pytest tests/test_benchmark_scanner.py` | `uv run pytest …`（同参数） |

结果写入 `docs/superpowers/specs/2026-08-07-uv-pytest-toolchain-migration-verification.md`，以表格式前后对比呈现，含失败项原因与处理。

## 分阶段实施

```
阶段 0  建基线：启动 / doctor / 测试 / 扫描吞吐四项当前耗时记录 + PDF 基准语料准备
阶段 1  uv 迁移：pyproject.toml + uv.lock + uv sync + uv run pytest 全绿
        （含删 requirements*.txt、更新 .gitignore 与 CLAUDE.md/AGENTS.md）
阶段 2  pytest 正式化：tool.pytest 配置 + conftest 分层 fixtures + 清理历史 .tmp
阶段 3  ReportRunner 落地：按 3.2 characterization-first 顺序
        （类型 → daily → weekly → monthly → 删旧编排，可跨多个提交）
阶段 4  main.py → src/cli/：先拆 parser/list/doctor/bootstrap doctor（独立于报告命令）；
        报告命令 CLI 随 ReportRunner 落地收窄为 request mapping + outcome 展示
阶段 5  结构边界：config.py 拆分 scanner_config + services 分层显式化 + _ContextEngine 公开化
阶段 6  性能：启动延迟加载收口、测试 fixture 复用、扫描外层复用
阶段 7  PDF 基准门禁：跑对比，出证据文档，决定 PDF 是否迁 Rust
阶段 8  量化验收：前后对比 + 写验收记录 + 收尾
```

阶段 1–6 为主干（每阶段 `uv run pytest` 全绿）；阶段 7 独立于主干、不阻塞；阶段 8 汇总。阶段 4 的报告命令 CLI 拆分依赖阶段 3（避免把三套旧编排先搬进 `src/cli/` 再改一遍），但 parser/list/doctor/bootstrap doctor 等独立模块可先行拆分。

## 测试策略

- **门禁**：阶段 1 后每阶段结束跑 `uv run pytest` 全量，把"迁移破坏"与"重构破坏"分开定位。
- **等价性**：`src/cli/` 拆分后行为由现有 `tests/test_main.py` 覆盖，不重写业务断言；新增 `parser.py` 参数等价、ReportRunner interface 测试（见既有 spec）、`scanner_config` 拆分后 `scanner_contract_profile()` 等价性测试。
- **本地 substitute**：SQLite、Jinja、filesystem 用真实临时资源；仅 LLM 用 deterministic mock adapter（lazy factory）。
- **契约回归**：Rust workspace tests、scanner contract fixture、Hybrid Office fallback 回归、cold/warm scan smoke 照常跑，证明本次不改变请求与契约。

## 风险与对策

| 风险 | 对策 |
|---|---|
| uv 解析升级依赖破坏 | 阶段 1 先全绿验证再动结构；必要时回退/pin 版本 |
| config 单例拆分破坏 import 面 | 纯搬移 + 委托，`scanner_contract_profile` 行为等价单测 |
| bootstrap doctor 行为丢失 | main.py 拆分时原样保留该分支 |
| ReportRunner 落地偏离既有 spec | 直接引用该 spec，实施以 characterization-first 冻结行为，最终不保留双轨 |
| PDF 基准不占优 | 维持 pdfplumber，不强行迁移（基准门禁的目的） |
| pytest-xdist 并发破坏 SQLite/Rust 子进程 | 只评估不默认开，确认安全后才加 |
| 测试临时文件再次堆积 | 统一 tmp_path + .gitignore 保障 |

## 验收标准

- [ ] `uv run python main.py <daily|weekly|monthly|list|doctor>` 行为与迁移前等价（退出码 0/1/130、提示、预览）。
- [ ] `requirements.txt` / `requirements-dev.txt` 已删除，依赖全部声明于 `pyproject.toml`，`uv.lock` 存在。
- [ ] `uv run pytest` 全绿；`uv run pytest` 为唯一测试入口。
- [ ] 三条报告命令通过 `ReportRunner.run`；CLI 不再直接编排 scheduler/store/renderer/LLM（验收即删旧编排）。
- [ ] `_ContextEngine` 已公开化；`scanner_config` 拆分后 `scanner_contract_profile()` 等价。
- [ ] 启动、测试、吞吐三项有迁移前后对比记录；未达标项有原因与处理说明。
- [ ] PDF 基准门禁产出证据文档，明确"迁移"或"维持现状"结论。
- [ ] Rust workspace tests、doctor --strict、cold/warm smoke 通过；Rust contract/scanner DB/fallback 无 diff。

## Out of Scope

1. Rust scanner 内核算法与性能预算重构（不含 4.3 的外层调用收紧）。
2. LLM provider、模型选择、prompt 与报告模板/JSON schema 变更。
3. 报告数据库 schema migration 与跨载体原子事务。
4. 新业务功能、Web/GUI/daemon。
5. 通用 workflow engine、plugin registry、middleware 或 dependency container（与既有 spec 一致）。
6. pytest-cov 覆盖率基线（需要时另行引入）。
