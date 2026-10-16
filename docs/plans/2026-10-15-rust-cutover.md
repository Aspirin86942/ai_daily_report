# Rust 正式切换材料

日期：2026-10-15。状态：14:33（北京时间）已正式切到 Rust，三个真实调度窗口待验收，当前 0/3。

用户已明确授权本次生产备份、部署、合成模型自检、任务更新及切换后的正常日报调用费用。正式计划任务现已直接运行 Rust；旧源码、`.venv`、旧解析程序、原配置与业务数据库均保留，旧运行环境已归档。

## 已执行结果

- 切换时正式任务 `AI Daily Report` 已启用，状态 `Ready`，配置回读无差异，首次触发安排为 2026-10-16 09:00（北京时间）；任务历史保持启用。
- 备份目录为 `data/backups/rust-cutover-20261015-01/`，共 13,046 个文件、414,686,076 字节。报告库与旧扫描库完成 SQLite 一致备份及完整性检查；其余文件逐项核对 SHA-256。归档配置去除了密钥，凭据继续使用现有用户环境。
- 正式配置的严格自检通过；2026-10-14 扫描预检记录 5 个来源，模型调用为 0。合成模型检查 3 次尝试后成功，账本保留 2 条失败和 1 条成功记录。
- 实际任务完成 Rust → 旧版 → Rust 的定义切换与回读验证。演练阶段始终禁用触发，未启动日报，未恢复旧数据库，最后启用 Rust 任务。
- 切换后核对：217 条历史运行、699 条历史模型调用和 1,838 个历史数据文件保持原样；旧扫描库哈希不变，新 Rust 扫描库独立建立。
- 证据目录为 `.artifacts/rust-migration/cutover-20261015-01/`，包含 `activation.json`、`preservation-verification.json`、`task-check-final.json`、备份记录及切换前后任务 XML。
- 自动跟进 `rust` 每日北京时间 10:45 核对真实窗口。10 月 16 日窗口失败，连续通过计数从后续真实触发重新计算；当前仍为 0/3，后续候选窗口为 10 月 17—19 日。实时计数保存在同目录 `scheduled-acceptance.json`。旧 V27 窗口单独保留。

## 当前状态与目标

| 项目 | 切换前 | 当前 Rust 配置 |
| --- | --- | --- |
| 计划任务 | `AI Daily Report`，每日上海时间 09:00，普通用户、单实例 | 保留触发规则及任务归属，直接执行 Rust EXE |
| 程序 | `D:/python_program/ai_daily_report/.venv/Scripts/pythonw.exe` 加仓库 `main.py` | `D:/python_program/ai_daily_report/.releases/rust-20261015-01/ai-daily-report.exe` |
| 配置 | 仓库 `config` | `D:/python_program/ai_daily_report/config-rust/settings.yaml` |
| 报告库 | `data/db/reports.sqlite3` | 保持原路径和结构 |
| 扫描库 | `data/db/scan_index_v3.sqlite3` | 新建独立 `data/db/scan_index_rust_v3.sqlite3` |
| 模型 | `deepseek-flash`，每次请求最多 3 次尝试 | 保留现有模型、请求参数和累计预算关闭状态 |

切换前只读核对：任务 2026-10-15 09:00 的退出码为 0，报告库没有运行中或待恢复发布的运行。该次成功记录来自旧版；Rust 的真实窗口从 10 月 16 日起单独记录。

准备材料位于仓库 `.artifacts/rust-migration/cutover-preparation-02/`：

- `settings.yaml`：从有效业务配置逐项提取的新配置，不含凭据；Rust 配置加载已验证。
- `inventory.json`：77 个旧源码、依赖清单及解析程序文件的大小和 SHA-256，业务路径、状态统计及新旧扫描库路径。
- `current-task.xml`、`current-task.json`：当前任务定义及读取时间点的状态。
- 准备材料和归档配置均不含凭据值；Rust 继续读取现有用户环境凭据。

## 执行顺序

1. 核对候选包清单、旧文件哈希、有效配置、任务状态和未完成运行。任务正在执行或文件在核对后变化时，停止切换并重新核对。
2. 在批准的切换窗口临时禁用该任务的新触发，保存新的任务 XML。对报告库及旧扫描库创建 SQLite 一致备份，检查完整性并保存 SHA-256；保存配置、快照与旧运行文件。凭据保留在原有认证位置，普通归档不包含凭据值。
3. 将已验收的整个程序包复制到目标版本目录，复制已审阅的新配置；用包校验脚本核对全部文件。保留旧程序目录与虚拟环境。
4. 使用新配置执行严格自检、无模型的扫描预检，以及一次合成模型检查。模型检查正常 1 次调用，单次最多 3 次尝试，写入正常审计。正式业务材料不在这次模型检查中发送。
5. 调用已验证的任务安装脚本，将同名且归属一致的任务更新到 Rust EXE。注册后重新读取 XML，核对程序、配置目录、每日与登录触发、普通权限和 `IgnoreNew` 单实例设置。
6. 连续三个真实每日窗口验收。每个窗口核对目标日期、退出状态、报告文件与数据库哈希、来源范围、质量和模型用量；手动运行独立记录。窗口按实际切换时间确定，不能预填通过。
7. 在旧版归档可恢复、Rust 已成为正式入口且验收通过后，移除已替代的旧 Python 生产源码、`scanner_native` 与专属依赖及旧测试。DOC/PPT 能力保留在 Rust 解析器中。`.venv`、用户数据、数据库和缓存不属于源码清理对象。

本次已按以下路径完成切换。这些命令保留为核对及后续运维参考：

```powershell
$release = 'D:/python_program/ai_daily_report/.releases/rust-20261015-01'
$config = 'D:/python_program/ai_daily_report/config-rust'
& "$release/scripts/verify_windows_release.ps1" -BundleDirectory $release
& "$release/ai-daily-report.exe" --config-dir $config doctor --strict | Out-Host
& "$release/ai-daily-report.exe" --config-dir $config daily --dry-run | Out-Host
& "$release/ai-daily-report.exe" --config-dir $config doctor --check-model | Out-Host
& "$release/scripts/install_daily_task.ps1" -ProjectRoot $release -ConfigDirectory $config -Apply
& "$release/scripts/check_daily_task.ps1" -ProjectRoot $release -ConfigDirectory $config
```

发布 EXE 使用 Windows 无控制台子系统。PowerShell 命令接 `| Out-Host`，保证等待进程结束并取得正确退出码；读取 JSON 时使用 `| ConvertFrom-Json`。调度器直接等待 EXE，无需 PowerShell 中转。

## 回退证据和已确认范围

隔离测试已验证版本指针的切换、回退与业务文件原字节保留。合成运行中新增的 10 条调用记录及已发布正文哈希均保留，报告库未被回写。本次另通过真实 Windows 任务的禁用状态回退演练及正式配置回读；生产数据库、历史账本和文件保留验证通过。旧 Python 能读取新增周月报，但会按原有规则拒绝 `schema_version=3` 的日报结构。

2026-10-15 用户已明确接受以下范围：任务可切回归档旧程序及其扫描库，继续后续日期的旧格式生成；v3 日报的查询、周月汇总与恢复由保留的 Rust 包处理。新快照也只由对应版本恢复。保持报告库、调用账本、正文和快照原样，不转换为旧格式，不用旧备份覆盖新业务记录。

正式回退时先停止新触发、核对运行状态，恢复已保存的旧程序和配置路径，按归属恢复任务 XML并读取核对。保留 Rust 版本目录处理其已有记录。切换前与切换后都不得让两个版本同时恢复同一个运行。

## 验收边界

- 已完成本机独立包验收：清空继承环境、PATH 仅包含 Windows System32，未加载 Python、`.pyd` 或外置 VCRUNTIME140；使用本机模型服务，零远程调用。
- A01 已按 2026-10-15 用户决定验收通过：豁免全新 Windows 实测，接受现有本机隔离与依赖检查证据。全新系统实测未执行，本项不再阻塞切换。
- 回退范围已确认，正式任务已切到 Rust；真实调度窗口仍为 0/3。
- 本次生产模型自检和后续正常日报已获得独立授权；切换验收与六组虚构材料验收分别保留调用记录。

## 2026-10-16 请求容量调整

09:00 的真实任务已启动 Rust，但在模型调用前报 `MODEL_COMPOSE_CAPACITY_INSUFFICIENT`，该窗口失败。用户随后要求将单次请求上限扩大十倍。09:19 已将正式配置 `config-rust/settings.yaml` 的 `llm.max_request_utf8_bytes` 从 `16000` 改为 `160000`，同步源码默认值和示例；原配置与前后 SHA-256 保存在 `data/backups/rust-request-capacity-20261016-01/`。

使用已发布 Rust EXE 和隔离配置对同一日期做完整补跑预检：旧限制复现失败，新限制通过，23 个可用来源排入 3 个批次，请求大小为 53,546、62,671、42,776 字节，规划遗漏为 0，模型调用为 0。正式配置严格自检及 7 项 CLI 配置测试通过。

按既有正常日报授权手动补跑，于 09:22 发布 `data/reports/2026-10/2026-10-15.md`，运行编号 `13c644b05f8e8cf6a27b5eb5af7f26cd`。共 7 次模型调用，其中 3 次事实提取因输出截断失败，缩小输入后重试成功，最后成稿成功。输入扩容已解除原容量错误；解析、摘录和模型输入仍存在覆盖缺口，日报保留 `partial` 标记。文件与数据库正文哈希一致，输入快照及 7 次调用的请求、响应哈希全部通过核对。

本次手动补跑不计入真实调度验收。元数据证据保存在 `data/diagnostics/rust-request-capacity-20261016-01/verification.json`、`manual-run.json` 和 `publish-verification.json`，未将业务正文写入诊断日志。

## 2026-10-16 事实输出与完整分批升级

正式任务已升级到 `.releases/rust-20261016-02/ai-daily-report.exe`，继续使用 `config-rust` 和原报告库、Rust 扫描库。事实提取独立设置 `facts_max_tokens=16384`，初始 `max_batches=32`；请求上限保持 `160000` 字节，成稿输出保持 `8192` token。输出截断后完整处理两个子批次；大批量成稿按主题及确定／待确认分组，再由 Rust 合并并校验全部事实引用。模型计划版本为 v4，报告格式仍为 v3。

用 09:22 日报对应的同一份冻结摘录作隔离验收，逐个来源核对摘录哈希一致：23 个可用来源、49,958 个字符全部进入成功请求，原日报为 9,914 字（约 19.8%）。最终保留 117 条事实，其中确定事实 96 条、待确认 21 条；全部在正文或待确认中出现。最终成稿复用四个成功事实批次，新执行六个成稿请求。解析限制 19 项、摘录限制 22 项和一个不可用来源继续保留，质量仍为 `partial`。本项证明模型阶段完整处理选定摘录，不代表原文件全文覆盖或每个业务判断均经人工确认。

验证通过：425 项 Rust 工作区测试、29 项模型专项（包含在工作区总数中）、fmt、严格 clippy、独立发布包验收和 10 项 Windows 运维检查。独立包验收使用本机 HTTP 替身，未继承 Python 环境，未改生产任务；已成功的真实候选再次执行时没有新增模型调用。

配置、任务 XML、升级前日报与报告库一致备份保存于 `data/backups/rust-fact-coverage-20261016-01/`，配置和正文备份哈希与原文件一致。正式配置严格自检与一次合成模型检查通过，检查运行编号 `b59ba986971b4f2f1501d9fd2e554e27`。计划任务回读为 `Ready`、`MatchesPlan=true`，下次每日触发为 2026-10-17 09:00。10:45 自动验收已更新为核对新包；真实窗口仍为 0/3。

证据：`data/diagnostics/rust-fact-coverage-20261016-02/verification.json` 保存同材料对照、快照哈希和引用核验；`.artifacts/rust-migration/fact-coverage-20261016-01/` 保存最终构建、测试、任务、自检和生产运行记录。原 Rust 包与本次备份配置仍可用于对应旧模型计划；回退程序时须同时恢复对应配置，不回写业务数据库或调用账本。

正式重跑已发布 2026-10-15 日报，运行编号 `7ad42e453bff295e314e2f0d0b4bd5b0`。与升级前来源摘录哈希完全相同，49,958 字全部成功处理；本次生成 111 条事实条目（88 条确定、23 条待确认），组织为 30 个事项，10 次调用全部成功。所有事实编号均在正文或待确认中出现，文件、数据库和请求响应快照哈希一致；升级前 709 条调用记录与原日报正文、结构化内容均完整保留。该手动运行不计入实际调度窗口。核验记录为 `.artifacts/rust-migration/fact-coverage-20261016-01/production-verification.json`。
