# Windows 安装、任务与回退

Rust 程序包面向 Windows x64。三个 EXE 使用静态 C 运行库，PDFium 固定版本和 SHA-256；运行不需要 Python、uv、虚拟环境或 Rust 工具链。

正式任务路径、配置、已完成验收及证据统一见[当前状态](current-state.md)。以下为安装、切换和回退说明。

## 目录与配置

```text
D:/DailyReport/
  current.json
  releases/<version>/
    ai-daily-report.exe
    ai-daily-office-parser.exe
    ai-daily-pdf-parser.exe
    pdfium.dll
    templates/  config/settings.example.yaml  scripts/  licenses/
    manifest.json
  shared/
    config/settings.yaml
    data/db/reports.sqlite3
    data/db/scan_index_rust_v3.sqlite3
    reports/  logs/  backups/
```

每个版本保存在独立目录。配置显式通过 `--config-dir` 传入；省略时只读取 EXE 旁的 `config`。配置中的相对路径始终以 EXE 目录为基准，正式安装建议全部填写绝对路径，避免升级后数据目录改变。

从包内示例生成新的 Rust 配置。旧配置中的 Python 路径、`fallback_after_timeout` 等废弃项会被拒绝，需要在准备阶段移除。保留业务模型、工作根目录、预算开关和扫描限制，扫描库使用独立的 `scan_index_rust_v3.sqlite3`。凭据继续使用当前用户环境变量，配置与凭据不进入发布包。

## 包校验与自检

```powershell
./scripts/verify_windows_release.ps1 -BundleDirectory D:/DailyReport/releases/<version>
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config doctor --strict | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config daily --dry-run | Out-Host
```

包校验检查完整文件清单、大小与哈希，拒绝缺失、额外或被修改的文件。严格自检检查工作目录、输出目录、模板版本、报告库、扫描库、Office/PDF 同版握手与 PDFium；不发送模型请求。

显式执行 `doctor --check-model` 会发送一批合成材料并把成功、失败及用量写入报告库。该动作产生模型费用。计划任务安装要求当前有效配置已有成功的模型检查记录；改变模型参数后需要重新检查。

## 计划任务

新安装示例采用 `personal` 每日扫描，日报应用预算 300 秒，最后 30 秒用于收尾，任务时限至少 6 分钟。扫描和解析在同一次日报运行内串行完成。时间检查约束程序主动启动的工作，Windows 文件系统调用卡死时由任务计划终止。日志记录正常应用退出；回读中 `267014` 表示调度终止，其他非零结果保留未知原因。外部强制终止前可能尚未完成保存。

日报和健康任务必须使用同一 profile、工作目录、处理目录与模型发送权限。安装器和回读脚本检查已有兄弟任务，拒绝 personal/evidence 或两种处理目录混用。两项任务使用当前生效配置，Worker 均指向同一候选包。

从旧任务组切换时，应在授权维护窗口保存现有任务 XML、配置、报告库一致备份及哈希，确认没有运行中的任务；按原归属移除旧 Collect、Parse 任务，再用现有安装脚本注册 Daily、Health 并逐个回读。当前程序已删除采集、补解析及归档出报入口。正式切换和真实模型自检须在对应授权内执行。

```powershell
./scripts/install_daily_task.ps1 -ProjectRoot D:/DailyReport/releases/<version> -ConfigDirectory D:/DailyReport/shared/config
# 核对计划后，已获准的注册操作使用 -Apply。
./scripts/install_daily_task.ps1 -ProjectRoot D:/DailyReport/releases/<version> -ConfigDirectory D:/DailyReport/shared/config -Apply
./scripts/install_daily_task.ps1 -ProjectRoot D:/DailyReport/releases/<version> -ConfigDirectory D:/DailyReport/shared/config -Kind health -Apply
./scripts/check_daily_task.ps1 -ProjectRoot D:/DailyReport/releases/<version> -ConfigDirectory D:/DailyReport/shared/config
./scripts/check_daily_task.ps1 -ProjectRoot D:/DailyReport/releases/<version> -ConfigDirectory D:/DailyReport/shared/config -Kind health
```

任务直接运行 `ai-daily-report.exe --config-dir "..." daily --scheduled`。个人日报每天上海时间 09:00 触发一次，汇总前一天；错过时间由 `StartWhenAvailable` 补执行，无登录触发、日内重复触发或失败自动重启。目标日已发布时快速返回且不调用模型。发布 EXE 使用 Windows 图形子系统，调度时不打开终端窗口；命令行调用保留标准输出与退出码。personal 调度日志位于 `log_dir/personal/scheduled-YYYY-MM-DD.jsonl`，evidence 保持 `log_dir/scheduled-YYYY-MM-DD.jsonl`。每条记录 kind、运行编号、目标日期、阶段、结果、错误码与耗时，错误包含 `message/retryable`。

健康任务 `AI Daily Report Health` 由 `scripts/install_daily_task.ps1 -Kind health -Apply` 注册，每日 11:00 与 23:00 运行 `scripts/health_notify.ps1`。Rust 先原子写入 `log_dir/health-YYYY-MM-DD.json`；脚本通过 `health --scheduled --output <临时文件>` 读取 UTF-8 JSON、回读 Daily 和 Health 两个任务后通知。通知组件异常另存错误，健康文件保持。health 任务不要求模型检查记录。

PowerShell 调用发布 EXE 时接 `| Out-Host`，让 PowerShell 等待退出；机器 JSON 应显式按 UTF-8 解码。健康通知优先读取程序 `--output` 文件，避免 Windows PowerShell 5.1 按本地代码页转换 stdout。调度日志包含 execution_status、material_status、publication_status、errors、reason_codes；读取、解析或输入预算产生缺口时保留 partial，命令成功退出不表示所有材料均已覆盖。

沿用当前用户、普通权限、每日上海时间和 `IgnoreNew` 单实例策略。安装先核对归属，保存原 XML，注册后重新读取比较。读取结果不符合计划时禁用该任务并返回错误。仅改写版本指针不会修改计划任务；任务升级必须显式更新并核对直接 EXE 路径。

任务归属按持久配置的父目录生成。将配置从旧仓库移到新的安装根目录时，先保存并按原归属移除旧任务，再注册新任务，禁止绕过归属检查覆盖同名任务。

## 备份与回退

2026-10-29 11:35:36 已切换至 `.releases/rust-20261029-summary-1500-01`（个人日报正文硬上限 1,500 字）。本次回退包为完整保留的 `.releases/rust-20261028-summary-compact-01`；配置、两项任务 XML、报告库及扫描库一致备份位于 `.artifacts/personal-summary-extension-20261029/deployment-01/before`，逐文件哈希见同级 `backup-manifest.json`。本次仅更新既有任务的执行动作与工作目录，并改写配置中的两个解析程序路径，触发器、权限和启用状态保持。回退时按备份恢复对应程序路径、任务动作和配置，核对同包解析程序并重新执行 `doctor --strict` 与任务回读；当前业务数据库、报告和调用账本继续保留，禁止用旧数据库备份覆盖新增数据。部署、恢复 10 月 28 日日报及任务实跑结果见同级 [final-verification.json](../.artifacts/personal-summary-extension-20261029/deployment-01/final-verification.json)。

2026-10-28 14:31:33 已切换至 `.releases/rust-20261028-summary-compact-01`。本次回退包为完整保留的 `.releases/rust-20261027-collection-clean-01`；配置、两项任务 XML、报告库及扫描库一致备份位于 `.artifacts/personal-summary-compaction-20261028/deployment-01/before`，逐文件哈希见同级 `backup-manifest.json`。本次仅更新既有任务的执行动作和配置中的两个解析程序路径，触发器及权限保持。回退时按备份恢复对应程序路径、任务动作和配置，核对同包解析程序并重新执行 `doctor --strict` 与任务回读；当前业务数据库、报告和调用账本继续保留，禁止用旧数据库备份覆盖新增数据。部署及正式任务实跑结果见同级 [final-verification.json](../.artifacts/personal-summary-compaction-20261028/deployment-01/final-verification.json)。

2026-10-27 18:55 的残留清理版本为 `.releases/rust-20261027-collection-clean-01`。本轮回退包为保留的 `.releases/rust-20261027-collection-retired-02`；任务 XML、配置和快照位于 `.artifacts/personal-collection-residue-20261027/deployment-01/before`，该处数据库一致备份已无损归档，恢复方式见下文。源码备份及逐文件哈希见同级 `source-before.zip` 和 `source-before.json`。回退仅恢复程序路径、两项任务和配套配置，保留切换后新增的报告、调用账本和快照。最新核验见同级 `final-verification.json`。

2026-10-27 日内采集退役的恢复资料位于 `.artifacts/personal-collection-retirement-20261027`，各文件 SHA-256 见 `backup-manifest.json`。回退到上一每日扫描包时，使用保留的 `.releases/rust-20261027-daily-scan-01`，恢复 `deployment-01/before/daily-settings.yaml` 及 `daily.xml`、`health.xml`，重新核对 Worker 路径、任务触发和启用状态。回退不覆盖报告库与新增业务记录。

2026-10-27 的空间清理将 27 个静态数据库副本合并为 15 份不同内容，保存于 `.artifacts/project-space-cleanup-20261027/phase2/database-backups.zip`。原路径、大小、修改时间和 SHA-256 保存在同目录 `recovery-index.json.gz`，压缩包及索引的校验值见 `recovery-catalog.json`。恢复程序会核对内容，并拒绝覆盖已有的不同文件。运行下面的命令可把残留清理前的数据库恢复到原备份位置；需要隔离恢复时添加 `--destination <项目内目标路径>`。

```powershell
C:/ProgramData/miniforge3/python.exe .artifacts/project-space-cleanup-20261027/phase2/restore.py --path .artifacts/personal-collection-residue-20261027/deployment-01/before/reports.sqlite3
```

同一索引还保存已清理测试材料的恢复原件路径。压缩包、索引、恢复程序及这些原件需共同保留；完整说明见[恢复说明](../.artifacts/project-space-cleanup-20261027/phase2/RECOVERY.txt)。历史数据库仅用于核查与隔离恢复，正式报告库继续保留全部新增记录。

如需恢复旧日内链路，另行确认恢复范围后，先从 `cache.zip`、`collector-config.zip`、`retired-release-complete.zip` 恢复清单中的三个原路径并逐文件核验。旧任务定义位于同一 `before` 目录的 `collect.xml`、`parse.xml`；只有相应目录、匹配程序及配置均恢复完成后才能注册。`cache-ledger.json` 保存退役前登记表；如已有新扫描快照，按仍然存在的文件合并恢复，不能覆盖新增登记。旧数据库一致备份用于核查与隔离恢复。

旧 Rust 二进制可能无法读取新增 v5 报告。回退时应为旧程序使用隔离的数据目录，保留新程序对新增报告的只读访问和修复能力。新处理缓存可以到期，但报告库及其新记录必须保留；禁止用切换前的数据库备份覆盖新增记录。

```powershell
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config backup --output D:/DailyReport/shared/backups/reports-before-switch.sqlite3 | Out-Host
./scripts/archive_scanner_database.ps1 -SourceDatabase <旧扫描库> -ArchiveDirectory <归档目录>
./scripts/update_release_pointer.ps1 -Mode Switch -PointerPath D:/DailyReport/current.json -ReleaseVersion <version> -ScannerDatabasePath shared/data/db/scan_index_rust_v3.sqlite3 -Apply
./scripts/update_release_pointer.ps1 -Mode Rollback -PointerPath D:/DailyReport/current.json -Apply
```

SQLite 备份使用一致快照，包含已经提交到 WAL 的数据，并验证完整性。目标必须不存在。版本指针原子替换，保留原指针备份及上一版本引用。回退切换程序及其扫描库，报告库、调用账本和新增快照继续保留，不用旧备份覆盖新业务数据。

旧 Python 拒绝新的 v3 日报结构。按 2026-10-15 已确认的回退范围，旧任务可继续后续日期的旧格式生成；新格式日报的查询、周月汇总和未完成运行恢复继续使用保留的 Rust 包，不向旧格式转换。

正式切换前保存旧源码、可运行环境、配置、快照、任务 XML 和文件哈希，核对未完成作业与正在执行的进程。真实每日窗口核对目标日、退出码、数据库与正文哈希、来源和模型用量。本轮生效范围见[当前状态](current-state.md)，更早扫描版本的清理条件保留在[历史运行记录](archive/history/runtime-through-20261027.md)。模拟任务及手动运行分别记录，不能代替真实窗口。
