> 历史记录：保留当时的数字、状态和结论；当前运行与验收以[当前状态](../../current-state.md)为准。

# Rust 迁移行为与验收对照

当前运行状态（2026-10-17）：正式日报已切换至 `archive-05` 和归档来源，独立 `archive-02` 每 30 分钟采集；10 月 16 日旧扫描日报已恢复发布。18 日首份真实定时归档日报仍待验收，当前状态与证据见 [归档实施记录](../../plans/2026-10-17-change-archive.md)。以下测试数量和迁移记录保留各自阶段的原始口径。

代码基线：`069045f`。日期：2026-10-15。表中“既有证据”表示迁移前行为已经受测试覆盖；Rust 完成状态以执行计划任务表为准。

## 能力、调用方与替代位置

| 能力 | 现有入口与实现 | 输入和预期结果 | 既有自动化证据 | Rust 位置 |
| --- | --- | --- | --- | --- |
| F01 参数配置 | `main.py`、`src/cli/`、`src/core/config.py` | 固定时钟、显式日期、冲突参数、环境覆盖；保留上海自然日及退出码 | `test_cli_split`、`test_report_context`、`test_report_settings`、`test_config` | `report_app::{config,types,main}` |
| F02 扫描上下文 | `NativeScanner` → `scanner_core::Scanner` | 32 份合成 Markdown；冷扫与缓存来源一致，时间窗右端不包含 | `test_native_scanner`、`test_scan_window`；Rust `context_pipeline`、`production_session` | 复用 `scanner_core`，应用直接调用 |
| F03 Office 文本 | `office_parser`、`document_parser`、Python 文档进程 | `tests/fixtures/worker_documents` 中真实 DOC/PPT/XLS；保留预期文字、预算及错误处理 | `test_document_parser_worker`、`test_excel_format_audit`；Rust `worker_contract` | `office_parser`，DOC/PPT 接 `office_oxide`，XLS 接 `calamine` |
| F04 PDF | `workers/pdf.py`、分类与页审计 | `pdf_classifier` 和 `pdf_benchmark` 语料；中英文、选页、无文本、加密、损坏明确区分 | `test_pdf_classifier`、`test_pdf_page_audit`、`test_document_parser_worker` | `pdf_parser`，唯一 PDFium 路径 |
| F05 模型调用 | `model_plan`、`model_slicing`、`model_calls` | 固定请求与响应；瞬时重试、永久失败、未知调用及成功复用保留账本 | `test_model_budget`、`test_model_slicing`、`test_partial_response_reuse`、`test_gateway_thinking_location` | `report_app::model` |
| F06 三类报告 | `EvidenceDailyService`、`ReportRunner`、`report_gen` | `daily-v2`、`weekly-v1`、`monthly-v1`；固定日期、时钟和正文；新内容六类案例单独验收 | `test_evidence_daily_service`、`test_evidence_reports`、`test_report_runner`、`test_report_gen` | `report_app::{run,reports}` |
| F07 历史恢复 | `runs`、`repair-output`；`report_run_store`、`snapshots`、`publication` | 临时真实 SQLite；预览不发布、先提交后发布、损坏快照拒绝、外部改动冲突、修复零调用 | `test_report_recovery`、`test_partial_response_reuse`、`test_sqlite_store`、`test_scan_preview` | `report_app::{store,snapshots,publish}` |
| F08 补跑修复 | `scripts/backfill_daily_reports.py`；`coverage_repair`、`coverage_execution_recovery` | 日期范围、固定作业、候选发布、失败修订、部分成功及预算修订不丢失既有调用 | `test_backfill`、`test_coverage_repair`、`test_coverage_execution_recovery`、`test_failed_execution_amendment`、`test_execution_runtime_revision` | `report_app::maintenance` 的 `backfill` 和 `coverage` |
| F09 OCR | `--ocr-manifest`；`ocr_import`、`workers/ocr` | 清单、原文件和文字 SHA-256 对应；页号合法；未核实内容进入待确认 | `test_evidence_daily_service` 的 OCR 案例、`test_pdf_page_audit` | `maintenance::ocr`、`ocr validate` |
| F10 任务运维 | `daily_task_plan.py --sid`；安装、检查、移除任务的 PowerShell | 独立测试 SID 和配置；XML 触发器、单实例、程序路径与归属一致 | `test_daily_task`、`test_healthcheck` | `task plan` 及现有 PowerShell |
| F11 安装回退 | `build_windows_release.ps1`、`windows_release.py`、`update_release_pointer.ps1` | 隔离安装目录；清单哈希、依赖、版本指针和回退数据边界 | `test_windows_release_tools`、`test_windows_rust_core_e2e` | Cargo 三个程序、PowerShell 打包和原生安装测试 |

旧 CLI 参数的逐项清单保存在 `rust/report_app/tests/fixtures/legacy-cli-options.json`。`coverage recover / amend` 已接入恢复、运行配置修订和调用预算修订，保留原运行、成功响应及历史消耗。

## 冻结材料

`rust/report_app/tests/fixtures/` 保存合成正文、原模型请求和响应、输入快照 v2、报告库结构 v1、规范 JSON 特殊值以及六类内容案例。`manifest.json` 保存文件原字节 SHA-256。所有报告文字均为合成材料。旧报告格式和新内容目标分别保存，不修改已发布历史正文。

`scripts/export_rust_migration_baseline.py` 是开发期导出工具。它仅使用临时数据库、合成来源和固定网络替身。迁移结束后生产程序不依赖该工具。

## 基线结果

- Python 全套：首次 479 通过，2 项因 PATH 中缺少 `uv 0.12.0` 失败；将经 PyPI SHA-256 核对的固定版本工具放入项目 `.artifacts/rust-migration/tools/` 后，相关文件 3 项全部通过。合计 481 项通过，无业务基线失败。
- Rust 全套结果及数量见 `.artifacts/rust-migration/rust-baseline.log`。
- 合成性能数据见 `.artifacts/rust-migration/python-performance.json`；当前记录冷扫、热扫和固定响应报告耗时，最终比较需要使用同一输入与计时范围。进程峰值内存尚需补充独立进程测量。
- 正式日报、正式报告库、任务注册和模型网络调用均未参与基线执行。

## T05 原生扫描与解析

- 工作进程协议统一为 v3；Office 和 PDF 均使用独立 Rust 程序，拒绝 v2 进程及重复声明的操作。
- DOC/PPT 使用 `office_oxide`，真实 XLS 使用 `calamine`，PDF 使用随包且核验哈希的 PDFium。现代 Office 保留原有有界路径。依据真实格式选择一次解析器，错误扩展名不会触发多引擎尝试。
- 独立扫描库为 `scan_index_rust_v3.sqlite3`，使用专属 SQLite application_id；旧 Python 扫描库被原样拒绝。解析器构建、协议及固定 PDFium 输入进入缓存身份。
- `t05-final.log` 记录 312 项测试全部通过。覆盖中文及混排 PDF、选页、密码和损坏文件、DOC/PPT/XLS、文字和行数限制、错误扩展名、旧格式关闭、来源变更、进程回收、缓存复用及损坏文件后继续处理其他文件。
- T05 完成时，新 Rust 扫描链已不调用 Python，正式程序、任务和数据尚未切换。当前生产切换结果见本文 T12；旧生产源码保留至三个真实窗口验收通过后清理。

## T06 模型执行与成稿

- `report_app/model` 使用一个阻塞 HTTP 客户端及一处应用重试循环。自动重定向、客户端重试已关闭，发送冻结请求的原字节，保存响应原字节。
- 来源先获得一次输入机会，再分配更深内容；切片保留完整段落、表头与表格行，并记录 Unicode 字符范围。提示词与响应结构升级为 v3，版本及内容哈希参与请求身份。
- 全日成稿前为全部批次事实分配容量；成稿必须覆盖全部已选事实，引用来源须与事实对应。未核实文字进入待确认。容量不足明确失败，成稿不缩批、不截掉事实。
- 成功结果复用不增加调用；跨运行引用核对原调用整行哈希、所属日期、作业身份、请求原字节及来源范围。未知调用保留预约用量；响应保存失败停止发送。
- `.artifacts/rust-migration/t06-report-app.log`：报告包 38 项通过，其中 `model_execution.rs` 21 项。覆盖限流、超时、永久失败、一次缩批、保存失败、成功与部分成功复用、调用跨午夜、预算开关、冻结费用、输入公平分配及原生 HTTP 请求。`t06-clippy.log`：严格静态检查通过。
- 所有网络验证使用本机 HTTP 服务或固定响应测试替身，未调用正式模型。跨作业预算的显式修订链在 T08 接入；当前拒绝未经验证的预算变更。实际模型内容质量验收与固定响应回放分别记录。
- 现有请求上限为 16,000 字节。新流程会将该上限同时用于事实提取与完整成稿；材料较多而无法预留充分事实空间时，返回 `MODEL_COMPOSE_CAPACITY_INSUFFICIENT`，不会自动提高配置上限。

## T07 报告与真实内容验收

- 日报采用 v3 事实提取和全日成稿，模板按概览、事项、待确认、来源附录呈现。周报、月报均支持数据库与扫描来源，预览、空材料、历史修复和输出冲突保留明确行为。
- 六组虚构材料使用获准的 `deepseek-flash` 生成并逐项复核，结果见 [内容核对](../../../.artifacts/rust-migration/content-quality/review.md) 和同目录 `review.json`。其中包含每组成稿路径、运行编号、正文与输入哈希。

| 场景 | 本次复核结果 | 采用成稿 |
| --- | --- | --- |
| 同事项跨文件重复 | 相同审批规则合为一条，同时引用两份来源；草稿状态与生效日期分别保留 | `same_topic_across_sources/candidate-v7.md` |
| 复杂条件与例外 | 保留含税金额、50000 元边界、且与或关系、2 个工作日、书面确认及关联交易例外 | `conditions_and_exceptions/candidate-v5.md` |
| 术语解释 | 首次解释 BOM，说明缓存命中含义，保留草稿状态 | `explain_terms/candidate-v5.md` |
| 数值与统计口径 | 甲公司 120/90/75% 与乙公司 80 分开，币种、含税、申请单及撤销排除口径明确 | `numbers_with_scope/candidate-v5.md` |
| 提案和完成状态 | 文件修改日与拟上线日区分，安全评审前提、尚未开始和缺少上线记录均保留 | `proposal_status/candidate-v5.md` |
| 材料不足 | 只写已知主题，将负责人、数量、生效日期和金额表缺口集中列出 | `missing_material/candidate-v5.md` |

审计账本累计 89 次尝试：44 次成功，43 条 HTTP 错误记录，2 条响应校验失败。尝试次数不等于计费次数。用户已取消本次六组验收的累计次数上限；每次请求最多三次尝试保持有效。六组内容验收完成后不再发送真实模型请求。

五组采用 v5 成稿，跨文件案例采用最终提示词下的 v7 成稿；最后关于版本差异的提示词局部调整只对跨文件案例做了真实复验。部分样本仍有简短的状态复述，关键条件、数值和完成状态未发现失真。这是六组样本的内容复核结果，不将未知材料的质量标为已验收。

## T08 维护操作

- `backfill` 支持固定预检输入、日期范围、显式作业预算、模型并发、无模型模式及已完成日期复用。预检和备份先于生成；状态文件中断后的重跑不会重复创建作业或丢失原消费。
- `coverage plan / generate / publish / recover / amend` 保持准备、候选与发布分离。恢复记录组成可核对的修订链，第二次恢复保留第一次恢复及全部失败调用；成功事实跨运行复用仍核对原记录、请求和来源身份。
- `ocr validate / run` 校验原 PDF、文字 SHA-256、页数和范围。显式 OCR 引擎通过有截止时间、内存和取消控制的子进程调用；未核实文字留在待确认。
- `cleanup` 默认仅核对。明确执行时只处理有归属且达到条件的材料，保留未完成运行、正式报告、引用快照、活动补跑、未知文件及人工修改。
- 证据：`report_app/tests/{backfill,maintenance,ocr,retention,model_execution,legacy_recovery}.rs`，本轮完整测试记录包含以上案例。并发模型测试以明确线程同步验证两个事实请求乱序结束、成稿仍按原输入顺序执行。

## T09 运维与任务

- `task plan` 由 Rust 生成任务 XML；只读取状态，不注册任务。任务归属绑定持久配置目录的父目录，支持版本目录变更后的归属核对。
- `doctor --strict` 检查配置、输出目录、固定模板、数据库、同版 Office/PDF 进程及 PDFium，不联网。`doctor --check-model` 显式生成合成请求，通过现有快照与调用账本记录。
- PowerShell 安装、检查和移除脚本复用原有归属及读取核对规则。发布 EXE 使用 Windows 无控制台子系统；脚本通过原生管道等待退出并获取 JSON 和退出码。调度器直接执行 Rust EXE。
- SQLite 一致备份包含已提交 WAL 数据，版本指针通过原子替换保存前版。定时日志只记录运行编号、日期、结果和错误码。
- `t11-powershell-release-03.log`：发布版 EXE 配合当前运维脚本，10 项任务/指针检查通过；另验证构建脚本拒绝覆盖旧生产 `rust/target`。计划任务接口在测试中全部使用函数替身，实际任务变更为 0。

## T10 程序包与源码边界

- 程序包为三个 EXE、固定 PDFium DLL、模板、配置示例、运维脚本、操作文档、依赖许可证及逐文件 SHA-256 清单，约 26 MB。Rust EXE 使用静态 C 运行库。构建输出限定在独立目录，不覆盖仍供旧任务使用的解析程序。
- Cargo 活动成员及锁文件已排除 PyO3；程序包无 Python、uv、虚拟环境、`.pyd` 或旧生产脚本。开发期 Python 对照工具只用于冻结基线和比较证据。
- `README.md`、项目 `AGENTS.md`、部署与操作说明已经更新。旧说明保存在 `docs/archive/python-*-2026-10-15.md`。
- 正式入口已在 T12 切到独立 Rust 包，旧生产源码、旧解析程序及虚拟环境已归档并暂时保留。已替代源码在三个真实窗口通过后清理，用户环境和业务数据继续保留。源码清理尚未标为完成。

## T11 最终验证

| 检查 | 结果与证据 |
| --- | --- |
| 格式与严格 Clippy | 通过；`t11-clippy-final.log` |
| 完整 Rust 工作区 | 419 项通过，0 失败；独立安装测试默认忽略后另行执行。`t11-workspace-04.log` |
| 原生进程控制 | 13 项均在完整工作区中通过，测试助手也使用 Rust；超时、取消、内存/输出上限、并行管道及进程树回收均覆盖 |
| 独立程序包 | 最终封存包的 `t11-bundle-final.log` 通过；`bundle-acceptance-final/acceptance.json` 保存全部命令和加载库记录 |
| 无 Python 依赖验证 | 子进程清空继承环境，PATH 仅含 System32；主进程未加载 Python、`.pyd` 或外置 VCRUNTIME140，Office/PDF 同版握手通过 |
| 实际运行范围 | 日报、重复运行、删除输出后零调用修复、事实成功后成稿失败及恢复、周月报 db/scan、空日、调度入口、列表、补跑重复执行、一致备份、任务计划及只读清理 |
| 模型隔离 | 包验收共 10 次本机 HTTP 请求，0 次远程请求；原程序执行真实 HTTP 通信，未增加产品回放开关 |
| Windows 运维 | 最终封存包的 `t11-powershell-final.log` 中 10 项隔离检查通过，指针回退保留业务文件；真实任务操作另见 T12 记录 |

当前机器仍装有 Python，本机进程环境隔离和依赖检查已通过。用户于 2026-10-15 明确要求该项默认通过，因此 A01 以“用户豁免全新 Windows 实测、接受现有本机证据”验收通过，不再保留为待验证。全新 Windows 实测未执行。独立原生安装测试程序及 `scripts/accept_windows_bundle.ps1` 保留为可选复验工具，无需 Python、Cargo 或模型凭据。

### 最终交付

交付版本为 `rust-20261015-01`。以下文件均位于 `.artifacts/rust-migration/`：

- `ai-daily-report-rust-20261015-01.zip`：11,571,257 字节；解压后 347 个文件共 26,112,929 字节。压缩包中的每个文件已与发布清单重新核对。
- `ai-daily-report-acceptance-tools-20261015.zip`：1,456,750 字节；包含独立原生验收程序、PowerShell 入口、操作说明和哈希清单。
- `delivery.json`：压缩包及验收证据的 SHA-256、测试结果、已确认回退范围和剩余验收项。
- `final-native-dependencies.json`：三个 EXE 及 PDFium 的 Windows 依赖记录，没有 Python 或外置 VCRUNTIME140 依赖。
- `production-preservation-final.json`：77 个旧运行文件及正式任务 XML 与切换准备材料一致，Rust 正式部署目录和正式配置尚未创建。

源码改动尚未提交，发布清单明确记录 `source_dirty=true`。封存包保留构建时文档；本执行记录继续追踪封包后的验收和正式切换状态。

### 同机性能对照

相同的 32 份 Markdown、DOCX 和 PDF 测试文件，各做五组冷/热扫描；报告使用同一业务材料及固定本地模型响应。计时均不含对象初始化，报告计时不含扫描和网络等待。旧报告执行一次摘要，新报告执行事实提取与一次成稿，分别保留实际成本。

| 项目 | Python 中位 / 最慢，ms | Rust 中位 / 最慢，ms | 中位变化 |
| --- | --- | --- | --- |
| Markdown 冷扫描 | 480.967 / 599.581 | 402.724 / 498.841 | -16.27% |
| Markdown 缓存命中 | 82.726 / 83.446 | 59.099 / 60.448 | -28.56% |
| DOCX 冷扫描 | 130.923 / 135.766 | 89.112 / 95.372 | -31.94% |
| DOCX 缓存命中 | 15.268 / 16.213 | 14.582 / 29.196 | -4.49% |
| PDF 冷扫描 | 992.021 / 1181.450 | 121.427 / 131.863 | -87.76% |
| PDF 缓存命中 | 18.010 / 30.637 | 15.183 / 17.399 | -15.70% |
| 固定响应完整报告 | 171.373 / 187.228 | 178.011 / 180.306 | +3.87% |

解析工作进程的观测峰值：Markdown 8,040,448 → 8,998,912 字节，DOCX 8,019,968 → 8,998,912 字节，PDF 41,775,104 → 8,994,816 字节。热缓存阶段没有解析工作进程记录。未改解析路径的中位耗时和观测峰值均未触发计划中的 10% / 20% 阻断阈值。PDF 已更换解析引擎，其性能变化与格式语料结果分别记录。

原始数据：`python-performance-five.json`、`rust-performance-five.json`、`performance-comparison.json`；可重复执行代码为开发期 `measure_python.py` 和 Rust `examples/performance.rs`。现用旧 `.venv` 的文件逻辑大小为 207,973,213 字节；它包含既有环境，不将其冒充旧可分发安装包大小。

### 产品验收矩阵

| 编号 | 当前结论 | 主要证据 |
| --- | --- | --- |
| A01 | 通过；2026-10-15 用户豁免全新 Windows 实测 | 已通过的本机独立包、隔离进程及加载库检查，加用户明确验收决定；全新系统实测未执行 |
| A02 | Rust 功能对应项通过，正式任务已切换 | F01—F11 对照、419 项测试、实际任务配置回读 |
| A03 | 日期和命令行为通过；真实调度归入 A12 | `cli_config`、`daily_pipeline`、`period_pipeline`、`operations` |
| A04 | 固定回放与六组内容复核通过 | `report_templates`、`report_fixture`、内容核对及原字节哈希 |
| A05 | 通过 | PDF `corpus`、Office `worker_contract / legacy_capability`、扫描集成 |
| A06 | 通过 | 23 项 `model_execution`、预算修订、冻结请求及本机 HTTP |
| A07 | 通过 | `state_recovery / publication / legacy_recovery`，成稿故障后单步恢复 |
| A08 | Rust 读取旧数据通过；Python 拒绝新 v3 日报，用户已确认由保留的 Rust 包处理新格式 | v1/v2 冻结材料、历史恢复、`rollback-readability.json` |
| A09 | 通过 | 13 项 `worker_process`、原生文档故障与 OCR 取消 |
| A10 | 通过 | 构建/协议/引擎身份、独立扫描库及冷热来源一致性 |
| A11 | 通过；发布包、生产自检、一致备份、实际任务回退和历史数据保留均已核对，回退范围已确认 | 发布清单、真实任务回读与回退 XML、`preservation-verification.json` |
| A12 | 正式任务已切到 Rust，三个真实窗口待验收，0/3 | 计划从 10 月 16 日 09:00 开始；[切换记录](2026-10-15-rust-cutover.md)及 `scheduled-acceptance.json` |

A12 的真实窗口尚未发生，不能预填通过。T12 的生产操作与正常日报费用已于 2026-10-15 获得用户独立明确授权，生产模型检查与六组虚构材料验收分别保留账本。

## T12 正式切换

2026-10-15 14:33（北京时间）正式任务已切到 `.releases/rust-20261015-01/ai-daily-report.exe`，配置为 `config-rust`。任务已启用、状态 `Ready`，读取的全部受核对字段与计划一致，任务历史保持开启。

| 项目 | 已核对结果 |
| --- | --- |
| 备份 | `data/backups/rust-cutover-20261015-01/`，13,046 个文件、414,686,076 字节，文件哈希逐项一致；报告库与旧扫描库的 SQLite 一致备份通过完整性检查 |
| 正式自检 | Rust 主程序、八份模板、同版 Office/PDF 解析进程、PDFium 和数据库通过严格自检 |
| 正式扫描预检 | 目标日 2026-10-14，5 条来源记录，0 次模型调用；材料保存在正常快照目录，日志不保存原文 |
| 合成模型检查 | 运行 `f83f7d2cd129ab4b080b0733bd790927` 成功；3 次尝试包含 2 条 HTTP 失败、1 条成功，后续任务操作复用本次检查，未重复调用 |
| 实际回退 | 在任务禁用状态下实际注册 Rust 定义、恢复旧定义并读取核对，最后重新注册并启用 Rust；演练未启动日报、未恢复旧数据库 |
| 历史保留 | 217 条既有运行、699 条既有模型调用和 1,838 个历史数据文件保持原样；旧扫描库哈希不变，报告库结构仍为 v1 |
| 扫描库隔离 | `scan_index_rust_v3.sqlite3` 已独立创建，结构版本 3、`application_id=1094996531` |
| 后续验收 | 首次触发 2026-10-16 09:00；计划三个窗口为 16—18 日。自动跟进 `rust` 每日 10:45 核对，仅有实际事件和报告证据才计为通过 |

原始记录位于 `.artifacts/rust-migration/cutover-20261015-01/`：`preflight.json`、`database-backups.json`、`deployment.json`、`doctor-strict.json`、`scan-preflight.json`、`doctor-model.json`、`task-rollback.json`、`activation.json`、`preservation-verification.json`、`task-check-final.json` 及切换前后 XML。备份配置已去除明文密钥；新程序继续读取现有用户环境凭据。

旧 V27 的窗口记录与自动跟进保持独立，不计入 Rust 切换后的 3/3。旧源码清理留到本次三个连续真实窗口通过后执行。
