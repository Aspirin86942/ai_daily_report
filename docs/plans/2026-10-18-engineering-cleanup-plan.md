# 工程清理实现计划（子项目 E）

- 设计：[2026-10-18-engineering-cleanup-design.md](2026-10-18-engineering-cleanup-design.md)
- 日期：2026-10-18
- 基线：`readability` 分支 `c126bd5`（B 已提交、未合并）；`main` 仍为 `204cd7d`

## 全局约束

- 只做零引用对象的删除与文档整理；不删除旧包、Python 源码、`scanner_native`、`pdf_extract_bench`、`.venv`、配置备份、`data/`、`.artifacts/`、`.releases/`、`config-rust/`。
- 不推送 Git，不合并 `main`，不改正式任务与配置，不调用真实模型。
- 删除前逐一确认路径与引用数；删除记录写入本计划自检表。
- 最终闸门：`cargo fmt --manifest-path rust/Cargo.toml --all -- --check`、`cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings`、`cargo test --manifest-path rust/Cargo.toml --workspace --locked`。

## 任务 1：文档状态与计划索引

1. `docs/plans/2026-10-17-scheduled-reliability-design.md` 状态改为「已实现并本地合并（`204cd7d`），待授权部署验收」。
2. `docs/plans/2026-10-18-readability-design.md` 状态补提交号 `c126bd5`。
3. 新增 `docs/plans/README.md`：表格列出 `docs/plans/` 全部设计/计划文件（含本文件），列为：文件、状态、日期、范围一句话、入口/提交。链接必须真实存在。
4. `docs/current-state.md` 增加「仓库内已完成、未上线」小节：可靠性 `204cd7d`、可读性 `c126bd5`；说明均不在 `rust-20261017-archive-05` 内，不影响 10-18 09:00 验收，部署需单独授权。原有正式任务、验收、证据内容不改。

## 任务 2：脚本归属说明

新增 `scripts/README.md`，覆盖 `scripts/` 下全部 21 个文件（含本 README 自身除外）。每个脚本一行：名称、用途、入口/前置条件、分类。分类固定为：生产/发布、计划任务、验收证据、旧 Python 保留。至少说明：

- `build_windows_release.ps1`、`verify_windows_release.ps1`、`windows_release.py`、`release_common.ps1`、`prepare_rust_dependencies.ps1`、`update_release_pointer.ps1`：生产/发布。
- `install_daily_task.ps1`、`remove_daily_task.ps1`、`check_daily_task.ps1`、`daily_task_common.ps1`、`daily_task_plan.py`、`health_notify.ps1`：计划任务与健康巡检。
- `accept_windows_bundle.ps1`、`accept_archive_bundle.py`、`archive_scanner_database.ps1`、`export_rust_migration_baseline.py`、`coverage_optimization_evidence.py`、`coverage_release_evidence.py`、`verify_coverage_corpus.py`、`benchmark_archive.py`、`benchmark_scanner.py`：验收证据/测量，含无引用脚本，标注「保留不删」。
- `backfill_daily_reports.py`：旧 Python 运行时补录，保留不用于生产。

## 任务 3：一次性产物清理与忽略规则

删除（`git rm`）：

- `tmp/dump_candidates.py`、`tmp/inspect_schema.py`、`tmp/list_tables.py`、`tmp/read_zjl.py`、`tmp/scan_nodes.py`、`tmp/scan_pay.py`、`tmp/search_oa.py`、`tmp/search_old_dbs.py`、`tmp/search2.py`。
- `outputs/01a12bb1-deae-7dd3-88d9-90c066aa3a56/历史日报错误分析_2026-04-12_2026-10-11.xlsx.inspect.ndjson`。
- `.agents/worktrees/.gitignore`（若目录因此变空，目录本身不必保留）。

保留：`outputs/01a12bb1-deae-7dd3-88d9-90c066aa3a56/历史日报错误分析_2026-04-12_2026-10-11.xlsx`（归档文档按文件名引用）。

`.gitignore` 增加：

```text
# 临时探查与生成 dump
/tmp/
*.inspect.ndjson
```

删除前用全仓检索确认零引用（文档、脚本、测试、代码）。

## 任务 4：术语补充

`CONTEXT.md` 增加「要素速览」「变化依据」「健康巡检」三个条目，定义与实现一致（见设计 E4）；不改其他条目。

## 任务 5：验证与记录

1. 运行全量 fmt/clippy/test，记录结果（应与 B 基线一致：490 项通过）。
2. `git status` 只含本计划范围；删除文件数、保留文件数与理由填入本计划自检表。
3. 检查 `docs/plans/README.md` 与 `scripts/README.md` 的链接/覆盖完整。
4. 不合并、不推送。

## 自检记录

| 项目 | 结果 | 说明 |
| --- | --- | --- |
| 引用检索 | 通过 | 逐项检索根 README/AGENTS/CLAUDE/CONTEXT、`docs/**/*.md`、`scripts/**`、`rust/**/*.rs` 及测试；11 项除本清理设计/计划的删除登记外，使用引用均为 0。删除前登记数见下表；脚本还检查模块导入名，4 个无其他使用引用的顶层脚本均标注保留 |
| 删除清单 | 完成 | `git rm` 10 个跟踪文件；另删除原本未跟踪且自忽略的 `.agents/worktrees/.gitignore`，共 11 项、7,071,765 字节。原工作簿保留且 SHA-256 未变；`.agents/` 空目录保留，其他目录未删除 |
| 文档索引 | 通过 | plans 索引覆盖 8/8（7 份专项文档及索引自身），全部 9 个相对链接存在；scripts 顶层实查 22/22，递归跟踪文件 31/31，全部 33 个相对链接存在；原计划 21 个为计数偏差 |
| 全量闸门 | 通过 | 按完整 cargo 路径执行 fmt/clippy/test，均退出码 0；54 组结果共 489 项通过、0 失败、1 项独立发布包验收按既有 `#[ignore]` 跳过，总计 490 项；与计划“490 项通过”的口径差异见下文。工作树及暂存区 `git diff --check` 均通过 |
| 遗留风险 | 已记录 | 真实部署与定时验收未执行，需单独授权；独立发布包验收未运行。正式任务、配置、数据、旧包、Python、原生扫描核心及证据保留。改动提交在 `engineering-cleanup` 分支 HEAD（未合并、未推送），由主代理复核后决定合并 |

### 删除前引用核对与实际删除记录

下表“清理登记”仅统计删除前两份工程清理文档中的文件名或完整路径提及，供核对删除范围；“使用引用”统计上述检索范围内其余文档、脚本和代码。清理文档已有删除清单，保留这些登记以便追溯。

| 目标 | 字节 | 删除前清理登记 | 使用引用 | 实际动作与恢复方式 |
| --- | --- | --- | --- | --- |
| `tmp/dump_candidates.py` | 1,088 | 2 | 0 | `git rm`；Git 基线可恢复 |
| `tmp/inspect_schema.py` | 339 | 1 | 0 | `git rm`；Git 基线可恢复 |
| `tmp/list_tables.py` | 346 | 1 | 0 | `git rm`；Git 基线可恢复 |
| `tmp/read_zjl.py` | 483 | 1 | 0 | `git rm`；Git 基线可恢复 |
| `tmp/scan_nodes.py` | 1,085 | 1 | 0 | `git rm`；Git 基线可恢复 |
| `tmp/scan_pay.py` | 1,706 | 1 | 0 | `git rm`；Git 基线可恢复 |
| `tmp/search_oa.py` | 1,263 | 1 | 0 | `git rm`；Git 基线可恢复 |
| `tmp/search_old_dbs.py` | 1,044 | 1 | 0 | `git rm`；Git 基线可恢复 |
| `tmp/search2.py` | 1,228 | 1 | 0 | `git rm`；Git 基线可恢复 |
| `outputs/01a12bb1-deae-7dd3-88d9-90c066aa3a56/历史日报错误分析_2026-04-12_2026-10-11.xlsx.inspect.ndjson` | 7,063,181 | 1 | 0 | `git rm`；Git 基线可恢复 |
| `.agents/worktrees/.gitignore` | 2 | 3 | 0 | 原本未跟踪，内容为 `*` 加 LF 换行；用文件补丁删除，可按原内容重新创建 |

10 个跟踪文件均核对存在于 `c126bd5`；如需恢复，针对选定单个路径执行 `git restore --source=c126bd5 --staged --worktree -- <目标相对路径>`。恢复操作未执行。

保留的 `outputs/01a12bb1-deae-7dd3-88d9-90c066aa3a56/历史日报错误分析_2026-04-12_2026-10-11.xlsx` 被 `docs/archive/history/AI_Daily_Report_错误与覆盖优化方案_v1.0.md` 引用。删除前后 SHA-256 均为 `7c207e281cfe2028263541b5744358d31c547dc590e54b2143f401dfc8155e06`。

### 全量闸门证据与偏差

2026-10-18 执行，三个命令均使用 `& "$env:USERPROFILE\.cargo\bin\cargo.exe"`：

| 闸门 | 参数 | 结果 | 耗时 |
| --- | --- | --- | --- |
| fmt | `fmt --manifest-path rust/Cargo.toml --all -- --check` | 退出码 0 | 1.48 秒 |
| clippy | `clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings` | 退出码 0，无告警 | 0.69 秒 |
| test | `test --manifest-path rust/Cargo.toml --workspace --locked` | 退出码 0，489 通过、0 失败、1 忽略 | 66.23 秒 |

完整日志位于本机临时目录 `C:\Users\lcf\AppData\Local\Temp\ai-daily-report-engineering-cleanup-20261018-db3a9b5a6fc647c88c94bf7e9591c822/`，分别为 `fmt.log`、`clippy.log`、`test.log`。

实际结果与原计划的差异：

- 脚本顶层实际为 22 个，已全部覆盖；附加索引原有 `pdf_benchmark/` 的 6 个脚本和 3 份结果，共 31 个原有文件，未改动这些脚本或结果。
- `.agents/worktrees/.gitignore` 未被 Git 跟踪，无法通过 `git rm` 移除；按明确授权删除该文件并记录可重建的原内容，空目录保留。
- 删除目标在两份清理文档中已有登记；逐一核对使用引用为 0，登记引用如实保留。
- 测试总计 490 项，其中 `standalone_bundle_runs_reports_recovery_and_scheduling_without_python` 需 `AI_DAILY_TEST_BUNDLE`，按现有忽略标记跳过；实际通过数为 489，未将跳过项计作通过。
- 原设计的“git status 干净”和“一次提交”按本次明确要求调整为保留待审查工作树；5 个已跟踪文件修改、10 个跟踪文件暂存删除、4 个未跟踪文档，另有 1 个忽略文件的本地删除。未执行 Git 提交或分支操作。

### 保护检查与遗留风险

- 自动比对确认 `current-state.md` 除新增小节外全文保持不变；`CONTEXT.md` 原有条目文本保持不变。
- 两份索引所有相对链接均存在，plans 的表格项目与本目录全部 Markdown 文件一致，scripts 的表格项目与原有递归跟踪文件一致。
- `git status --short`、`git diff --stat`、`git diff --cached --stat` 和 `git diff --stat HEAD` 已核对；已跟踪差异合计 15 个文件、20 行新增、22,266 行删除。4 个未跟踪文档和原本忽略的脚手架删除另列，未额外暂存文档。
- 本轮无 Rust、模板、依赖或生产配置改动；`data/`、`.tmp/`、`.releases/`、`config-rust/`、`.artifacts/`、`.venv/`、`rust/scanner_native`、`rust/pdf_extract_bench`、`src/` 保持原状，未运行生产任务或真实模型。
- 本地闸门不包含实际生产部署和 2026-10-18 09:00 定时验收；运行与部署状态继续以 `docs/current-state.md` 的正式任务和独立验收证据为准。测试原始日志位于临时目录，主代理审查前应保留。
