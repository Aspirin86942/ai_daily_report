# CLAUDE.md

## Project Overview

审计报告生成器 v5.0 — 基于 LLM 的审计报告自动化生成工具。支持日报、周报、月报三种模式，每种模式支持 `db` (聚合历史日报) 和 `scan` (扫描工作目录文件) 两种数据来源。使用 Google Gemini API 生成结构化报告，JSON 强制输出 + Pydantic 校验。

## Commands

```bash
# 安装依赖
pip install -r requirements.txt

# 配置检查
python check_config.py

# 日报
python main.py daily                           # 交互模式
python main.py daily -i "今日工作内容"          # 命令行模式
python main.py daily --no-save -i "预览模式"    # 预览不保存
python main.py daily --date 2026-02-05 -i "..."  # 指定日期

# 周报
python main.py weekly --source db               # 聚合本周日报
python main.py weekly 2026-W05 --source scan -i "补充"  # 扫描文件
python main.py weekly --source db --no-save     # 预览不保存

# 月报
python main.py monthly --source db              # 聚合本月日报
python main.py monthly 2026-01 --source scan -i "补充"  # 扫描文件

# 列表
python main.py list

# 测试
pytest tests/ -v
```

## Architecture

```
日报:
  用户输入 + 文件扫描 + 昨日计划
      ↓
  LLMClient.generate_report() → JSON
      ↓
  DailyReportData (Pydantic)
      ↓
  ├─→ data/db/YYYY-MM-DD.json
  └─→ data/reports/YYYY-MM/YYYY-MM-DD.md

周报 (source=db):
  history_mgr.get_week_reports() → [DailyReportData] + missing_days
      ↓
  LLMClient.generate_weekly_report() → JSON
      ↓
  WeeklyReportData (Pydantic)
      ↓
  ├─→ data/db/weekly/YYYY-Wnn.json
  └─→ data/reports/weekly/YYYY-Wnn.md

周报 (source=scan):
  scanner.scan_files(monday, sunday, summary_mode=True) → ScanResult
      ↓
  LLMClient.generate_weekly_report() → JSON
      ↓
  (同上存储)

月报: 同周报模式，存储路径为 data/db/monthly/ 和 data/reports/monthly/
```

## Project Structure

```
src/
├── core/
│   ├── config.py        # 单例配置 (Dynaconf)
│   ├── llm.py           # Gemini API 客户端 (_call_llm_with_json 公共重试)
│   └── logger.py
├── models/
│   └── schemas.py       # Pydantic 模型 (Daily/Weekly/Monthly + 枚举)
├── services/
│   ├── file_scanner.py  # 文件扫描 (scan_files + summary_mode + total_max_chars)
│   ├── history_mgr.py   # JSON 数据库读写 (日/周/月)
│   └── report_gen.py    # Jinja2 渲染 (日/周/月)
└── utils/
    └── text_tools.py    # 文本工具 (parse_week_label, get_month_date_range)

config/
├── settings.toml        # 路径、LLM、扫描器配置 (含 summary_* 限制)
└── .secrets.toml        # API Key、代理 (不提交 Git)

templates/
├── system_prompt.md     # 日报 LLM Prompt
├── report_template.md   # 日报 Jinja2 模板
├── weekly_prompt.md     # 周报 LLM Prompt
├── monthly_prompt.md    # 月报 LLM Prompt
├── weekly_template.md   # 周报 Jinja2 模板
└── monthly_template.md  # 月报 Jinja2 模板

tests/
├── test_schemas.py      # 数据模型测试
├── test_text_tools.py   # 工具函数测试
├── test_file_scanner.py # 文件扫描器测试
├── test_history_mgr.py  # 历史管理器测试
└── test_report_gen.py   # 报告生成器测试
```

## Key Patterns

- **LLM**: `response_mime_type="application/json"` 强制 JSON，Pydantic 校验，`_call_llm_with_json()` 公共重试逻辑
- **文件扫描**: `scan_files(start_date, end_date, summary_mode)` 通用接口，ThreadPoolExecutor 并行，Pandas 矢量化处理 Excel
- **Token 控制**: `summary_mode=True` 时使用缩减限制 (excel_max_rows=10, pdf_max_pages=2)，`total_max_chars=50000` 全局上限
- **存储**: JSON (程序读取) + Markdown (人类阅读) 双轨制，日/周/月各有独立目录
- **周边界**: ISO 标准 Monday-Sunday，`date.fromisocalendar()` 处理跨年

## Modifying LLM Output

修改日报输出：
1. `templates/system_prompt.md` - Prompt
2. `src/models/schemas.py` - `DailyReportData`
3. `templates/report_template.md` - 渲染模板

修改周报输出：
1. `templates/weekly_prompt.md` - Prompt
2. `src/models/schemas.py` - `WeeklyReportData`
3. `templates/weekly_template.md` - 渲染模板

修改月报输出：
1. `templates/monthly_prompt.md` - Prompt
2. `src/models/schemas.py` - `MonthlyReportData`
3. `templates/monthly_template.md` - 渲染模板

## Adding File Types

在 `src/services/file_scanner.py`:
1. `_extract_content()` 添加分发
2. 实现 `_parse_xxx()` 方法
3. `config/settings.toml` 添加扩展名
