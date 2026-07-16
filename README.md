# 审计日报生成器 v5.0

基于 LLM 的审计报告自动化工具。支持日报、周报、月报三种模式，支持扫描工作目录文件（Excel/PDF/PPTX/Word/TXT/Markdown），并结合用户输入生成结构化报告。

## 部署边界

当前仓库提供的是**源码式单机部署**：在仓库 checkout 中创建本地 `.venv` 并从 `main.py` 运行。它不是 wheel/安装包，也未自带 Windows Service、定时任务、容器镜像或多机编排。若要用计划任务调度，应把计划任务的进程启动目录固定为仓库根目录，并调用该 checkout 内的 Python 绝对路径。

## 快速开始

### Windows（PowerShell）

```powershell
# 1) 先创建本机配置；已有 settings.yaml 或 Windows 配置时不覆盖
if (
    -not (Test-Path -LiteralPath "config\settings.yaml") -and
    -not (Test-Path -LiteralPath "config\settings.windows.yaml")
) {
    Copy-Item -LiteralPath "config\settings.example.yaml" -Destination "config\settings.windows.yaml"
}

# 2) 编辑实际使用的本机配置，至少设置真实 paths.work_dir
# Windows YAML 路径使用正斜杠，例如 D:/audit/work

# 3) 先在当前进程或组织的凭据注入系统中设置所选 provider 密钥
#    DeepSeek: DEEPSEEK_API_KEY    OpenAI: OPENAI_API_KEY

# 4) 幂等部署：保留现有配置，复用 .venv，安装依赖并运行 doctor
.\scripts\deploy_windows.ps1

# 需要默认 Rust 性能路径时，本机安装 Rust toolchain 后加上：
.\scripts\deploy_windows.ps1 -BuildRust

# 5) 生成日报
.\.venv\Scripts\python.exe main.py daily -i "今日完成XX审计"
```

`deploy_windows.ps1` 不接受 API Key 参数，也不会写入或持久化 API Key。它优先使用 `requirements.lock`；锁文件不存在时才回退到 `requirements.txt`。因为最后会运行 `doctor`，请在调用脚本前完成路径和密钥注入。

维护者只有在 `requirements.txt` 发生受审查的变更后才更新锁文件，并同时验证 Python 3.10 与 3.13：

```powershell
uv pip compile requirements.txt --universal --python-version 3.10 --generate-hashes --output-file requirements.lock
```

### Linux

```bash
# 1) 先创建并编辑本机配置；已有 settings.yaml 时保留
if [ ! -f config/settings.yaml ] && [ ! -f config/settings.linux.yaml ]; then
  cp config/settings.example.yaml config/settings.linux.yaml
fi

# 2) 创建独立环境，优先使用锁定依赖
python3 -m venv .venv
dependency_file=requirements.txt
if [ -f requirements.lock ]; then dependency_file=requirements.lock; fi
./.venv/bin/python -m pip install --requirement "$dependency_file"

# 3) 在当前进程或组织凭据系统中注入 DEEPSEEK_API_KEY / OPENAI_API_KEY
./.venv/bin/python main.py doctor
./.venv/bin/python main.py daily -i "今日完成XX审计"
```

## 常用命令

```bash
# 日报
python main.py daily
python main.py daily -i "今日工作内容"
python main.py daily --no-save -i "预览模式"
python main.py daily --date 2026-02-05 -i "..."

# 周报
python main.py weekly --source db
python main.py weekly 2026-W05 --source scan
python main.py weekly --source db -i "补充说明"

# 月报
python main.py monthly --source db
python main.py monthly 2026-01 --source scan

# 列出已有日报日期
python main.py list

# 检查环境与配置
python main.py doctor

# 测试（先安装开发依赖）
python -m pip install -r requirements-dev.txt
python -m pytest tests/ -v

# scanner benchmark（真实扫描链路）
python scripts/benchmark_scanner.py \
  --start-date 2026-05-24 \
  --end-date 2026-05-25 \
  --json-out data/benchmarks/scanner.json \
  --markdown-out data/benchmarks/scanner.md
```

## 数据来源（`--source`）

- `db`: 从 SQLite 历史库聚合（推荐）
- `scan`: 直接扫描工作目录文件

## 升级说明

- 当前版本只支持现行 SQLite schema。若沿用旧版 `data/db/reports.sqlite3` 并触发 schema 过期错误，请先备份数据库，再按当前结构重建。
- 当前版本不再提供旧 JSON 历史数据到 SQLite 的自动迁移。若历史数据仍停留在旧 JSON 载体中，`weekly --source db` 和 `monthly --source db` 不会自动读取这部分内容。

## 配置说明

### 本机配置文件

运行时按以下顺序读取并合并本机配置，后加载的文件优先：

- 可选通用本机配置：`config/settings.yaml`
- Linux: `config/settings.linux.yaml`；Windows: `config/settings.windows.yaml`
- 敏感配置：`config/.secrets.yaml`

这些文件均已加入 `.gitignore`，不会提交到 GitHub。仓库只提交 `config/settings.example.yaml` 作为示例。已有 `config/settings.yaml` 的部署可直接继续使用；新部署更推荐把非敏感项放入系统专用配置，把密钥放入环境变量或 `config/.secrets.yaml`。为兼容已有本机配置，`llm.DEEPSEEK_API_KEY` 也可读取，但不应提交。

```bash
# Linux
if [ ! -f config/settings.yaml ] && [ ! -f config/settings.linux.yaml ]; then
  cp config/settings.example.yaml config/settings.linux.yaml
fi
```

```powershell
# Windows PowerShell
if (
    -not (Test-Path -LiteralPath "config\settings.yaml") -and
    -not (Test-Path -LiteralPath "config\settings.windows.yaml")
) {
    Copy-Item -LiteralPath "config\settings.example.yaml" -Destination "config\settings.windows.yaml"
}
```

```yaml
paths:
  # Linux: /home/george/bochu_work
  # Windows: D:/audit/work
  work_dir: "/home/george/bochu_work"
  data_dir: "data"
  reports_dir: "data/reports"
  db_dir: "data/db"

llm:
  provider: "deepseek"            # deepseek | openai
  model_id: "deepseek-chat"       # OpenAI 示例: gpt-4o-mini
  temperature: 0.2
  max_tokens: 8192
  max_retries: 3
```

- 默认使用 `deepseek`，因此只配置 `OPENAI_API_KEY` 但不切换 `llm.provider` 时，`python main.py doctor` 仍会按 DeepSeek 路径校验并报缺少 `DEEPSEEK_API_KEY`。
- OpenAI 用户请先把 `llm.provider` 改成 `openai`，再配置 `OPENAI_API_KEY`。

### Rust scanner/context core

生产扫描路径固定为 Python 应用壳加 Rust scanner/context core。Python 负责 CLI、配置、报告存储、模板与 LLM；Rust 负责发现、分类、parser 路由、worker deadline、缓存、审计和确定性 context 压缩。

```yaml
scanner:
  engine: "rust_v2"
  rust_scanner_bin: "rust/target/release/ai-daily-scanner"
  rust_office_parser_bin: "rust/target/release/ai-daily-office-parser"
  rust_index_db_path: "data/db/scan_index_v2.sqlite3"
```

Discovery 已作为 library 链接进主 scanner，不再部署独立 discovery executable。Office parser 继续作为崩溃隔离 worker；PDF 和配置允许的旧 Office 格式由 Python document worker 处理。parser fallback 由 Rust core 在单文件总 deadline 内决策和审计，不存在顶层 scanner 静默回退。

在 Windows 源码 checkout 中构建和验收：

```powershell
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
.\.venv\Scripts\python.exe main.py doctor --strict
```

`doctor --strict` 会验证 scanner 合同与 build、v2 数据库父目录和两个 worker handshake；它不会解析业务文件或调用 LLM。

`.xlsx` 使用 `rust_xlsx_bounded_v1`，`.docx` / `.pptx` 使用 `rust_office_oxide_v1`。`parser_backend` 表示真正产生内容的 parser，`worker_lane` 表示执行隔离 lane，两者必须分别审计。

使用合成或已批准脱敏目录运行 benchmark，并指定临时 v2 数据库：

```powershell
.\.venv\Scripts\python.exe scripts\benchmark_scanner.py `
  --start-date 2026-05-24 `
  --end-date 2026-05-25 `
  --scan-db-path .tmp\scanner-benchmark\scan_index_v2.sqlite3 `
  --json-out .tmp\scanner-benchmark\scanner.json `
  --markdown-out .tmp\scanner-benchmark\scanner.md
```

完整的路由、cache identity 和审计说明见 `docs/scanner-backends.md`。

### 数据外发与真实烟测

扫描到的审计文件内容会被解析、压缩并填入 prompt，随后发送给配置的第三方 LLM provider（DeepSeek 或 OpenAI）。`--no-save` 只是不保存本地报告，**不会阻止文件内容发送给 LLM**。

生产使用前必须先完成数据分级和 provider 合规审批，将 `paths.work_dir` 限定在最小必要范围，用 `scanner.excluded_dirs` 排除敏感目录，用 `scanner.ignored_patterns` 排除敏感文件模式，并在源文件或进入扫描目录前完成脱敏。不要把未经审批的大范围共享盘直接设为 `work_dir`。

最后用可脱敏的真实样本执行一次 LLM 烟测；该命令会真实调用 provider、发送扫描内容并可能产生费用：

```powershell
.\.venv\Scripts\python.exe main.py daily --no-save -i "部署烟测"
```

验收时应同时确认命令退出码为 0、生成内容完整、日志没有未预期 fallback，且 `--no-save` 未新增本地报告。

### `config/.secrets.yaml`

```yaml
api:
  deepseek_api_key: "your-deepseek-key"
  openai_api_key: "your-openai-key"

proxy:
  http_proxy: "http://127.0.0.1:10808"
  https_proxy: "http://127.0.0.1:10808"
```

## 存储说明

- 当前默认历史存储：`data/db/reports.sqlite3`
- Markdown 报告输出：`data/reports/`

## 项目结构

```text
config/                # 配置文件
  settings.example.yaml     # GitHub 示例配置
  settings.yaml             # 可选通用本机配置（不提交）
  settings.linux.yaml       # Linux 本机配置（不提交）
  settings.windows.yaml     # Windows 本机配置（不提交）
  .secrets.yaml             # 本机敏感配置（不提交）

data/
  db/                  # SQLite 数据库目录（reports.sqlite3）
  reports/             # Markdown 报告输出（日/周/月）

src/
  core/                # 配置、环境检查、日志、LLM 客户端
  models/              # Pydantic 数据模型
  services/            # 文件扫描、parser backend、SQLite 存储、报告生成
  utils/               # 工具函数

rust/
  discovery/           # Rust 文件发现 CLI
  office_parser/       # Rust Office parser CLI

scripts/
  benchmark_scanner.py # 真实 scanner 性能与 backend 证据

templates/             # Prompt + Jinja2 模板
tests/                 # pytest 测试
main.py                # CLI 入口
```

## 技术栈

Python 3.10+ | Rust | DeepSeek/OpenAI | SQLite | Pydantic | Dynaconf | Jinja2 | Pandas
