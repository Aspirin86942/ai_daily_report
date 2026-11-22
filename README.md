# AI Daily Report

个人日报内核 v2 根据本地文件生成可核对的中文日报。每天上海时间 09:00 按前一天的文件修改时间选材。Windows x64 默认包包含主程序、PDF 子程序和固定版本的 PDFium。

新生成的正文最多 1,500 个 Unicode 字符，每条事项链接到依据。独立附件保存本轮采纳的原文和位置，包括正文未展开的材料。旧报告沿用各自保存的格式与字节。

当前源码只生成个人日报。周月报、旧通用日报、批量补跑、覆盖修复作业和 OCR 命令已退役。旧报告、调用记录和归档证据继续可查；已保存正文及附件可原样恢复。正式部署状态见[当前状态](docs/current-state.md)，本地候选包的检索、语义、性能与恢复验收见[内核优化记录](docs/plans/2026-11-21-kernel-optimization.md)。

## 使用

解压完整程序包，在程序目录外建立配置和数据目录。复制 `config/settings.example.yaml` 为配置目录中的 `settings.yaml`，填写工作路径及经确认的模型容量。凭据通过 `DEEPSEEK_API_KEY`、`OPENAI_API_KEY` 或本地 `.secrets.yaml` 提供。

```powershell
./ai-daily-report.exe --config-dir D:/DailyReport/config doctor --strict | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config daily --dry-run | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config daily | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config daily --target-date 2026-10-30 -i "补充工作进展" | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config daily --target-date 2026-10-30 --regenerate --reason "补充材料后重新生成" | Out-Host
```

`daily` 默认生成上一个已结束自然日的日报。`--dry-run` 仅准备输入，零模型请求；`--no-save` 生成留有调用记录的预览。补充也可写入 `report.notes_dir/YYYY-MM-DD.md`。重复触发已发布日期时复用原报告。新配置默认使用 `personal`；显式 `evidence` 配置用于历史查询与恢复。

```powershell
./ai-daily-report.exe --config-dir D:/DailyReport/config search --query "预算 审批" --kind all | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config runs --target-date 2026-10-30 | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config repair-output --target-date 2026-10-30 | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config repair-output --run-id <运行编号> | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config health --format text | Out-Host
```

历史检索可按多个关键词查找日报事项和独立依据附件，返回来源位置。历史周报和月报也可按正文检索。`repair-output` 从数据库恢复原正文及附件，检测到人工编辑时停止覆盖。归档可查询版本、原件位置、提取原文及差异，并执行核验与一致备份。旧生成任务的续跑使用保留的旧程序与配套配置。

## 运行边界

- 支持文本、Markdown、CSV、JSON、日志、XLSX、真实 XLS、DOCX、PPTX 和 PDF。旧 DOC/PPT 继续由显式启用的 Rust 解析器处理。
- 文件材料最多 180,000 字，人工补充最多 2,000 字；正文与依据附件合计最多 1,000,000 字节。正式运行采用的限额以活动配置和[当前状态](docs/current-state.md)为准。
- 处理缓存位于 `data_dir/daily_cache`，材料保留 7 天，文件与空间预留合计最多 1 GiB。历史报告、归档和恢复资料分别保留。
- 运行总预算 300 秒，270 秒后停止启动新工作，扫描最多 120 秒。文本和 Office 在主程序中解析，默认并发最多 4 路。显式单路配置涵盖 PDF。每个模型步骤有独立的有限尝试和截止时间。
- 预览、失败和未知调用结果均保留审计与用量。不设置累计调用次数、token 用量或金额额度。
- 扫描保存读取时可见的内容。中间版本、已经删除或覆盖的文件可能无法还原，覆盖缺口会进入报告。

## 实现与开发

本工作区有三个 Rust 包。`daily_report` 负责扫描调度、缓存、日报、历史检索和发布，`daily_parsers` 在主程序内发现文件并解析文本和 Office，`pdf_worker` 使用固定的 `pdfium.dll` 处理 PDF。需要识别纯图 PDF 时使用带 OCR 功能的独立构建，并显式开启 `scanner.pdf_image_ocr`。识别结果带有页码和待核实标记。

开发需要 Windows x64、Rust MSVC 工具链及 C/C++ 构建工具。

```powershell
cargo build --manifest-path rust/Cargo.toml --workspace --all-targets --locked
# 在 debug PDF 解析程序旁放置 rust/target/pdfium/bin/pdfium.dll。
cargo fmt --manifest-path rust/Cargo.toml --all -- --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path rust/Cargo.toml --workspace --locked
./scripts/build_windows_release.ps1 -OutputDirectory ./dist/personal-report -ReleaseVersion personal-report
./scripts/verify_windows_release.ps1 -BundleDirectory ./dist/personal-report
```

两项 Windows 程序同批构建并核对解析身份。默认发布包含 PDFium、三份活动模板、运维脚本、配置示例、许可证及 SHA-256 文件清单。可选 OCR 包另行构建。

- [日报使用与恢复](docs/daily-report-operations.md)
- [Windows 安装与回退](docs/windows-deployment.md)
- [扫描与解析后端](docs/scanner-backends.md)
- [历史归档查询](docs/change-archive.md)
- [历史文档与证据](docs/archive/README.md)
