# 轻量清理设计(2026-08-10)

## 背景与目标

项目实际规模远超 CLAUDE.md 描述:Python 侧约 8k 行(含 `cli/`、`workers/`、`report_runner/`),
Rust scanner 侧约 39k 行。近期大量 commit 集中于 scanner 的 benchmark、验收证据与门禁加固,
同时部分文档已明显过期(CLAUDE.md 结构列出的 4 个旧 Python scanner 文件已删除、
AGENTS.md 仍引用不存在的 `requirements.txt`),本地 6 个性能边界测试必红。

**目标**:在不动架构、不砍功能的前提下,把项目整理干净——文档准确、死代码清空、
本地测试全绿(门禁以 skip 而非红牌呈现)。保留全部生产链路、证据体系与验收语义。

**决策记录**(与用户逐条确认):

| # | 决策 | 选择 |
|---|---|---|
| 1 | 精简方向 | 轻量整理,不动架构(不删 Rust、不砍功能、不删证据体系) |
| 2 | `docs/superpowers/` 42 份已完成设计/计划文档 | 移至 `docs/archive/{specs,plans}/` |
| 3 | 6 个本地必红的性能门禁测试 | 本地 skip(`perf_gate` marker),CI 全跑,验收语义不变 |
| 4 | `CONTEXT.md` 术语表 | 保留,实施时核对术语与代码一致 |
| 5 | 执行方案 | 分阶段(P1 文档 → P2 死代码 → P3 门禁 → P4 依赖+全量),每阶段独立 commit、可回退 |

## 清理清单

### A. 文档层(已核实,直接执行)

| 项 | 现状 | 处理 |
|---|---|---|
| `docs/superpowers/specs/`(24 份) | 已完成的历史设计文档,零交叉引用 | `git mv` → `docs/archive/specs/` |
| `docs/superpowers/plans/`(18 份) | 已完成的历史计划文档,零交叉引用 | `git mv` → `docs/archive/plans/` |
| `CLAUDE.md` | Project Structure 列 4 个已删除文件(`file_scanner.py`/`scan_discovery.py`/`scan_planner.py`/`office_parser.py`);scan 描述与 ADR 0002 一致性需核实 | 结构更新为现状树(含 `cli/`、`workers/`、`report_runner/`、`context_engine` 等);scan 描述与 ADR 0002 + `docs/scanner-backends.md` 对齐 |
| `AGENTS.md` | 命令写 `pip install -r requirements.txt`(文件不存在) | 改为 uv 命令(`uv sync`、`uv run python ...`),逐条核对其余内容 |
| `CONTEXT.md` | 术语表,唯一引用者将被归档 | 保留,核对术语仍与代码一致 |

**保留不动**:`docs/adr/`(0001、0002)、`docs/contracts/`(16 个 schema)、
`docs/scanner-backends.md`、`docs/windows-deployment.md`、`.artifacts/` 证据、
`README.md`(如有过时路径在 P4 顺带修正)。

### B. Python 死代码(P2,逐个验证后删除)

- 系统性 import 扫描(`src/`、`tests/`、`scripts/`),候选:未使用 import、无引用模块/函数、未被引用的测试 fixture。
- **删除规则**:每个候选删除前 grep 全仓确认零引用,删除后 pytest 全绿;拿不准的保留。

### C. 依赖核对(P4)

- 已确认有使用:`sharepoint-to-text`、`pandas`、`pdfplumber`、`pypdfium2`、`openpyxl`、
  `python-pptx`、`python-docx`、`reportlab`(dev,测试用)。
- 实施时以 import 扫描兜底核对,预计此项动作很小。

## 性能门禁「本地 skip」机制(P3)

现状:6 个性能边界测试无环境检测,本地必红(如 Python 冷启动 3.2s > 2s 预算);
CI(GitHub Actions `windows-release.yml`)上通过。`conftest.py` 已有
`rust_release_binaries` 的"缺失时由依赖方决定跳过"模式。

**设计**:一处判定 + 一个 marker,测试只声明"我是门禁"。

1. `pyproject.toml` 注册 marker:`markers = ["perf_gate: 性能门禁,仅 CI 全跑"]`。
2. `conftest.py` 加 `pytest_collection_modifyitems` 钩子:
   `CI` 环境变量或显式 `RUN_PERF_GATES=1` 时全跑;否则对每个 `perf_gate` 测试加 skip,
   reason 注明"性能门禁仅 CI 全跑(本地环境与验收预算不符)"。
3. 具体测试以本地运行 pytest 观察失败集为准打标;判据 = 断言时间/性能预算的测试。
   预期数量 ≈ 6。

**效果**:本地 `pytest` 全绿(6 个显示 skipped);CI 行为零改动;验收语义不变。

## 执行阶段与验证

| 阶段 | 动作 | 验证 |
|---|---|---|
| P1 文档 | 建 `docs/archive/` + `git mv` 42 份文档;更新 CLAUDE.md、AGENTS.md;核对 CONTEXT.md 术语 | 无残留引用;`pytest` 全绿(本阶段不应影响测试) |
| P2 死代码 | import 扫描 → 逐个 grep 验证零引用 → 删除(记录证据) | 每批删除后 `pytest` 全绿 |
| P3 门禁 | marker + conftest 钩子 + 目标测试打标 | 本地 `pytest` 全绿且目标测试显示 skipped |
| P4 依赖+全量 | 依赖使用核对;README 过时路径修正(如有) | `uv sync` + `uv run python main.py doctor --strict` + `uv run pytest` + `cargo test --workspace --locked` 全绿 |

每阶段独立 commit,可回退。Rust 代码零改动(除 cargo 自身门禁外)。

## 风险与回退

- **误删死代码**:以 grep 零引用 + pytest 全绿双闸把关;每阶段独立 commit,可 `git revert`。
- **门禁误标**:marker 只加在断言预算的测试上;CI 判定逻辑不变,本地可用
  `RUN_PERF_GATES=1` 强制全跑。
- **文档不一致残留**:P1 收尾 grep 全仓确认无对已删/已移路径的引用。

## 自归档说明

本设计文档及其产生的实施计划属于"已完成的设计文档",P1 归档时一并移入
`docs/archive/`,与规则一致,不设例外。
