# 报告文本化简化设计

## 背景

当前项目的日报、周报、月报都采用较强的结构化模型，尤其强调分类、状态、量化指标、完成率、风险等级等字段。这套设计更适合输入事实非常完整、可稳定量化的场景。

本项目的实际使用方式并不满足这个前提。当前主要依赖磁盘扫描结果和简短人工补充来生成报告，扫描上下文通常只能提供大致线索，无法支撑稳定、可信的数量描述、完成率或风险分级。继续要求模型输出这类字段，会制造“看起来精确、实际没有依据”的伪信息。

用户希望把三类报告统一调整为更自然的文本表达方式。日报只保留三类主体内容：

1. 今日工作完成内容
2. 今日工作小结
3. 明日工作计划

周报和月报允许比日报多一层汇总，但整体仍保持段落式、弱结构、少推断的风格。

## 目标

- 去掉无可靠依据的量化描述要求。
- 去掉过度结构化的列表、分类、状态、完成率、风险等级等字段。
- 将日报调整为纯段落文本的三段式结构。
- 将周报和月报调整为“总览 + 三段主体”的纯段落文本结构。
- 保持 `daily`、`weekly`、`monthly`、`list` 命令仍然可用。
- 保持 `weekly/monthly --source db|scan` 的双来源模式仍然可用。
- 让周报和月报继续能够基于日报历史聚合，但聚合对象改为文本内容而非细粒度量化字段。

## 非目标

- 不保留与旧 SQLite 结构的兼容层。
- 不为旧 JSON 或旧 SQLite 历史数据提供自动迁移。
- 不保留旧的 `risks`、`statistics`、`category_summaries`、`quantitative` 等结构。
- 不新增可视化界面、导出格式或额外命令。

## 核心设计

### 1. 日报结构

日报模型收缩为四个字段：

- `date`
- `completed_work`
- `work_summary`
- `next_plan`

字段语义如下：

- `completed_work`：今日工作完成内容，使用一段或多段自然语言描述，不分条、不编号，不要求分类、状态或量化。
- `work_summary`：今日工作小结，用一段较完整的总结性文字概括推进情况、重点判断或整体进展。
- `next_plan`：明日工作计划，用一段或多段自然语言描述，不分条、不编号。

不再输出以下字段：

- `achievements`
- `risks`
- `yesterday_review`

昨日计划仍可作为生成日报时的参考输入，但不再单独渲染为一个输出字段。

### 2. 周报结构

周报模型调整为六个字段：

- `week_label`
- `date_range`
- `overview`
- `completed_work`
- `work_summary`
- `next_plan`

字段语义如下：

- `overview`：本周总览，作为高于三段主体的一层汇总，用于概括本周整体推进方向和主要关注点。
- `completed_work`：本周完成内容，纯段落文本。
- `work_summary`：本周工作小结，纯段落文本。
- `next_plan`：下周工作计划，纯段落文本。

`data_source` 不再作为报告模型输出字段，也不在 Markdown 中展示。若运行时仍需要知道来源，应作为内部流程参数使用，而不是报告内容的一部分。

### 3. 月报结构

月报模型调整为五个字段：

- `year_month`
- `overview`
- `completed_work`
- `work_summary`
- `next_plan`

字段语义与周报保持一致，只是时间粒度从周切换为月。

不再保留以下字段：

- `category_summaries`
- `risks`
- `statistics`
- `key_achievements`
- `next_month_plans`
- `missing_days`
- `data_source`

## 生成逻辑

### 日报生成

日报仍基于三类输入：

- 用户口述
- 文件扫描结果
- 昨日计划

但生成目标只保留三段主体文本：

- `completed_work`
- `work_summary`
- `next_plan`

Prompt 需要明确要求模型：

- 避免无依据的量化和统计性表述。
- 只有在用户输入或扫描内容中明确出现具体数字时，才允许保留数字。
- 输出必须是自然段文本，而不是项目符号或编号列表。
- 不单独输出“昨日计划完成情况”。

### 周报和月报生成

#### `db` 模式

周报和月报在 `db` 模式下，不再依赖日报中的列表项、分类项或量化字段做聚合。新的聚合方式改为：

1. 从 SQLite 读取对应周期内的日报记录。
2. 提取每份日报中的 `completed_work`、`work_summary`、`next_plan` 文本。
3. 将这些文本整理为时间顺序上下文。
4. 交给 LLM 生成对应周期的 `overview`、`completed_work`、`work_summary`、`next_plan`。

#### `scan` 模式

`scan` 模式继续使用文件扫描结果作为主要上下文来源，但输出结构与 `db` 模式保持一致，即只生成纯段落文本字段，不再生成伪量化结构。

## Markdown 模板设计

日报 Markdown 调整为以下四段：

- 标题
- 今日工作完成内容
- 今日工作小结
- 明日工作计划

周报 Markdown 调整为以下五段：

- 标题
- 本周总览
- 本周完成内容
- 本周工作小结
- 下周工作计划

月报 Markdown 调整为以下五段：

- 标题
- 本月总览
- 本月完成内容
- 本月工作小结
- 下月工作计划

所有模板统一要求：

- 使用自然段。
- 不使用编号列表。
- 不使用项目符号列表。
- 不展示内部流程字段。

## SQLite 与数据持久化

本次变更采用直接切换到新结构的方式，不保留旧结构兼容层。

新的日报存储字段：

- `date`
- `completed_work`
- `work_summary`
- `next_plan`
- `raw_json`
- `created_at`
- `updated_at`

新的周报存储字段：

- `week_label`
- `date_range`
- `overview`
- `completed_work`
- `work_summary`
- `next_plan`
- `raw_json`
- `created_at`
- `updated_at`

新的月报存储字段：

- `year_month`
- `overview`
- `completed_work`
- `work_summary`
- `next_plan`
- `raw_json`
- `created_at`
- `updated_at`

`next_plan` 也保存为单个文本字段，而不是 JSON 数组。原因是用户已经明确不希望保留列表型结构，数据库结构应与最终输出风格保持一致。

## 历史数据策略

本次采用“重建数据库，不保留旧数据”的策略。

具体含义：

- 不为旧 SQLite 结构写兼容读取逻辑。
- 不编写旧结构到新结构的自动迁移脚本。
- 实施时直接重建 `data/db/reports.sqlite3`。
- 旧历史数据如需保留，由用户自行备份原数据库文件，不纳入本次改造范围。

这样做的原因是：

- 当前项目是个人工具，兼容层价值低。
- 保留旧结构会迫使代码继续维护无用字段。
- 从旧结构自动拼接新文本也只能得到质量有限的历史结果，收益不高。

## 受影响文件

本次实现预计至少涉及以下文件：

- `src/models/schemas.py`
- `src/core/llm.py`
- `src/services/sqlite_store.py`
- `src/services/history_mgr.py`
- `src/services/report_gen.py`
- `templates/system_prompt.md`
- `templates/weekly_prompt.md`
- `templates/monthly_prompt.md`
- `templates/report_template.md`
- `templates/weekly_template.md`
- `templates/monthly_template.md`
- `tests/test_schemas.py`
- `tests/test_sqlite_store.py`
- `tests/test_history_mgr.py`
- `tests/test_report_gen.py`

如果现有代码中有基于旧字段的汇总逻辑或断言，也需要一并收缩到新模型。

## 错误处理与边界

- 如果 `db` 模式下某个周期内没有日报，应继续明确报错，而不是生成空报告。
- 如果 `scan` 模式下没有扫描到有效内容，应继续生成“无文件证据”类上下文提示，但最终输出仍然必须符合新 schema。
- 若 LLM 返回旧字段或列表结构，应以新的 Pydantic schema 校验失败并重试。
- 若数据库文件不存在或表结构不匹配，应在重建流程中显式初始化新 schema，而不是尝试兼容旧表。

## 测试策略

重点测试从“字段细节正确”转向“文本型模型和渲染稳定”。

至少覆盖以下内容：

- `DailyReportData`、`WeeklyReportData`、`MonthlyReportData` 新 schema 的校验。
- SQLite 读写是否与新文本字段一致。
- 周报和月报在 `db` 模式下是否能够正确读取日报文本并传递到汇总流程。
- Markdown 模板渲染是否输出正确章节标题和段落文本。
- 旧列表字段相关测试全部删除或改写，避免继续锁定旧结构。

## 方案取舍

本设计最终采用“弱结构文本化”的中间方案，而不是只改模板或完全自由文本：

- 不选“只改模板”的原因：底层旧 schema 仍会持续制造不必要的量化输出。
- 不选“完全自由文本”的原因：周报和月报仍然需要稳定字段，才能维持聚合流程与持久化。
- 选择当前方案的原因：字段数量少、表达自然，同时保留足够稳定的模型边界，便于后续维护。

## 验收标准

- 日报最终只展示三段主体内容，不再展示列表、量化指标、风险、昨日计划完成情况。
- 周报和月报最终只展示“总览 + 三段主体”。
- Prompt 中不再要求模型输出量化、完成率、风险等级等伪精确信息。
- 周报和月报在 `db` 与 `scan` 模式下都能生成符合新 schema 的报告。
- SQLite 仅保存新结构字段。
- 实施时直接重建数据库，不保留旧数据兼容逻辑。
