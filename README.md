# 审计日报生成器 v5.0

基于 LLM 的审计报告自动化工具。支持日报、周报、月报三种模式，支持扫描工作目录文件（Excel/PDF/PPTX/Word/TXT/Markdown），并结合用户输入生成结构化报告。

## 快速开始

```bash
# 1) 安装依赖
pip install -r requirements.txt

# 2) 选择 LLM provider 并配置对应密钥
#
# 默认 provider 是 DeepSeek：
# - 保持 config/settings.toml 中 llm.provider = "deepseek"
# - 配置 DEEPSEEK_API_KEY（或写入 config/.secrets.toml）
#
# 如果使用 OpenAI：
# - 先把 config/settings.toml 中 llm.provider 改为 "openai"
# - 再配置 OPENAI_API_KEY（或写入 config/.secrets.toml）

# 3) 检查配置
python check_config.py

# 4) 生成日报
python main.py daily -i "今日完成XX审计"
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

# 测试
python -m pytest tests/ -v
```

## 数据来源（`--source`）

- `db`: 从 SQLite 历史库聚合（推荐）
- `scan`: 直接扫描工作目录文件

## 配置说明

### `config/settings.toml`

```toml
[paths]
work_dir = "D:\\01- 工作"
data_dir = "data"
reports_dir = "data/reports"
db_dir = "data/db"

[llm]
provider = "deepseek"            # deepseek | openai
model_id = "deepseek-chat"       # OpenAI 示例: gpt-4o-mini
temperature = 0.2
max_tokens = 8192
max_retries = 3
```

- 默认使用 `deepseek`，因此只配置 `OPENAI_API_KEY` 但不切换 `llm.provider` 时，`python check_config.py` 仍会按 DeepSeek 路径校验并报缺少 `DEEPSEEK_API_KEY`。
- OpenAI 用户请先把 `llm.provider` 改成 `openai`，再配置 `OPENAI_API_KEY`。

### `config/.secrets.toml`

```toml
[api]
deepseek_api_key = "your-deepseek-key"
openai_api_key = "your-openai-key"

[proxy]
http_proxy = "http://127.0.0.1:10808"
https_proxy = "http://127.0.0.1:10808"
```

## 存储说明

- 当前默认历史存储：`data/db/reports.sqlite3`
- Markdown 报告输出：`data/reports/`

## 项目结构

```text
config/                # 配置文件
  settings.toml
  .secrets.toml

data/
  db/                  # SQLite 数据库目录（reports.sqlite3）
  reports/             # Markdown 报告输出（日/周/月）

src/
  core/                # 配置、日志、LLM 客户端
  models/              # Pydantic 数据模型
  services/            # 文件扫描、SQLite 存储、报告生成
  utils/               # 工具函数

templates/             # Prompt + Jinja2 模板
tests/                 # pytest 测试
main.py                # CLI 入口
```

## 技术栈

Python 3.10+ | DeepSeek/OpenAI | SQLite | Pydantic | Dynaconf | Jinja2 | Pandas
