# 周报七段式模板改造设计

Status: DRAFT
Mode: Builder

## 1. 问题陈述

当前周报链路只支持 4 个正文段：

- `overview`
- `completed_work`
- `work_summary`
- `next_plan`

而本轮需求要求把周报统一改为 7 个固定段落：

1. 本周主要工作完成情况
2. 自我成长
3. 有待改善的地方及相关措施
4. 本周工作小结
5. 下周主要工作目标及计划
6. 需要的协助与支持
7. 其他

如果只改最终 Markdown 模板，不改数据结构和生成链路，会出现两个问题：

1. 模板标题和实际字段不一致，生成内容会错位。
2. SQLite 存储和 LLM 输出 schema 仍然是旧结构，后续维护会持续混乱。

因此这次改造必须按全链路处理，而不是停留在展示层。

## 2. 已确认需求

- 采用严格 7 段模型，而不是旧 4 段字段的展示层拼接。
- 周报的 LLM 输出 schema、Prompt、Markdown 模板、SQLite 周报存储需要一起调整。
- 旧周报数据需要可读，但不能臆造不存在的历史内容。
- 周报正文仍保持自然段表达，不使用项目符号、编号列表或表格。

## 3. 目标与非目标

### 3.1 目标

- 把周报正式升级为 7 个固定段落的数据契约。
- 让生成结果、存储结果和最终 Markdown 展示保持一致。
- 对历史旧周报提供兼容读取能力。
- 用测试覆盖 schema、Prompt、渲染和 SQLite 存取。

### 3.2 非目标

- 不修改日报 schema。
- 不修改月报 schema。
- 不对历史旧周报补写虚构的成长、支持或建议内容。
- 不在本轮重做周报 CLI 交互流程。

## 4. 推荐方案

采用严格 7 段模型加周报表迁移。

理由如下：

- 数据契约最清晰。后续任何人看 `WeeklyReportData` 或 `weekly_reports` 表结构，都能直接对应最终周报版式。
- 兼容策略可控。旧数据通过映射继续可读，但不会继续污染新结构。
- 测试边界明确。字段、模板、存储、生成可以一次性对齐。

## 5. 设计细节

### 5.1 周报数据契约

周报字段调整为：

- `week_label`
- `date_range`
- `completed_work`
- `self_growth`
- `improvement_actions`
- `work_summary`
- `next_plan`
- `support_needed`
- `other_notes`

字段语义对应如下：

- `completed_work`: 对应“本周主要工作完成情况”，强调例行与专项推进、关键数据和进度情况。
- `self_growth`: 对应“自我成长”，强调本周工作中的成长、收获和领悟。
- `improvement_actions`: 对应“有待改善的地方及相关措施”，同时包含问题与改进动作。
- `work_summary`: 对应“本周工作小结”，强调整体回顾、分析和评价。
- `next_plan`: 对应“下周主要工作目标及计划”，强调关键目标和任务安排。
- `support_needed`: 对应“需要的协助与支持”，说明困难、依赖和外部支持诉求。
- `other_notes`: 对应“其他”，用于建议或补充说明。

这里删除旧 `overview` 字段，因为它与 7 段式结构中的任何一个标题都不是一一对应关系，继续保留只会制造歧义。

### 5.2 LLM 输出与 Prompt

`templates/weekly_prompt.md` 需要同步更新：

- 只允许输出新的 7 个正文段字段。
- 明确要求所有字段使用自然段，不要项目符号、编号列表或表格。
- 对每个字段分别给出生成指导，尤其是：
  - `completed_work` 要体现例行及专项、关键数据和进度情况。
  - `self_growth` 要聚焦成长和领悟，不写空泛口号。
  - `improvement_actions` 要体现问题和对应措施，不只写问题。
  - `support_needed` 没有明确信息时允许审慎表述为当前暂无明确协助需求。
  - `other_notes` 没有补充时允许写简短保守说明。

### 5.3 Markdown 模板

`templates/weekly_template.md` 需要按以下顺序输出：

1. 本周主要工作完成情况（例行及专项，体现关键数据及进度情况）
2. 自我成长（结合本周工作开展，收获了什么成长和领悟）
3. 有待改善的地方及相关措施
4. 本周工作小结（整体回顾分析本周的工作，直面问题进行思考，有评有论）
5. 下周主要工作目标及计划（提炼关键目标，做好任务计划管理）
6. 需要的协助与支持（针对工作过程中出现的困难等）
7. 其他（建议等）

页头继续保留 `week_label` 和 `date_range`，页尾继续保留生成时间。

### 5.4 SQLite 存储升级

`weekly_reports` 表需要增加以下列：

- `self_growth`
- `improvement_actions`
- `support_needed`
- `other_notes`

并移除旧 `overview` 列的依赖。

考虑到 SQLite 不适合在当前实现里做复杂在线迁移，本轮采取以下策略：

1. 新建或缺失列时执行兼容性升级。
2. 读写以 `raw_json` 为主，结构化列用于检索和人工检查。
3. 如果发现现有 `weekly_reports` 仍是旧列结构，则执行表重建迁移：
   - 创建新表结构
   - 复制旧数据
   - 通过旧 `raw_json` 做字段映射
   - 替换原表

这样可以避免旧数据库在新模型下直接报错。

### 5.5 旧周报兼容映射

当读取到旧版 4 段式周报 JSON 时，按以下规则兼容：

- `completed_work = overview + 两个换行 + completed_work`
- `self_growth = ""`
- `improvement_actions = ""`
- `work_summary = work_summary`
- `next_plan = next_plan`
- `support_needed = ""`
- `other_notes = ""`

兼容原则：

- 只迁移已有文本，不臆造新内容。
- 把旧 `overview` 合并到“本周主要工作完成情况”，因为旧总览通常承载整体推进和重点背景，放在这里最接近用户目标。
- 历史空白字段允许为空字符串。

## 6. 测试与验收

至少覆盖以下测试面：

- `tests/test_schemas.py`
  - 新 `WeeklyReportData` 可正常实例化
  - 旧未知字段仍会被拒绝

- `tests/test_report_gen.py`
  - 周报 Markdown 渲染包含 7 个新标题
  - Prompt 测试断言包含新字段语义

- `tests/test_sqlite_store.py`
  - 新 `weekly_reports` 表结构断言更新
  - 周报保存和读取走新字段
  - 旧 JSON 兼容读取可工作

- `tests/test_main.py`
  - 周报命令生成路径适配新结构

验收标准：

- 周报生成成功时，LLM 输出、Pydantic 模型、SQLite 存储和 Markdown 模板字段完全一致。
- 历史旧周报不会因缺少新字段而崩溃。
- 新模板输出顺序与用户提供的 7 段格式一致。

## 7. 风险点与边界条件

- 历史数据库里如果存在非常旧且损坏的 `raw_json`，兼容读取可能仍失败；这类情况只记录日志，不猜测内容。
- 新 Prompt 增加字段后，模型更容易输出空泛文字，需要测试确保仍保持客观、简洁。
- `support_needed` 和 `other_notes` 在输入不足时容易被模型编造，因此 Prompt 必须显式禁止臆造。
- 旧 `overview` 合并到 `completed_work` 后，历史周报第一段可能比新生成周报更长，这是可接受的兼容代价。

## 8. 伪代码草案

```python
# 目标：
# - 把周报从旧 4 段升级为新 7 段
# - 保证新旧周报都可读取
# - 保证生成、存储、渲染使用同一套字段
#
# 输入：
# - weekly_prompt_template: 周报 Prompt 模板
# - weekly_template: 周报 Markdown 模板
# - weekly_raw_json: SQLite 中存储的周报 JSON
# - report_context: 聚合后的日报上下文和文件证据
#
# 输出：
# - weekly_report: 新版 WeeklyReportData
# - markdown_text: 七段式周报 Markdown
# - persisted_row: 存入 SQLite 的新结构记录

def build_weekly_report(report_context, weekly_prompt_template, llm_client):
    # 1. 用新版 schema 约束模型输出，避免模板已经升级但数据还是旧字段
    prompt = render_weekly_prompt(
        template=weekly_prompt_template,
        context=report_context,
        schema=WeeklyReportData.model_json_schema(),
    )

    # 2. 让 LLM 直接按 7 段生成，避免后处理阶段再去猜字段归属
    weekly_report = llm_client.generate(prompt, response_model=WeeklyReportData)
    return weekly_report


def load_weekly_report(raw_json):
    parsed = json.loads(raw_json)

    # 3. 先尝试按新结构读取，这是当前主路径
    if looks_like_new_weekly_schema(parsed):
        return WeeklyReportData(**parsed)

    # 4. 如果还是旧结构，只迁移已有内容，不补造成长、协助支持或其他建议
    if looks_like_legacy_weekly_schema(parsed):
        merged_completed = join_non_empty(
            parsed.get("overview", ""),
            parsed.get("completed_work", ""),
            separator="\n\n",
        )
        return WeeklyReportData(
            week_label=parsed["week_label"],
            date_range=parsed["date_range"],
            completed_work=merged_completed,
            self_growth="",
            improvement_actions="",
            work_summary=parsed.get("work_summary", ""),
            next_plan=parsed.get("next_plan", ""),
            support_needed="",
            other_notes="",
        )

    # 5. 未知结构直接失败并记录日志，避免静默吞掉脏数据
    raise WeeklyReportParseError("Unsupported weekly report schema")


def migrate_weekly_reports_table(conn):
    # 6. 如果表结构还是旧版，就重建 weekly_reports
    if weekly_table_is_legacy(conn):
        create_weekly_reports_new_table(conn)

        for row in read_all_legacy_weekly_rows(conn):
            # 为什么这里按 raw_json 迁移：
            # 因为 raw_json 才是最完整的历史载体，直接拼列容易丢上下文
            weekly_report = load_weekly_report(row.raw_json)
            insert_new_weekly_row(conn, weekly_report)

        replace_legacy_weekly_table(conn)


def render_weekly_markdown(weekly_report, template):
    # 7. 模板只消费新版 7 段字段，避免展示层继续背负兼容逻辑
    return template.render(report=weekly_report)
```
