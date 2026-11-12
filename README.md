# AI Daily Report

根据本地文件生成可核对的中文个人日报。每天上海时间 09:00 扫描一次工作目录，按昨天的文件修改时间选材，生成摘要与独立依据附件。完整 Windows 程序包无需 Python、uv 或虚拟环境。

日报按进展、留意、下一步整理，通常 3～5 项、正文目标 420～560 字，最多 5 项、1,500 字。初稿在上限内时完整发布；超过上限才单独精简。精简失败时按完整事项减量，并保留下一步计划。原文、来源位置和内容指纹保存在依据附件中。

当前源码只生成个人日报。周月报、旧通用日报、批量补跑、覆盖修复作业和 OCR 命令已退役。旧报告、调用记录和归档证据继续可查；已保存正文及附件可原样恢复。正式部署状态见[当前状态](docs/current-state.md)，本地候选包验证见[消融实验结果](docs/plans/2026-10-31-personal-ablation-results.md)。

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
./ai-daily-report.exe --config-dir D:/DailyReport/config search --query 审批 --kind all | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config runs --target-date 2026-10-30 | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config repair-output --target-date 2026-10-30 | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config repair-output --run-id <运行编号> | Out-Host
./ai-daily-report.exe --config-dir D:/DailyReport/config health --format text | Out-Host
```

历史检索覆盖日报、周报和月报。`repair-output` 从数据库恢复原正文及附件，检测到人工编辑时停止覆盖。归档可查询版本、原件位置、提取原文及差异，并执行核验与一致备份。旧生成任务的续跑使用保留的旧程序与配套配置。

## 运行边界

- 支持文本、Markdown、CSV、JSON、日志、XLSX、真实 XLS、DOCX、PPTX 和 PDF。旧 DOC/PPT 继续由显式启用的 Rust 解析器处理。
- 文件材料最多 180,000 字，人工补充最多 2,000 字；正文与依据附件合计最多 1,000,000 字节。正式运行采用的限额以活动配置和[当前状态](docs/current-state.md)为准。
- 处理缓存位于 `data_dir/daily_cache`，材料保留 7 天，文件与空间预留合计最多 1 GiB。历史报告、归档和恢复资料分别保留。
- 程序预算 300 秒，270 秒后停止启动新工作；扫描最多 120 秒、一个解析进程。每个模型步骤保留有限尝试，通信失败后结束本轮请求。
- 预览、失败和未知调用结果均保留审计与用量。不设置累计调用次数、token 用量或金额额度。
- 扫描保存读取时可见的内容。中间版本、已经删除或覆盖的文件可能无法还原，覆盖缺口会进入报告。

## 实现与开发

`report_app` 负责个人日报、历史查询、模型调用、存储和恢复；`scanner_core` 负责发现文件、选择、调度、缓存和覆盖审计。Office 与 PDF 在独立解析程序中运行，PDF 使用固定的 `pdfium.dll`。解析格式、协议、构建身份和缓存校验保持各自职责。

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

发布使用共享构建缓存，三个 EXE 同时构建，包含固定 PDFium、个人日报模板、配置示例、运维脚本、许可证和 SHA-256 文件清单。

- [日报使用与恢复](docs/daily-report-operations.md)
- [Windows 安装与回退](docs/windows-deployment.md)
- [扫描与解析后端](docs/scanner-backends.md)
- [历史归档查询](docs/change-archive.md)
- [历史文档与证据](docs/archive/README.md)
