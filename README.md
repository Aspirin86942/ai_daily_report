# 审计日报生成器 v5.0

基于 LLM 的审计报告自动化生成工具。支持日报、周报、月报三种模式，扫描工作目录文件（Excel/PDF/PPTX/Word），结合用户口述，使用 Google Gemini API 生成结构化报告。

## 快速开始

```bash
# 1. 安装依赖
pip install -r requirements.txt

# 2. 配置 API Key
# 编辑 config/.secrets.toml 或设置环境变量 GOOGLE_API_KEY

# 3. 验证配置
python check_config.py

# 4. 生成日报
python main.py daily -i "今日完成XX审计"
```

## 使用方式

```bash
# 日报
python main.py daily                              # 交互模式
python main.py daily -i "今日工作内容"             # 命令行模式
python main.py daily --no-save -i "预览模式"       # 预览不保存
python main.py daily --date 2026-02-05 -i "..."   # 指定日期

# 周报
python main.py weekly --source db                  # 聚合本周日报
python main.py weekly 2026-W05 --source scan       # 扫描指定周文件
python main.py weekly --source db -i "补充说明"    # 附加补充

# 月报
python main.py monthly --source db                 # 聚合本月日报
python main.py monthly 2026-01 --source scan       # 扫描指定月文件

# 列出已有日报
python main.py list
```

### 数据来源 (`--source`)

- `db` — 从已保存的日报 JSON 聚合 (推荐，数据结构化)
- `scan` — 扫描工作目录文件 (适用于无历史日报的场景，使用 `summary_mode` 缩减上下文)

## 配置

### config/settings.toml
```toml
[paths]
work_dir = "D:\\01- 工作"  # 扫描的工作目录

[llm]
model_id = "gemini-2.5-flash"
temperature = 0.2

[scanner]
allowed_extensions = [".xlsx", ".xls", ".pptx", ".pdf", ".txt", ".md", ".docx"]
max_workers = 4
```

### config/.secrets.toml
```toml
[api]
google_api_key = "your-api-key"

[proxy]
http_proxy = "http://127.0.0.1:10808"
https_proxy = "http://127.0.0.1:10808"
```

## 项目结构

```
├── config/              # 配置文件
├── data/
│   ├── db/             # JSON 数据库 (日报/周报/月报)
│   └── reports/        # Markdown 报告 (日报/周报/月报)
├── src/
│   ├── core/           # 配置、日志、LLM 客户端
│   ├── models/         # Pydantic 数据模型
│   ├── services/       # 文件扫描、历史管理、报告生成
│   └── utils/          # 工具函数
├── templates/          # LLM Prompt + Jinja2 渲染模板
├── tests/              # 单元测试
└── main.py             # 主程序 (子命令 CLI)
```

## 核心特性

- **多模态报告**: 日报/周报/月报，统一 JSON 强制输出 + Pydantic 校验
- **双数据源**: `db` 聚合历史日报 / `scan` 扫描工作目录文件
- **并行处理**: ThreadPoolExecutor 并行扫描文件
- **Token 控制**: `summary_mode` 缩减解析限制 + `total_max_chars` 全局上限
- **双存储**: JSON（程序读取）+ Markdown（人类阅读）
- **ISO 周标准**: Monday-Sunday，`date.fromisocalendar()` 处理跨年

## 技术栈

Python 3.10+ | Google Gemini 2.5 | Pydantic | Dynaconf | Rich | Jinja2 | Pandas
