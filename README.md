# AI Daily Report

根据本地文件生成可核对的中文日报、周报和月报。Rust 程序负责扫描、提取事实、成稿、存储及恢复，Windows 程序包无需 Python、uv 或虚拟环境。

新安装示例启用 `report.profile: personal`：日报按进展、留意、下一步整理，通常 3～7 项、最多 10 项，正文最多 1,500 字。关键数字、条件、待审批状态和来源摘录随日报保存。旧配置缺省为 `evidence`，历史日报保留原正文与完整证据附录。

个人日报关注指定工作目录的变化，尽可能比较前后内容，材料不足时明确说明。输入最多 20,000 字，人工补充最多 2,000 字。通常一次模型请求，同一工作范围和目标日期合计最多 3 次，预览、失败、未知结果、重启均计入。`data_dir/daily_cache` 中的处理文件与进行中的空间预留合计最多 1 GiB，正文最长保留 7 天；容量不足时可提前回收，不保证七天的完整历史。历史报告、旧归档、程序包和测试产物另行保留，整个项目占用不受此缓存限额约束。最终日报及依据文件最多 100,000 字节。程序总预算 300 秒，目录遍历、解析和模型调用均检查剩余时间。

个人模式优先保存已发现的新增和修改，初始基线只利用剩余空间补采，不要求全部补齐，也不为补采基线淘汰同级材料。后台默认准备今天与昨天的比较；更早的补报需指定 `--target-date`，按现存材料处理。比较使用目标日开始前和目标日内各自最后一次捕获的版本，未保存过的修改前正文无法事后还原。

每次先恢复未完成的复制，再清理超过 7 天的处理材料。容量不足时，保护今天、昨天及显式目标日所需的比较版本和共享内容，依次回收无必要引用的中间材料、待用材料；仍放不下就保留缺口。发现变化但未取得正文、缺少修改前版本、初始基线待补采分别记录，解析队列为零不代表材料完整。复制按顺序进行，正文只在需要比较时解析。后台每批取 128 项，在 120 秒预算内继续处理并预留收尾时间；每项任务同一轮最多尝试一次。个人模式同时运行一个解析进程，保留系统预留和 512 MiB 子进程上限，并为主进程后续增长预留 128 MiB。

缓存的 72 MiB 控制空间覆盖 SQLite 主库（上限 32 MiB）、回滚日志、额度账本及原子更新临时文件。复制同时考虑原件、两份回执的临时占用和 16 MiB 解析空间；解析按输出上限预留，日报快照按实际字节数预留，模型请求前保留响应写入空间。元信息未变化且本日已核验的文件走零正文读取快路径；每日内容核验会读取正文。性能指标以本机固定样本的测量为准。

## 当前状态

正式任务、配置、已完成事项及待验收结果见[当前状态](docs/current-state.md)。

## 使用

解压完整程序包，在程序目录外建立配置与数据目录。将 `config/settings.example.yaml` 复制成配置目录里的 `settings.yaml`，填写绝对工作路径、输出目录和已确认的模型参数。凭据通过 `DEEPSEEK_API_KEY`、`OPENAI_API_KEY` 或本地 `.secrets.yaml` 提供。

```powershell
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config doctor --strict | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config collect --baseline | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config collect | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config daily --dry-run | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config daily --target-date 2026-10-11 | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config weekly 2026-W41 --source db | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config monthly 2026-10 --source db | Out-Host
```

`daily` 默认报告上一个已结束的自然日，时区固定为 `Asia/Shanghai`。personal 要求 `archive.daily_source: archive`，拒绝 `daily --source scan`。首次基线用于建立文件状态；后续采集才提供可比较变化，基线不计为新进展。`--dry-run` 准备有限输入、零模型请求；`--no-save` 生成有调用记录的预览。补充可通过 `daily -i "..."` 或 `report.notes_dir/YYYY-MM-DD.md` 提供。`doctor --strict` 不联网；`doctor --check-model` 明确发送一批合成材料并记录用量。

```powershell
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config runs --target-date 2026-10-11 | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config repair-output --target-date 2026-10-11 | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config health --format text | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config search --query 审批 --kind daily --limit 20 | Out-Host
```

修复输出直接读取数据库中的正文，零扫描、零模型调用。检测到人工编辑时停止覆盖。`health` 只读检查采集连续性、日报发布与哈希、运行状态、磁盘与数据库完整性，不调用模型；退出码 0/1/2 分别表示健康/警告/不健康。`search` 只读检索历史日报、周报和月报正文，命中到具体事项，支持日期、类型和来源过滤；索引保存在独立的 `report_search.sqlite3`，可删除重建，报告库不被修改。

## 文件与实现

支持文本、Markdown、CSV、JSON、日志、XLSX、真实 XLS、DOCX、PPTX 和 PDF。旧版 DOC/PPT 使用 Rust 解析，默认关闭，需显式设置 `scanner.legacy_office_enabled: true` 并加入允许的扩展名。根据真实格式选择一个解析器，错误后记录覆盖缺口。

```text
ai-daily-report.exe
  ├─ report_app：配置、命令、模型调用、报告、恢复及维护
  ├─ archive：持续采集、全文、差异和报告依据
  └─ scanner_core：发现、选择、解析调度、缓存及覆盖审计
       ├─ ai-daily-office-parser.exe：Office 文件
       └─ ai-daily-pdf-parser.exe + pdfium.dll：PDF 文件
```

新扫描库固定为 `scan_index_rust_v3.sqlite3`，与旧库分开。报告库保留既有结构。请求与响应保存原始字节、哈希和用量；事实提取成功而成稿失败时，恢复只补齐成稿。每次请求默认最多尝试三次，程序不会自动更换模型或解析器。

## 开发与构建

需要 Windows x64、Rust MSVC 工具链及 C/C++ 构建工具。运行环境仅需完整程序包与 Windows 系统库；构建脚本静态链接 Rust 程序的 C 运行库。

```powershell
./scripts/prepare_rust_dependencies.ps1
cargo build --manifest-path rust/Cargo.toml --workspace --all-targets --locked
# 将固定 pdfium.dll 放在 debug 解析程序旁，再运行依赖 PDF 的测试。
Copy-Item rust/target/pdfium/bin/pdfium.dll rust/target/debug/pdfium.dll
cargo fmt --manifest-path rust/Cargo.toml --all -- --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path rust/Cargo.toml --workspace --locked
./scripts/build_windows_release.ps1 -OutputDirectory ./dist/rust-report -ReleaseVersion rust-report
./scripts/verify_windows_release.ps1 -BundleDirectory ./dist/rust-report
```

构建输出使用独立目录，保留现有生产解析程序。发布包包含三个 EXE、固定 PDFium DLL、模板、配置示例、运维脚本、依赖许可证与 SHA-256 清单。

- [使用与恢复](docs/daily-report-operations.md)：补跑、覆盖修复、OCR、预算和保留策略。
- [Windows 安装与回退](docs/windows-deployment.md)：独立目录、计划任务、自检和回退。
- [扫描与解析后端](docs/scanner-backends.md)：当前调用关系、格式路由和工作进程。
- [历史归档索引](docs/archive/README.md)：历次设计、迁移与验收材料。
- [旧 Python 操作说明归档](docs/archive/python-runtime-readme-2026-10-15.md)：切换前的运行参考。

变化归档、版本差异、引用证据、采集任务与统计命令见 [变化归档操作说明](docs/change-archive.md)。2,648 文件测量、并发配置和覆盖限制见 [归档性能验收](docs/archive/history/archive-performance-20261017.md)。
