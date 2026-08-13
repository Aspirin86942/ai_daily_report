# report-run 行为冻结矩阵（迁移前）

| mode | source | scanner 调用 | LLM 方法 | render | 保存顺序 | 失败退出码 |
|---|---|---|---|---|---|---|
| daily | 固定 scan | `build_context(daily, scan, as_of_date-1, as_of_date)` | `generate_report` | `render_markdown` | `save_report` → `save_markdown` | 1 |
| weekly | db | 0 | `generate_weekly_report` | `render_weekly_markdown` | `save_weekly_report` → `save_weekly_markdown` | 1 |
| weekly | scan | 1 | `generate_weekly_report` | `render_weekly_markdown` | `save_weekly_report` → `save_weekly_markdown` | 1 |
| monthly | db | 0 | `generate_monthly_report` | `render_monthly_markdown` | `save_monthly_report` → `save_monthly_markdown` | 1 |
| monthly | scan | 1 | `generate_monthly_report` | `render_monthly_markdown` | `save_monthly_report` → `save_monthly_markdown` | 1 |

固定规则：

- daily 无输入时报错并返回失败；`--no-save` 不写报告 SQLite 或 Markdown；`--date` 只覆盖最终报告日期，不改变 scan window。
- weekly/monthly 的 `source=db` 没有日报时返回失败，并提示“未找到…日报数据”。
- scan `partial` 按原顺序显示 warning 后继续生成；scan `error` 在构造 LLM client 前终止。
- weekly/monthly 的补充输入按 `\n\n---\n\n用户补充: <input>` 格式追加到 file context。
- render 在发布之前；保存时先写报告 SQLite，再写 Markdown。
- 退出码：成功 0、预期失败 1、`KeyboardInterrupt` 130。

冻结证据：迁移前运行 `uv run pytest`，结果为 `237 passed, 1 skipped`。
