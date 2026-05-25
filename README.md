# 审计日报生成器 v5.0

基于 LLM 的审计报告自动化工具。支持日报、周报、月报三种模式，支持扫描工作目录文件（Excel/PDF/PPTX/Word/TXT/Markdown），并结合用户输入生成结构化报告。

## 快速开始

```bash
# 1) 安装依赖
pip install -r requirements.txt

# 2) 选择 LLM provider 并配置对应密钥
#
# 默认 provider 是 DeepSeek：
# - Linux 保持 config/settings.linux.yaml 中 llm.provider = "deepseek"
# - Windows 保持 config/settings.windows.yaml 中 llm.provider = "deepseek"
# - 配置 DEEPSEEK_API_KEY（或写入 config/.secrets.yaml）
#
# 如果使用 OpenAI：
# - 先把本机 settings.*.yaml 中 llm.provider 改为 "openai"
# - 再配置 OPENAI_API_KEY（或写入 config/.secrets.yaml）

# 3) 检查环境与配置
python main.py doctor

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

# 检查环境与配置
python main.py doctor

# 测试
python -m pytest tests/ -v
```

## 数据来源（`--source`）

- `db`: 从 SQLite 历史库聚合（推荐）
- `scan`: 直接扫描工作目录文件

## 升级说明

- 当前版本只支持现行 SQLite schema。若沿用旧版 `data/db/reports.sqlite3` 并触发 schema 过期错误，请先备份数据库，再按当前结构重建。
- 当前版本不再提供旧 JSON 历史数据到 SQLite 的自动迁移。若历史数据仍停留在旧 JSON 载体中，`weekly --source db` 和 `monthly --source db` 不会自动读取这部分内容。

## 配置说明

### 本机配置文件

运行时按系统自动读取本机配置：

- Linux: `config/settings.linux.yaml`
- Windows: `config/settings.windows.yaml`

这两个文件是本机配置，已加入 `.gitignore`，不会提交到 GitHub。仓库只提交 `config/settings.example.yaml` 作为示例。首次配置可复制示例文件：

```bash
# Linux
cp config/settings.example.yaml config/settings.linux.yaml

# Windows 用户可复制成 config/settings.windows.yaml 后修改路径
```

```yaml
paths:
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
  settings.linux.yaml       # Linux 本机配置（不提交）
  settings.windows.yaml     # Windows 本机配置（不提交）
  .secrets.yaml             # 本机敏感配置（不提交）

data/
  db/                  # SQLite 数据库目录（reports.sqlite3）
  reports/             # Markdown 报告输出（日/周/月）

src/
  core/                # 配置、环境检查、日志、LLM 客户端
  models/              # Pydantic 数据模型
  services/            # 文件扫描、SQLite 存储、报告生成
  utils/               # 工具函数

templates/             # Prompt + Jinja2 模板
tests/                 # pytest 测试
main.py                # CLI 入口
```

## 技术栈

Python 3.10+ | DeepSeek/OpenAI | SQLite | Pydantic | Dynaconf | Jinja2 | Pandas
