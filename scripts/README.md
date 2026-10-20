# 脚本归属与保留说明

核对日期：2026-10-18。本目录顶层实际有 **23 个脚本**；其中生产/发布 6 个、计划任务 6 个、验收证据 9 个、旧 Python 保留 1 个、离线维护 1 个。原有脚本的文件头和关键参数已按历史记录逐项核对，本次补充维护脚本入口。另列出 `pdf_benchmark/` 的 6 个脚本和 3 份结果，覆盖目录内全部 32 个文件（本 README 除外）。

正式运行状态见[当前状态](../docs/current-state.md)。脚本归类仅说明用途；生产切换、任务安装/移除、模型调用、原始数据或证据覆盖仍需相应授权。新增离线维护入口及其隔离验证见下节；运行真实目录仍须相应授权。

## 离线维护

| 文件 | 用途 | 入口或前置条件 | 分类 |
| --- | --- | --- | --- |
| [compress_historical_artifacts.py](compress_historical_artifacts.py) | 对冻结清单内的两处历史解析产物执行 NTFS LZNT1 透明压缩、核验与回退 | Windows/NTFS、已有 Python 3.13 标准库、三项正式任务可只读查询；`plan` / `apply` / `verify` / `rollback` | 离线维护 |

此脚本不加入 Rust 生产发布包，不给产品增加 Python 依赖，也不修改保留的 Python 回退环境。它不调用采集、解析或模型，不启动、注册或改写计划任务。代码目录 `CodeRoot` 与待维护数据目录 `SourceRoot` 必须显式区分；脚本不会从当前目录猜测数据位置。

范围固定为相对 `SourceRoot` 的以下两处目录，且只处理其中名称恰为 `body.md`、`units.jsonl`、`units.idx` 的普通文件：

- `.artifacts/rust-migration/archive-benchmark-final-20261017-04/incremental/data/archive/extractions`
- `.artifacts/rust-migration/archive-benchmark-final-20261017/incremental/data/archive/extractions`

多硬链接、重解析点、加密、原本已压缩或扫描期间变化的项保留在清单的 `skipped` 中，附具体原因；这些项和范围外文件均不参与 apply/rollback。目录的压缩继承属性不改。

以下为维护命令模板。执行真实目录前须审阅 `plan` 生成的清单并取得对应授权；`Evidence` 必须是固定证据根下的全新目录，不得置于目标目录中。

```powershell
$Python = '<已有维护 Python 的绝对路径>'
$CodeRoot = '<维护代码的绝对目录>'
$SourceRoot = '<经授权的数据项目绝对目录>'
$Evidence = Join-Path $SourceRoot '.artifacts/rust-migration/historical-artifact-compression/<全新批次>'
& $Python -B "$CodeRoot\scripts\compress_historical_artifacts.py" plan --project-root $SourceRoot --evidence-dir $Evidence
$Manifest = Join-Path $Evidence 'manifest.json'
& $Python -B "$CodeRoot\scripts\compress_historical_artifacts.py" apply --manifest $Manifest
& $Python -B "$CodeRoot\scripts\compress_historical_artifacts.py" verify --manifest $Manifest
```

`plan` 只读目标文件，发布不可修改的 `manifest.json` 及其原始 UTF-8 字节摘要 `manifest.sha256`。其余命令每次另建 `runs/<run_id>/`，保存事件、任务窗口回读与最终汇总；旧证据全部保留。运行输出一条标准 JSON；`completed` / `paused` / `failed` 的退出码依次为 0 / 1 / 2，具体原因在 `pause_reason` / `error_code` / `message` 中。`verify` 报告当前压缩和未压缩数量。`failed` 的空间汇总不作最终节省实测，收益以成功运行及独立前后测量为准。

三项正式任务运行中、查询失败、任务缺失，或进入下一次触发前 300 秒时暂停；每 100 文件或累计 256 MiB 重新查询，单个大文件完整处理。空间不足时回退也暂停。排除原因后重复同一条 apply 或 rollback 命令即可续跑；程序重新核对冻结清单、身份、完整哈希和真实压缩状态，不依赖旧日志判断进度。重复 apply/rollback 的匹配项计入 `files_already`，不重复写入；重复 verify 创建新的只读核验证据。

**需要恢复时才执行**以下独立回退命令块，沿用上面的维护解释器、代码目录与已审阅清单变量；回退后再次核验：

```powershell
& $Python -B "$CodeRoot\scripts\compress_historical_artifacts.py" rollback --manifest $Manifest
& $Python -B "$CodeRoot\scripts\compress_historical_artifacts.py" verify --manifest $Manifest
```

回退先对全部候选作只读核验，再在同一锁定 HANDLE 解压并复核哈希、随机块和修改时间。它按当前分配量、原始逻辑大小与实际 NTFS 簇粒度保守估计增量，开始前保留额外 1 GiB，每次解压前重查余量。哈希、身份或非压缩属性不符时停止，保留具体错误，不覆盖内容或自动修复。完成后再次 verify，应看到全部候选恢复原始未压缩状态。

隔离测试入口：[test_historical_artifact_compression.py](../tests/test_historical_artifact_compression.py)。使用已有维护 Python 执行 `-B <CodeRoot>\tests\test_historical_artifact_compression.py -v`；测试仅在系统临时目录构造合成 NTFS 文件，不读取或修改真实历史产物。

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
| [deduplicate_historical_artifacts.py](deduplicate_historical_artifacts.py) | 对固定白名单内的冻结历史输入建立同卷硬链接，并保留逐文件核验与恢复日志 | Windows；`plan --project-root ... --output ...` 生成清单，`apply`、`verify`、`rollback` 必须同时指定 `--manifest` 和 `--sha256`；不处理正式归档及已有硬链接 | 历史材料维护 |
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
