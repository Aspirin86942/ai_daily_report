# CLAUDE.md

## Project Overview

审计报告生成器 v5.0：基于 LLM 的日报/周报/月报自动生成工具。

当前核心能力：
- LLM provider 可切换：`deepseek` / `openai`
- 报告结构化输出：JSON + Pydantic 校验
- 历史数据默认存储：SQLite（`data/db/reports.sqlite3`）
- 支持 `db`（历史聚合）与 `scan`（文件扫描）两种数据来源
- scan 路径默认使用 Rust scanner core（Rust discovery + PyO3 `scanner_native` 内嵌调用）；`.xlsx` 走 `rust_xlsx_bounded_v2` 有界预览，`.docx` / `.pptx` 走 `rust_office_oxide_v2`，PDF 与显式启用的 legacy 格式走 Python document worker。Office parser 超时默认不 fallback（除非显式开启 `fallback_after_timeout`），无顶层静默 fallback，详见 `docs/scanner-backends.md` 与 ADR 0002。

## Commands

Python 依赖与测试统一走 uv；Rust 组件仍用 cargo。Windows 上由 Miniforge
提供基础 Python，但不激活 Conda base；uv 使用项目独立的 `.venv`，并把缓存
放在同盘的 `.uv/cache` 以使用 hardlink。

```bash
# 安装依赖（uv）
uv sync

# 环境与配置检查
uv run python main.py doctor

# 日报
uv run python main.py daily
uv run python main.py daily -i "今日工作内容"
uv run python main.py daily --no-save -i "预览模式"
uv run python main.py daily --date 2026-02-05 -i "..."

# 周报
uv run python main.py weekly --source db
uv run python main.py weekly 2026-W05 --source scan -i "补充"
uv run python main.py weekly --source db --no-save

# 月报
uv run python main.py monthly --source db
uv run python main.py monthly 2026-01 --source scan -i "补充"

# 列表
uv run python main.py list

# 测试
uv run pytest

# Rust scanner helpers
cd rust/discovery && cargo test && cargo build --release
cd rust/office_parser && cargo test && cargo build --release
```

## Architecture

```text
日报:
  用户输入 + 文件扫描 + 昨日计划
      ↓
  LLMClient.generate_report() -> JSON
      ↓
  DailyReportData (Pydantic)
      ↓
  ├─→ SQLite: daily_reports
  └─→ Markdown: data/reports/YYYY-MM/YYYY-MM-DD.md

周报/月报:
  source=db: 从 SQLite 聚合历史日报
  source=scan: 扫描文件构造上下文
      ↓
  LLMClient.generate_weekly_report / generate_monthly_report
      ↓
  WeeklyReportData / MonthlyReportData
      ↓
  ├─→ SQLite: weekly_reports / monthly_reports
  └─→ Markdown: data/reports/weekly|monthly/
```

## Project Structure

```text
src/
├── cli/                 # CLI 子命令 (daily/weekly/monthly/list/doctor)
├── core/
│   ├── config.py        # 单例配置 (Dynaconf)
│   ├── healthcheck.py   # CLI 环境检查
│   ├── llm.py           # DeepSeek/OpenAI 客户端 + JSON 校验重试
│   └── logger.py
├── models/
│   ├── schemas.py       # 报告 Pydantic 模型 (日/周/月)
│   └── scanner_contract.py  # scanner/worker 契约 DTO 镜像
├── services/
│   ├── sqlite_store.py  # SQLite 存储实现（日/周/月）
│   ├── native_scanner.py    # PyO3 scanner_native 适配（scanner core 内嵌调用）
│   ├── scanner_config.py    # scanner settings 归一化/校验
│   ├── document_parser.py   # Python document 解析 (PDF/legacy lane)
│   ├── report_gen.py
│   └── report_runner/    # 报告运行编排 (requests/outcomes/runner)
├── workers/             # crash-isolated Python worker (document/PDF classifier)
└── utils/
    └── text_tools.py

rust/                    # Cargo workspace
├── scanner_core/        # Rust scanner/context core (store/scheduler/session/…)
├── scanner_native/      # PyO3 CPython 扩展（scanner core 内嵌）
├── scanner_contract/    # 共享契约 DTO 与版本常量
├── worker_contract/     # worker-v2 信封契约
├── discovery/           # Rust discovery
└── office_parser/       # Rust Office parser worker CLI
```

## Key Patterns

- LLM 输出：使用 JSON 模式 + Pydantic 严格校验
- 扫描策略：单文件预算内正文逐字保留；超预算文件头+尾行边界兜底（`.log` 尾优先）；默认全局 500k / 单文件 100k 字符，均可用 scanner profile 覆盖
- Scanner backend：`parser_backend` 表示内容由谁解析，`worker_lane` 表示执行通道，不能混用
- Office parser：`.xlsx` 走 `rust_xlsx_bounded_v2` 有界预览，`.docx` / `.pptx` 走 `rust_office_oxide_v2`；Rust 超时默认不 fallback，除非显式开启 `fallback_after_timeout`
- Cache profile：解析缓存身份由 Rust core 归一化 settings 唯一决定——backend、fallback、timeout、预算字段、策略版本常量与 scanner/worker 构建指纹均参与失效；Python 仅传输显式配置的 wire 叶子，详见 `docs/scanner-backends.md`
- 存储策略：SQLite 作为程序事实源，Markdown 作为阅读输出
- 周边界：ISO 周（Monday-Sunday）

## Deep Docs

- `docs/scanner-backends.md`: scanner discovery/parser backend、fallback、cache profile、benchmark 读法和验证命令。

## Modifying LLM Output

日报输出改动：
1. `templates/system_prompt.md`
2. `src/models/schemas.py` 的 `DailyReportData`
3. `templates/report_template.md`

周报输出改动：
1. `templates/weekly_prompt.md`
2. `src/models/schemas.py` 的 `WeeklyReportData`
3. `templates/weekly_template.md`

月报输出改动：
1. `templates/monthly_prompt.md`
2. `src/models/schemas.py` 的 `MonthlyReportData`
3. `templates/monthly_template.md`

## LLM Provider Notes

- `llm.provider = "deepseek"` 时，使用 `DEEPSEEK_API_KEY`（或 `api.deepseek_api_key`），默认模型 `deepseek-chat`
- `llm.provider = "openai"` 时，使用 `OPENAI_API_KEY`（或 `api.openai_api_key`）
- `llm.base_url` 可覆盖 API 端点（缺省时 DeepSeek 用 `https://api.deepseek.com`，OpenAI 用 SDK 默认）
- DeepSeek 和 OpenAI 均使用 Chat Completions API + JSON schema 输出
- openai SDK 在 `LLMClient` 构造时才导入（懒导入），避免拖慢 `list`/`doctor` 等命令启动

## CodeGraph

本项目已由 CodeGraph 索引（`.codegraph/` 存在）。需要理解或定位 `src/`、`rust/` 代码时，**先用**
`codegraph explore "<符号名或问题>"`（或 MCP `codegraph_explore`）——一次返回相关符号的逐字源码、
调用链与 blast radius，能跨动态分发，比 Grep/Read 更准更快。仅在输出不足以回答时，才回退到
Grep/Glob/Read。
