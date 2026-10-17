# AI Daily Report

根据本地文件生成可核对的中文日报、周报和月报。Rust 程序负责扫描、提取事实、成稿、存储及恢复，Windows 程序包无需 Python、uv 或虚拟环境。

日报按“要素速览 → 内容概览 →（事项目录）→ 事项详情 → 待确认 → 来源与覆盖附录”呈现，归档来源另有“变化依据”审计附录。速览行给出材料、事实与缺口数量；事项详情按重要度排序，长段按句切分，来源引用紧邻事实；覆盖缺口按错误码聚合；事项达到 10 个时生成可跳转目录。同一事项集中说明，保留关键数字、条件、例外及方案状态。技术统计集中放在附录，引用、哈希与版本信息不因排版调整而删减。历史日报继续保留原正文。

## 当前状态

正式任务、配置、已完成事项及待验收结果见[当前状态](docs/current-state.md)。

## 使用

解压完整程序包，在程序目录外建立配置与数据目录。将 `config/settings.example.yaml` 复制成配置目录里的 `settings.yaml`，填写绝对工作路径、输出目录和已确认的模型参数。凭据通过 `DEEPSEEK_API_KEY`、`OPENAI_API_KEY` 或本地 `.secrets.yaml` 提供。

```powershell
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config doctor --strict | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config daily --dry-run | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config daily --target-date 2026-10-11 | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config weekly 2026-W41 --source db | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config monthly 2026-10 --source db | Out-Host
```

`daily` 默认报告上一个已结束的自然日，时区固定为 `Asia/Shanghai`。日报来源由 `archive.daily_source` 决定，也可用 `--source archive` 或 `--source scan` 显式指定。`--dry-run` 仅准备输入，不调用模型：归档来源读取已有归档，扫描来源扫描原目录。`--no-save` 生成有审计记录的预览。`doctor --strict` 不联网；`doctor --check-model` 明确发送一批合成材料并记录用量。

```powershell
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config runs --target-date 2026-10-11 | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config repair-output --target-date 2026-10-11 | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/shared/config health --format text | Out-Host
```

修复输出直接读取数据库中的正文，零扫描、零模型调用。检测到人工编辑时停止覆盖。`health` 只读检查采集连续性、日报发布与哈希、运行状态、磁盘与数据库完整性，不调用模型；退出码 0/1/2 分别表示健康/警告/不健康。

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
