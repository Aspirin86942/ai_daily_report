# CLAUDE.md

## Project Overview

审计报告生成器 v5.0：基于 LLM 的日报/周报/月报自动生成工具。

当前核心能力：
- LLM provider 可切换：`deepseek` / `openai`
- 报告结构化输出：JSON + Pydantic 校验
- 历史数据默认存储：SQLite（`data/db/reports.sqlite3`）
- 支持 `db`（历史聚合）与 `scan`（文件扫描）两种数据来源

## Commands

```bash
# 安装依赖
pip install -r requirements.txt

# 环境与配置检查
python main.py doctor

# 日报
python main.py daily
python main.py daily -i "今日工作内容"
python main.py daily --no-save -i "预览模式"
python main.py daily --date 2026-02-05 -i "..."

# 周报
python main.py weekly --source db
python main.py weekly 2026-W05 --source scan -i "补充"
python main.py weekly --source db --no-save

# 月报
python main.py monthly --source db
python main.py monthly 2026-01 --source scan -i "补充"

# 列表
python main.py list

# 测试
python -m pytest tests/ -v
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
├── core/
│   ├── config.py        # 单例配置 (Dynaconf)
│   ├── healthcheck.py   # CLI 环境检查
│   ├── llm.py           # DeepSeek/OpenAI 客户端 + JSON 校验重试
│   └── logger.py
├── models/
│   └── schemas.py       # Pydantic 模型
├── services/
│   ├── sqlite_store.py  # SQLite 存储实现（日/周/月）
│   ├── file_scanner.py
│   └── report_gen.py
└── utils/
    └── text_tools.py
```

## Key Patterns

- LLM 输出：使用 JSON 模式 + Pydantic 严格校验
- 扫描策略：`summary_mode` + `total_max_chars` 控制上下文长度
- 存储策略：SQLite 作为程序事实源，Markdown 作为阅读输出
- 周边界：ISO 周（Monday-Sunday）

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
- DeepSeek 和 OpenAI 均使用 Chat Completions API + JSON schema 输出
