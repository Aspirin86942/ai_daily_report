# 审计日报生成器 v5.0

仅支持 Windows x64 与 CPython 3.13.13。生产扫描链已经收口为：

```text
CLI → ReportRunner → NativeScanner → PyO3 Scanner
→ scanner_core → worker v2 pools
```

Python 负责 CLI、配置、报告 SQLite、模板、Markdown 和 LLM；Rust 在当前
Python 进程内负责文件发现、分类、路由、缓存、审计和确定性上下文压缩。
Office/PDF 解析仍在隔离 worker 中执行。一次扫描只有一次 PyO3 调用，不启动
scanner 子进程，也不序列化 scanner JSON transport。

## Windows 源码运行

在仓库根目录的 PowerShell 中执行：

```powershell
if (
    -not (Test-Path -LiteralPath "config\settings.yaml") -and
    -not (Test-Path -LiteralPath "config\settings.windows.yaml")
) {
    Copy-Item -LiteralPath "config\settings.example.yaml" `
        -Destination "config\settings.windows.yaml"
}

# 编辑本机配置，至少把 paths.work_dir 改成获批的绝对目录。
# 在当前进程或凭据系统中注入 DEEPSEEK_API_KEY / OPENAI_API_KEY。
.\.venv\Scripts\python.exe -m pip install -r requirements.txt
$env:PYO3_PYTHON = (Resolve-Path '.\.venv\Scripts\python.exe').Path
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
.\.venv\Scripts\python.exe main.py doctor --strict
.\.venv\Scripts\python.exe main.py daily -i "今日完成 XX 审计"
```

根目录 `.python-version` 是开发、Release 和部署唯一版本来源。原生模块在首次
扫描或 doctor 时才导入；源码 checkout 会加载 release build 生成的
`ai_daily_scanner_native.dll`。版本不精确匹配 CPython 3.13.13 时导入直接失败。

## 常用命令

```powershell
# 日报、周报、月报
.\.venv\Scripts\python.exe main.py daily -i "今日工作内容"
.\.venv\Scripts\python.exe main.py daily --no-save -i "预览模式"
.\.venv\Scripts\python.exe main.py weekly --source db
.\.venv\Scripts\python.exe main.py weekly 2026-W05 --source scan
.\.venv\Scripts\python.exe main.py monthly 2026-01 --source db
.\.venv\Scripts\python.exe main.py list

# 完整验证
.\.venv\Scripts\python.exe -m pytest tests -v
cargo fmt --manifest-path rust/Cargo.toml --all -- --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked

# 只对合成或已批准脱敏目录采 cold/warm 多样本。
$benchmarkRoot = Join-Path $env:TEMP ('ai-daily-benchmark-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $benchmarkRoot | Out-Null
.\.venv\Scripts\python.exe scripts\benchmark_scanner.py `
  --work-dir (Resolve-Path 'tests\fixtures\worker_documents') `
  --state-dir $benchmarkRoot `
  --start-date 2000-01-01 `
  --end-date 2100-01-01 `
  --iterations 5 `
  --json-out (Join-Path $benchmarkRoot 'scanner.json') `
  --markdown-out (Join-Path $benchmarkRoot 'scanner.md')
```

Benchmark 不读取本机 `settings.yaml`，每组 cold/warm 使用独立的新鲜 v3 数据库，
并报告 median、nearest-rank p95、吞吐、worker 峰值 RSS、完整复用状态、scanner
进程启动次数和 scanner transport 字节数。历史单样本不能作为当前 p95。

## Scanner 接口与路由

外部报告 seam 保持为：

```python
ReportRunner.run(ReportRunRequest) -> ReportRunOutcome
```

唯一 Python scanner seam 为：

```python
scanner = NativeScanner(config)
scanner.build_context(ScanRequest) -> ScanResult
scanner.doctor() -> DoctorResponse
```

`ScanRequest` 只包含报告模式、日期范围和可选压缩配置；`source` 与用户输入不会
进入 scanner。`ScanResult` 同时返回上下文 envelope 与本次运行的完整 evidence，
无需二次查询数据库。

路由身份固定为：

- 文本类：`light_text_v2` / `rust_core`；
- `.xlsx`：`rust_xlsx_bounded_v2` / `rust_office_process_v2`；
- `.docx`、`.pptx`：`rust_office_oxide_v2` / `rust_office_process_v2`；
- PDF：`python_pdf_text_v2` / `python_document_process_v2`；
- 显式启用的旧 Office：Python document worker 的 v2 backend/lane。

`parser_backend` 与 `worker_lane` 是独立审计维度。普通失败遵循 Rust 编译期固定的
fallback 顺序；超时 fallback 默认关闭，只能用 `fallback_after_timeout` 开启。

## 配置与数据边界

源码模式按顺序合并本机配置：

- `config/settings.yaml`（可选）；
- `config/settings.windows.yaml`；
- `config/.secrets.yaml`（可选敏感项）。

只跟踪 `config/settings.example.yaml`。scanner 路径收口为 `index_db_path` 和
`office_worker_path`；Python executable 和 module root 从当前精确运行环境推导。
未知或已删除 scanner 键会在启动/doctor 时报错，不提供别名兼容。

源码默认数据位置：

- 报告 SQLite：`data/db/reports.sqlite3`；
- scanner SQLite：`data/db/scan_index_v3.sqlite3`；
- Markdown：`data/reports/`。

scanner 数据库只接受 `user_version=3`。文件不存在时创建；任何其他版本只读拒绝，
不迁移、不修补。报告 SQLite、模板、Markdown 路径以及 SQLite-before-Markdown
发布顺序不受 scanner 重构影响。`--no-save` 不保存报告，但允许 scanner run/cache
副作用。

扫描内容会进入 prompt 并发送给所选第三方 LLM。`--no-save` 不阻止外发。生产前
必须完成数据分级与 provider 合规审批，并把 `paths.work_dir` 限定为最小必要目录。
doctor、构建验证和 benchmark 使用合成数据不构成业务目录授权。

## Windows Release 与回滚

仓库内 release 工具只构建和验证产物，不自动安装、部署、切换配置或处理真实数据库：

```powershell
New-Item -ItemType Directory -Path .\dist -Force | Out-Null
.\scripts\build_windows_release.ps1 `
  -OutputDirectory .\dist\ai-daily-report-2026.08.12 `
  -ReleaseVersion 2026.08.12
```

产物固定包含 CPython 3.13.13 `cp313-win_amd64` wheel、
`ai-daily-office-parser.exe`、Python 应用文件及带 SHA-256/build identity 的
`manifest.json`。构建脚本会在一次性 3.13.13 venv 中安装、导入 wheel，并验证
错误版本拒绝。

实际切换前必须另行授权停止报告进程、归档旧 scanner 数据库和修改 release
pointer。旧数据库与报告 SQLite 不删除；新 release 使用新的
`scan_index_v3.sqlite3`。回滚只切回旧 release/旧数据库指针，新 v3 数据库留作
诊断，不做反向转换。详见 [Windows 部署说明](docs/windows-deployment.md)。

## 项目结构

```text
main.py                      # lazy Python CLI
src/core/                    # 配置、healthcheck、日志、LLM
src/services/report_runner/  # 报告业务深模块
src/services/native_scanner.py # 唯一 Python scanner adapter
src/workers/                 # crash-isolated Python worker v2
rust/scanner_native/         # PyO3 CPython 3.13 extension
rust/scanner_core/           # scanner/context 深模块
rust/scanner_contract/       # scanner 领域 DTO
rust/worker_contract/        # worker v2 envelope
rust/discovery/              # discovery library
rust/office_parser/          # crash-isolated Office worker
scripts/                     # benchmark、release、归档与 pointer 工具
templates/                   # prompt 和 Markdown 模板
tests/                       # Python/Windows release tests
```

项目只承诺 Windows x64、PowerShell、CPython 3.13.13、
`.venv\Scripts\python.exe` 与 `doctor --strict` 组合下的行为；不提供 abi3、其他
Python 版本或 Linux 兼容层。
