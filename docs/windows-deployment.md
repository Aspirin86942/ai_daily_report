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

新安装示例采用 `personal + archive`，日报应用预算 300 秒，最后 30 秒用于收尾，任务时限至少 6 分钟；采集和补解析应用预算至多 120 秒，任务时限 3 分钟。时间检查约束程序主动启动的工作，Windows 文件系统调用卡死时由任务计划终止。日志记录正常应用退出；回读中 `267014` 表示调度终止，其他非零结果保留未知原因。外部强制终止前可能尚未完成保存。

日报、采集、补解析和健康任务必须使用同一 profile、工作目录、处理目录与模型发送权限。安装器和回读脚本检查已有兄弟任务，拒绝 personal/evidence 或两种处理目录混用。两份配置可保留独立文件，但以上字段必须一致，Worker 均指向同一候选包。

从旧任务组切换时，应在授权维护窗口保存四份任务 XML、两份配置、报告库一致备份及哈希，确认没有运行中的任务；按原归属移除已停用的旧任务定义，再用现有安装脚本注册四个一致的新任务并逐个回读。首次在新处理区执行 `collect --baseline`，随后由普通采集继续建立基线；在观察充分前，日报保留首日缺口。正式切换和真实模型自检须在对应授权内执行。

```powershell
./scripts/install_daily_task.ps1 -ProjectRoot D:/DailyReport/releases/<version> -ConfigDirectory D:/DailyReport/shared/config
# 核对计划后，已获准的注册操作使用 -Apply。
./scripts/install_daily_task.ps1 -ProjectRoot D:/DailyReport/releases/<version> -ConfigDirectory D:/DailyReport/shared/config -Apply
./scripts/install_daily_task.ps1 -ProjectRoot D:/DailyReport/releases/<version> -ConfigDirectory D:/DailyReport/shared/config -Kind health -Apply
./scripts/check_daily_task.ps1 -ProjectRoot D:/DailyReport/releases/<version> -ConfigDirectory D:/DailyReport/shared/config
./scripts/check_daily_task.ps1 -ProjectRoot D:/DailyReport/releases/<version> -ConfigDirectory D:/DailyReport/shared/config -Kind health
```

任务直接运行 `ai-daily-report.exe --config-dir "..." daily --scheduled`。日报任务带 09:00、09:30、10:30 三次触发：09:00 正常出报，09:30/10:30 为兜底续跑；目标日已发布时快速返回且不调用模型。发布 EXE 使用 Windows 图形子系统，调度时不打开终端窗口；命令行调用保留标准输出与退出码。personal 调度日志位于 `log_dir/personal/scheduled-YYYY-MM-DD.jsonl`，evidence 保持 `log_dir/scheduled-YYYY-MM-DD.jsonl`。每条记录 kind、运行编号、目标日期、阶段、结果、错误码与耗时，错误包含 `message/retryable`。

补解析任务 `AI Daily Report Parse` 使用采集配置，以 `-Kind parse` 安装，运行 `archive drain --scheduled --max-seconds 120`。工作日窗口内每半小时检查队列，常规时间比窗口网格晚 5 分钟，并补窗口末次；正式窗口为 09:05、09:35…17:35、18:00。它与采集共用处理区锁，不访问源目录、不调用模型，personal 的调度上限 3 分钟，evidence 保持 5 分钟；窗口外登录或延后启动跳过。

健康任务 `AI Daily Report Health` 由 `scripts/install_daily_task.ps1 -Kind health -Apply` 注册，每日 11:00 与 23:00 运行 `scripts/health_notify.ps1`。Rust 先原子写入 `log_dir/health-YYYY-MM-DD.json`；脚本通过 `health --scheduled --output <临时文件>` 读取 UTF-8 JSON、回读四个任务后通知。通知组件异常另存错误，健康文件保持。health 和 parse 任务不要求模型检查记录。

PowerShell 调用发布 EXE 时接 `| Out-Host`，让 PowerShell 等待退出；机器 JSON 应显式按 UTF-8 解码。健康通知优先读取程序 `--output` 文件，避免 Windows PowerShell 5.1 按本地代码页转换 stdout。调度日志新增 execution_status、material_status、publication_status、queued、errors、reason_codes；材料未齐备为 partial，晨间等待材料为 deferred，不能将命令结束等同于完整处理。

沿用当前用户、普通权限、每日上海时间、登录补触发和 `IgnoreNew` 单实例策略。安装先核对归属，保存原 XML，注册后重新读取比较。读取结果不符合计划时禁用该任务并返回错误。仅改写版本指针不会修改计划任务；任务升级必须显式更新并核对直接 EXE 路径。

任务归属按持久配置的父目录生成。将配置从旧仓库移到新的安装根目录时，先保存并按原归属移除旧任务，再注册新任务，禁止绕过归属检查覆盖同名任务。

## 备份与回退

旧 Rust 二进制可能无法读取新增 v5 报告。回退时应为旧程序使用隔离的数据目录，保留新程序对新增报告的只读访问和修复能力。新处理缓存可以到期，但报告库及其新记录必须保留；禁止用切换前的数据库备份覆盖新增记录。

```powershell
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config backup --output D:/DailyReport/shared/backups/reports-before-switch.sqlite3 | Out-Host
./scripts/archive_scanner_database.ps1 -SourceDatabase <旧扫描库> -ArchiveDirectory <归档目录>
./scripts/update_release_pointer.ps1 -Mode Switch -PointerPath D:/DailyReport/current.json -ReleaseVersion <version> -ScannerDatabasePath shared/data/db/scan_index_rust_v3.sqlite3 -Apply
./scripts/update_release_pointer.ps1 -Mode Rollback -PointerPath D:/DailyReport/current.json -Apply
```

SQLite 备份使用一致快照，包含已经提交到 WAL 的数据，并验证完整性。目标必须不存在。版本指针原子替换，保留原指针备份及上一版本引用。回退切换程序及其扫描库，报告库、调用账本和新增快照继续保留，不用旧备份覆盖新业务数据。

旧 Python 拒绝新的 v3 日报结构。按 2026-10-15 已确认的回退范围，旧任务可继续后续日期的旧格式生成；新格式日报的查询、周月汇总和未完成运行恢复继续使用保留的 Rust 包，不向旧格式转换。

正式切换前保存旧源码、可运行环境、配置、快照、任务 XML 和文件哈希，核对未完成作业与正在执行的进程。真实每日窗口核对目标日、退出码、数据库与正文哈希、来源和模型用量。旧扫描三个连续成功窗口的清理条件继续按当前状态保留。模拟任务及手动运行分别记录，不能代替真实窗口。
