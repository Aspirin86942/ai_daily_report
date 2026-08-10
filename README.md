# 审计日报生成器 v5.0

仅支持 Windows x64 与 CPython 3.13.13 的审计报告工具。生产架构固定为 **Python application shell +
Rust scanner/context core**：Python 负责 CLI、配置、报告数据库、模板和 LLM；
Rust 负责文件发现、分类、parser 路由、worker deadline、缓存、审计和确定性
context 压缩。根目录 `.python-version` 是开发、CI、Release 和部署共同使用的
唯一 Python 版本来源。

## Windows 源码快速开始

在 PowerShell 中从仓库根目录执行：

```powershell
if (
    -not (Test-Path -LiteralPath "config\settings.yaml") -and
    -not (Test-Path -LiteralPath "config\settings.windows.yaml")
) {
    Copy-Item -LiteralPath "config\settings.example.yaml" `
        -Destination "config\settings.windows.yaml"
}

# 编辑本机配置，至少把 paths.work_dir 改成绝对路径。
# 在当前进程或组织凭据系统中注入 DEEPSEEK_API_KEY / OPENAI_API_KEY。
.\scripts\deploy_windows.ps1
.\.venv\Scripts\python.exe main.py doctor --strict
.\.venv\Scripts\python.exe main.py daily -i "今日完成 XX 审计"
```

`deploy_windows.ps1` 保留已有配置和 `.venv`，并先验证 `-Python` 指向的创建者
解释器及已有 `.venv` 均为 CPython 3.13.13，再从 `requirements.lock` 安装依赖、
构建 locked Rust workspace 并运行 strict doctor。版本不符时脚本会报告期望值、
实际值和修复提示，不会自动删除或重建已有 `.venv`。脚本不接受、输出或持久化
API Key。`doctor --strict` 验证 scanner 合同/build、v2 数据库父目录和两个隔离
worker handshake；它不打开业务文件，也不调用 LLM。

## Windows release 安装与回滚

预构建包必须先由源码 checkout、已验证安装或独立认证分发中的
`verify_windows_package.ps1` 验证。不要先解压并执行归档内脚本。

```powershell
# 维护者：先完成 locked release build，再生成归档。
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
.\scripts\package_windows.ps1 `
  -OutputPath .\dist\ai-daily-report-windows-x64.zip `
  -ReleaseVersion "2026.07.16"

# 使用可信 checkout 中的 bootstrap 做结构、allowlist、hash 和 handshake 验证。
.\scripts\verify_windows_package.ps1 `
  -ArchivePath .\dist\ai-daily-report-windows-x64.zip

$installRoot = "D:\ai-daily-report"
New-Item -ItemType Directory `
  -Path "$installRoot\shared\config" -Force | Out-Null
Copy-Item -LiteralPath .\config\settings.example.yaml `
  -Destination "$installRoot\shared\config\settings.windows.yaml"
# 编辑 shared 配置：paths.work_dir 必须是绝对路径；配置文件不会被安装器改写。

.\scripts\install_windows_release.ps1 `
  -ArchivePath .\dist\ai-daily-report-windows-x64.zip `
  -InstallRoot $installRoot

# 可以从任意 cwd 调用。
& "$installRoot\run_current_release.ps1" doctor --strict
& "$installRoot\run_current_release.ps1" list

# 校验 previous release、运行 strict doctor，再原子切回。
& "$installRoot\rollback_windows_release.ps1"
```

安装采用并排目录和原子指针：

```text
<install-root>/
  current.json
  releases/<version>/
  shared/config/
  shared/data/reports/
  shared/data/db/
  shared/logs/
  run_current_release.ps1
  rollback_windows_release.ps1
```

release 切换与回滚不会复制或重写 `shared/config`、数据或日志。launcher 设置
六个绝对环境路径，并拒绝缺失、相对、逃逸到 `shared/` 外或 version-local 的
目录。详细的包格式、可信边界和恢复流程见
[`docs/windows-deployment.md`](docs/windows-deployment.md)。

`manifest.json`、`SHA256SUMS` 和握手提供完整性/损坏检测，不证明发布者身份。
V1 不自动下载归档；远程 artifact 在没有 Authenticode 或与预期仓库/tag 绑定的
artifact attestation 前，不应仅凭自带 hash 被称为可信。

## 常用命令

```powershell
# 日报、周报、月报
.\.venv\Scripts\python.exe main.py daily -i "今日工作内容"
.\.venv\Scripts\python.exe main.py daily --no-save -i "预览模式"
.\.venv\Scripts\python.exe main.py weekly --source db
.\.venv\Scripts\python.exe main.py weekly 2026-W05 --source scan
.\.venv\Scripts\python.exe main.py monthly 2026-01 --source db
.\.venv\Scripts\python.exe main.py list

# 验证
.\.venv\Scripts\python.exe -m pytest tests -v
cargo fmt --manifest-path rust/Cargo.toml --all --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path rust/Cargo.toml --workspace --locked

# 只对合成或已批准脱敏目录做 scanner benchmark。
.\.venv\Scripts\python.exe scripts\benchmark_scanner.py `
  --start-date 2026-05-24 `
  --end-date 2026-05-25 `
  --scan-db-path .\.tmp\scanner-benchmark\scan_index_v2.sqlite3 `
  --json-out .\.tmp\scanner-benchmark\scanner.json `
  --markdown-out .\.tmp\scanner-benchmark\scanner.md
```

`--source db` 从本地报告 SQLite 聚合；`--source scan` 经 Rust core 扫描
`paths.work_dir`。Discovery 是链接进 scanner 的 Rust library，不再有独立
discovery executable。Office worker 保持独立进程隔离：`.xlsx` 使用
`rust_xlsx_bounded_v1`，`.docx` / `.pptx` 使用 `rust_office_oxide_v1`。
PDF 和明确启用的旧 Office 格式走 Python document worker。Rust 拥有 parser
fallback 决策，不存在顶层 scanner 静默回退。

## 配置与数据边界

源码模式按顺序合并：

- `config/settings.yaml`（可选通用本机配置）
- Windows 的 `config/settings.windows.yaml`
- `config/.secrets.yaml`（可选敏感配置）

这些本机文件不提交。建议把非敏感项放在 Windows 配置，把密钥放在环境变量或
`.secrets.yaml`。默认 provider 是 DeepSeek；切换到 OpenAI 时要同时设置
`llm.provider: openai` 和 `OPENAI_API_KEY`。

扫描内容会进入 prompt 并发送给所选第三方 LLM。`--no-save` 只禁止保存本地
报告，**不会阻止外发**。生产前必须完成数据分级和 provider 合规审批，将
`paths.work_dir` 限定为最小必要目录，并用 `excluded_dirs` / `ignored_patterns`
排除未授权内容。Benchmark、doctor、package E2E 使用合成数据不构成外发授权。

源码模式默认报告数据库为 `data/db/reports.sqlite3`，scanner 数据库为
`data/db/scan_index_v2.sqlite3`，Markdown 输出为 `data/reports/`。安装模式则
全部解析到 `<install-root>/shared`。

## 项目结构

```text
main.py                  # Python CLI
src/core/                # 配置、healthcheck、日志、LLM
src/services/            # Rust adapter、报告存储与生成
src/workers/             # crash-isolated Python document worker
rust/scanner_core/       # scanner/context core
rust/discovery/          # discovery library
rust/office_parser/      # crash-isolated Office worker
scripts/                 # 部署、打包、安装、回滚与 benchmark
templates/               # prompt 和 Markdown 模板
tests/                   # Python/Windows release tests
```

## 支持范围

项目仅承诺 Windows x64、PowerShell、CPython 3.13.13、
`.venv\Scripts\python.exe` 与 `doctor --strict` 组合下的生产和源码行为；不再提供
Linux 源码兼容性或 CI 承诺。
