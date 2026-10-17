# 脚本归属与保留说明

核对日期：2026-10-18。本目录顶层实际有 **22 个脚本**，均已逐项阅读文件头和关键参数；其中生产/发布 6 个、计划任务 6 个、验收证据 9 个、旧 Python 保留 1 个。另列出 `pdf_benchmark/` 的 6 个脚本和 3 份结果，覆盖递归跟踪的全部 31 个文件（本 README 除外）。

正式运行状态见[当前状态](../docs/current-state.md)。脚本归类仅说明用途；生产切换、任务安装/移除、模型调用、原始数据或证据覆盖仍需相应授权。本轮只补说明，不运行这些脚本、不安装依赖、不改扫描覆盖或 OCR 开关。

## 生产/发布

| 文件 | 用途 | 入口或前置条件 | 分类 |
| --- | --- | --- | --- |
| [build_windows_release.ps1](build_windows_release.ps1) | 构建三个纯 Rust 程序并生成完整发布清单 | `-OutputDirectory`、`-ReleaseVersion`；Rust 工具链和固定 PDFium；使用独立构建目录，输出目录须不存在 | 生产/发布 |
| [verify_windows_release.ps1](verify_windows_release.ps1) | 校验 Rust 发布包、文件哈希与构建身份 | `-BundleDirectory`；调用 `release_common.ps1`，不切换正式包 | 生产/发布 |
| [windows_release.py](windows_release.py) | 保留旧 Python 原生包的打包、校验、数据库归档和指针工具 | Python `3.13.13`；子命令 `verify-runtime`、`package`、`verify-bundle`、`archive-db`、`pointer-switch`、`pointer-rollback`；打包需匹配 wheel/worker，指针写入需 `--apply` | 生产/发布 |
| [release_common.ps1](release_common.ps1) | 复用本地路径保护、原子 JSON 写入和 Rust 包校验 | 由发布脚本通过点加载调用；无独立命令行入口 | 生产/发布 |
| [prepare_rust_dependencies.ps1](prepare_rust_dependencies.ps1) | 按固定清单下载并校验 PDFium | 可传 `-Destination`；读取 `rust/build-inputs.json`，下载需要网络，校验 SHA-256 | 生产/发布 |
| [update_release_pointer.ps1](update_release_pointer.ps1) | 切换或回退发布指针并保留前一引用 | `-Mode Switch\|Rollback`、`-PointerPath`、`-Apply`；切换还需 `-ReleaseVersion`、`-ScannerDatabasePath` | 生产/发布 |

## 计划任务

| 文件 | 用途 | 入口或前置条件 | 分类 |
| --- | --- | --- | --- |
| [install_daily_task.ps1](install_daily_task.ps1) | 预览或安装日报、采集、健康任务 | `-ProjectRoot` 指向程序包，`-ConfigDirectory` 选择配置，`-Kind daily\|collect\|health`；`-Apply` 才安装，日报/采集先做相应自检 | 计划任务 |
| [remove_daily_task.ps1](remove_daily_task.ps1) | 预览或移除归属当前配置和用户的任务 | `-ProjectRoot`、`-ConfigDirectory`、`-Kind`；`-Apply` 才移除，保留业务数据 | 计划任务 |
| [check_daily_task.ps1](check_daily_task.ps1) | 只读核对任务归属、配置差异和上海时间触发信息 | `-ProjectRoot`、`-ConfigDirectory`、`-Kind`；需要 Windows 调度器可读权限 | 计划任务 |
| [daily_task_common.ps1](daily_task_common.ps1) | 共用 Rust `task plan`、任务 XML 校验和安装/移除逻辑 | 由任务脚本点加载；程序包内需有 `ai-daily-report.exe` | 计划任务 |
| [daily_task_plan.py](daily_task_plan.py) | 保留旧 Python 运行时的配置读取与任务计划 JSON/XML 生成 | `python scripts/daily_task_plan.py --sid <SID>`；需旧 Python 配置和源码，当前 PowerShell 任务脚本调用 Rust `task plan` | 计划任务 |
| [health_notify.ps1](health_notify.ps1) | 调用只读健康检查，核对宿主任务并写健康文件、异常通知 | `-Program`、`-ConfigDirectory`；`-NoNotify` 关闭本地通知，配置中的健康日志仍写入；无模型调用 | 计划任务 |

## 验收证据

引用核对覆盖根说明、`docs/**/*.md`、`scripts/**`、Rust 源码与测试，同时检查 Python 模块导入名；排除本次清理文档和索引自身的登记。下列 4 个顶层脚本没有查到其他使用引用，已明确标注保留。测量在隔离数据目录执行；计时期间不并发构建或测试。

| 文件 | 用途 | 入口或前置条件 | 分类 |
| --- | --- | --- | --- |
| [accept_windows_bundle.ps1](accept_windows_bundle.ps1) | 独立验收无 Python 的发布包、报告恢复和调度契约 | `-BundleDirectory`、全新 `-OutputDirectory`、`-Harness`；需构建 `bundle_acceptance` 测试程序，默认查找 `bundle-acceptance.exe` | 验收证据 |
| [accept_archive_bundle.py](accept_archive_bundle.py) | 用本地 HTTP 模型替身和合成数据验收归档 CLI | `--release`、`--output`；Windows/Python，依赖 `benchmark_archive`；历史证据脚本，保留不删 | 验收证据 |
| [archive_scanner_database.ps1](archive_scanner_database.ps1) | 调用 Rust `archive-scanner` 保存扫描数据库归档 | `-SourceDatabase`、`-ArchiveDirectory`，可指定 `-Program`；涉及数据库归档写入，需相应授权 | 验收证据 |
| [export_rust_migration_baseline.py](export_rust_migration_baseline.py) | 从合成输入冻结旧 Python 契约供 Rust 回归核对 | `python scripts/export_rust_migration_baseline.py`；需旧 Python、原生扩展及 `.artifacts/rust-migration/rust-baseline.log`，会写 Rust 测试夹具 | 验收证据 |
| [coverage_optimization_evidence.py](coverage_optimization_evidence.py) | 采集配置与混合语料覆盖优化证据 | `--runtime-root`、`--worker-root`、`--output`，可选 `--config-only` / `--mixed-corpus-only`；旧 Python/native 环境；历史证据脚本，保留不删 | 验收证据 |
| [coverage_release_evidence.py](coverage_release_evidence.py) | 将旧安装包、Git 状态与离线闸门日志绑定为证据 | `--bundle`、`--output`，可重复 `--gate-log`；依赖 `windows_release.py`；历史证据脚本，保留不删 | 验收证据 |
| [verify_coverage_corpus.py](verify_coverage_corpus.py) | 通过已安装原生扫描器验证固定合成语料 | `--corpus`、`--output`；需旧 Python、原生扩展及匹配工作进程；历史证据脚本，保留不删 | 验收证据 |
| [benchmark_archive.py](benchmark_archive.py) | 测量归档全文件、增量和无变化采集的耗时、资源与内容签名 | `--release`、`--corpus`、`--output`、`--reference`，可选 `--phase sample\|full\|incremental\|all`；Windows，零模型调用 | 验收证据 |
| [benchmark_scanner.py](benchmark_scanner.py) | 通过旧 Python/native 链路输出冷/热扫描性能证据 | `--work-dir`、独立 `--state-dir`，可选 `--office-worker-path`、`--iterations`；需旧 Python 原生环境，输出 JSON/Markdown | 验收证据 |
| [pdf_benchmark/generate_classifier_corpus.py](pdf_benchmark/generate_classifier_corpus.py) | 生成固定 PDF 分类器数值验证语料与清单 | `python scripts/pdf_benchmark/generate_classifier_corpus.py`；需 `reportlab`、`pypdf`；已有夹具跳过生成，保留基线 | 验收证据 |
| [pdf_benchmark/generate_corpus.py](pdf_benchmark/generate_corpus.py) | 生成含中英文参考正文的合成 PDF 基准语料 | `python scripts/pdf_benchmark/generate_corpus.py`；需 `reportlab`；已有 PDF 和参考文本跳过生成 | 验收证据 |
| [pdf_benchmark/metrics.py](pdf_benchmark/metrics.py) | 提供 PDF 对照测量共用的可打印率与正文保真度指标 | 由同目录 Python/Rust 测量脚本导入；保留历史比较口径 | 验收证据 |
| [pdf_benchmark/run_python.py](pdf_benchmark/run_python.py) | 用 `pdfplumber` 输出固定语料的质量与耗时 | `python scripts/pdf_benchmark/run_python.py`；需固定语料与 `pdfplumber`，写 `results/python.json` | 验收证据 |
| [pdf_benchmark/run_rust.py](pdf_benchmark/run_rust.py) | 用历史 `pdf-extract` 候选程序输出对称测量结果 | `python scripts/pdf_benchmark/run_rust.py`；需固定语料与 `rust/pdf_extract_bench` 的 release 程序，写 `results/rust.json` | 验收证据 |
| [pdf_benchmark/summarize.py](pdf_benchmark/summarize.py) | 汇总两侧原始结果并按冻结阈值生成迁移判定 | `python scripts/pdf_benchmark/summarize.py`；先保留并核对两份原始 JSON，写 `results/summary.json` | 验收证据 |
| [pdf_benchmark/results/python.json](pdf_benchmark/results/python.json) | 保存 Python PDF 测量原始结果 | `run_python.py` 生成；历史证据，保留不删，不代表当前生产性能 | 验收证据 |
| [pdf_benchmark/results/rust.json](pdf_benchmark/results/rust.json) | 保存 Rust PDF 候选测量原始结果 | `run_rust.py` 生成；历史证据，保留不删，不代表当前生产解析路径 | 验收证据 |
| [pdf_benchmark/results/summary.json](pdf_benchmark/results/summary.json) | 保存冻结门槛下的历史质量与性能比较 | `summarize.py` 生成；保留两侧原始结果和未通过项 | 验收证据 |

## 旧 Python 保留

| 文件 | 用途 | 入口或前置条件 | 分类 |
| --- | --- | --- | --- |
| [backfill_daily_reports.py](backfill_daily_reports.py) | 对审查过的历史日期补录日报并维护可续跑账本和调用预算 | 旧 Python `3.13.13`、`src/`、匹配扩展/工作进程与配置；使用 `--start-date` / `--end-date`、`--preflight` / `--plan` 和预算参数，`--dry-run` 检查计划；保留，不用于当前生产，模型调用与发布需单独授权 | 旧 Python 保留 |

## 保留边界

旧 Python 源码、`.venv/`、`rust/scanner_native`、`rust/pdf_extract_bench`、匹配旧工作进程与发布包继续保留，供恢复、回退和历史测量使用。扫描核心现有 `#[allow(dead_code)]` 豁免不在本次清理范围。

一次性探查脚本与生成 dump 的移除清单、引用核对及恢复方法见[工程清理自检记录](../docs/plans/2026-10-18-engineering-cleanup-plan.md)。`.tmp/` 的历史日志、`outputs/` 中被归档文档引用的原始工作簿和业务证据继续保留。
