# 工程清理设计（子项目 E）

- 状态：已实现，待用户验收
- 日期：2026-10-18
- 范围：仓库内文档状态与索引、脚本归属说明、一次性产物与忽略规则、术语补充；不含旧包/Python/.venv/扫描核心的删除，不含代码重构
- 关联：[当前状态](../current-state.md)、[可靠性设计](2026-10-17-scheduled-reliability-design.md)、[可读性设计](2026-10-18-readability-design.md)

## 一、背景与证据

2026-10-18 对仓库（`git ls-files` 669 个文件）只读盘点：

| 现象 | 证据 |
| --- | --- |
| 计划文档状态漂移 | `2026-10-17-scheduled-reliability-design.md` 仍写「待用户审查」，实际已实现并本地合并（`204cd7d`） |
| `current-state.md` 不反映仓库进度 | 只记正式任务与 10-18 验收，未说明可靠性、可读性改进已在仓库、不在生产包 |
| 计划文档无索引 | `docs/plans/` 5 份设计/计划（含 10-18 可读性）没有任何入口索引或状态表 |
| 脚本无归属说明 | `scripts/` 顶层 22 个脚本（另有 `pdf_benchmark/` 子目录 9 个文件），6 个在文档/代码中没有引用（`accept_archive_bundle.py`、`coverage_optimization_evidence.py`、`coverage_release_evidence.py`、`verify_coverage_corpus.py` 等），新旧 Python 运行时脚本与生产脚本混放 |
| 一次性产物入库 | 跟踪 `tmp/*.py` 9 个临时库/OA 探查脚本（连接 `data/db`、读取个人 Excel），互不引用；`outputs/<uuid>/历史日报错误分析_*.xlsx.inspect.ndjson` 7.0 MB 生成 dump，无引用 |
| 临时目录未忽略 | 根 `.gitignore` 不含 `tmp/`、`*.inspect.ndjson`，同类临时文件可再次被提交 |
| 术语表缺新概念 | `CONTEXT.md` 未收录「要素速览」「变化依据」「健康巡检」 |
| 死代码仅 8 处显式豁免 | `#[allow(dead_code)]` 分布在 `discovery`（2）、`scanner_core`（6），属扫描核心保留路径，不清理 |
| 依赖无闲置 | 工作区 15 个直接依赖均有实际引用（`glob`、`quick_xml`、`pdfium_render`、`serde_saphyr` 等逐一核对） |

历史证据日志位于未跟踪的 `.tmp/`（被 `docs/archive/history/daily-report-implementation-evidence.md` 引用），不在清理范围。

## 二、目标与非目标

**目标**

1. 文档状态与实际一致：计划文档标注实现/合并状态，`current-state.md` 增加「仓库内已完成、未上线」小节。
2. 可发现：新增 `docs/plans/README.md` 索引与 `scripts/README.md` 归属表。
3. 仓库卫生：移除一次性产物与临时脚本（保留在 Git 历史中），补充忽略规则，避免再次入库。
4. 术语补齐：`CONTEXT.md` 收录速览、变化依据、健康巡检。
5. 全量验证：不改代码行为，`fmt`/`clippy`/`test` 结果与清理前一致。

**非目标**

- 不删除旧发布包（`.releases/`）、旧 Python 源码（`src/`）、`scanner_native`、`pdf_extract_bench`、`.venv`、配置备份、业务数据、`.artifacts/` 证据。
- 不改扫描/解析核心、不动 `#[allow(dead_code)]`、不做依赖或 API 重构。
- 不改 `docs/archive/` 下的历史记录，不改正式任务与配置。
- 不移动 `outputs/*.xlsx`（历史分析证据，归档文档按文件名引用）。

## 三、方案

三项独立、可回退的清理，一次提交：

1. **文档（E1/E4）**：改 3 份文档的现状描述，新增 2 个索引/说明文件，补 3 条术语。
2. **产物（E3）**：删除 9 个 `tmp/*.py`、1 个 7 MB `*.inspect.ndjson`、1 个遗留 `.agents/worktrees/.gitignore`；在 `.gitignore` 增加 `tmp/` 与 `*.inspect.ndjson`。
3. **保留清单（E5）**：在 `scripts/README.md` 与设计文档中写明保留项与理由，避免后续误删。

删除只作用于工作树，文件仍在 Git 历史中；不涉及数据目录与发布包。

## 四、详细设计

### E1 文档状态与索引

- `2026-10-17-scheduled-reliability-design.md`：状态改为「已实现并本地合并（`204cd7d`），待授权部署验收」。
- `2026-10-18-readability-design.md`：保持「已实现，待用户验收」，补充提交号 `c126bd5`。
- 新增 `docs/plans/README.md`：表格列出 5 份设计/计划（变更归档、可靠性、可读性）的状态、日期、一句话范围、对应提交或验收入口。
- `current-state.md` 新增「仓库内已完成、未上线」小节：列出可靠性 `204cd7d`、可读性 `c126bd5`，说明两者不在 `rust-20261017-archive-05` 内、首份 v4 验收不受影响、部署需单独授权；保留原有正式任务表述。

### E2 脚本归属说明

新增 `scripts/README.md`，按四类列出全部 22 个顶层脚本（另附 `pdf_benchmark/` 子目录 9 个文件），每行：脚本、用途、入口/前置条件、状态（生产/发布、计划任务、验收证据、旧 Python 保留）。对无引用脚本明确标注「历史证据脚本，保留不删」，避免下次盘点重复判断。

### E3 一次性产物与忽略规则

| 目标 | 动作 | 依据 |
| --- | --- | --- |
| `tmp/dump_candidates.py` 等 9 个 | `git rm` | 一次性库/Excel 探查脚本，互不引用，无文档引用 |
| `outputs/01a12bb1-…/历史日报错误分析_*.xlsx.inspect.ndjson` | `git rm` | 7.0 MB 生成 dump，无任何引用 |
| `outputs/01a12bb1-…/历史日报错误分析_*.xlsx` | 保留 | 归档文档按文件名引用，属历史分析证据 |
| `.agents/worktrees/.gitignore` | `git rm` | 遗留工作树脚手架，无引用 |
| `.gitignore` | 增加 `tmp/`、`*.inspect.ndjson` | 防止同类临时文件再次入库 |

### E4 术语补充

`CONTEXT.md` 增加：

- **要素速览**：日报标题下的一行统计，给出材料、事实与缺口数量；`no_changes` 不输出。
- **变化依据**：归档日报（格式 v4）的审计附录，逐条保留事项标识、版本、位置、原文与引用 SHA-256。
- **健康巡检**：只读检查采集连续性、发布哈希、运行状态、解析队列、磁盘与数据库完整性，不调用模型。

### E5 保留项与理由

- `rust/scanner_native`（PyO3，旧 Python 运行时回退）、`rust/pdf_extract_bench`（PDF 测量基线）：不在工作区成员内，保留。
- `#[allow(dead_code)]` 8 处：扫描核心保留路径，删除会触发告警且无运行收益，保留并在设计中记录。
- `tmp/*.py` 之外的历史证据日志在未跟踪的 `.tmp/`，保留在本机，不纳入仓库。

## 五、错误处理与边界

| 场景 | 处理 |
| --- | --- |
| 删除后仍有引用 | 先全仓检索引用；仅删除零引用文件，索引文档中说明保留项 |
| 误删历史证据 | 只删工作树副本，Git 历史可恢复；`outputs/*.xlsx` 与 `data/` 不动 |
| `current-state.md` 与本轮验收冲突 | 新小节只描述仓库状态，不改正式任务表述与验收条件 |
| 文档与代码不一致 | 以实际代码与 `cargo test` 结果为准，不写未验证结论 |

## 六、测试与验收

1. 全量 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked` 与清理前一致（490 项通过）。
2. `git status` 干净；删除文件均可从历史恢复；无 `data/`、`.releases/`、`config-rust/` 改动。
3. 文档检查：`docs/plans/README.md` 链接全部存在；`scripts/README.md` 覆盖 `scripts/` 全部文件；`CONTEXT.md` 新术语与实现一致。
4. 不调用真实模型，不改正式任务与配置。

## 七、风险与已定决策

| 风险 | 处理 |
| --- | --- |
| 删除临时脚本影响可追溯性 | 脚本仍在 Git 历史；`scripts/README.md` 记录「历史证据脚本已移除」 |
| 索引文档随代码变化过期 | 索引只写状态与入口，不复制实现细节；每次子项目收尾更新 |
| 文档改写超出实际 | 以提交号与测试结果为准，不声称未上线的功能已生效 |

已定决策：

- 清理只做零引用对象，宁可保留也不误删证据。
- 死代码豁免与扫描核心不动，E 只记录不重构。
- 不合并 B 分支到 `main`，等待用户验收（本轮只做 E 的提交）。
