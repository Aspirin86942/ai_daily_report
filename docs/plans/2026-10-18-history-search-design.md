# 历史报告检索设计（子项目 F1）

- 状态：已实现，待用户验收（history-search 分支）
- 日期：2026-10-18
- 范围：报告库（日报/周报/月报）的只读全文检索；不含归档全文检索、模型问答、报告写路径改造
- 关联：[产品能力范围方案](2026-10-18-product-capabilities-options.md)、[当前状态](../current-state.md)、[日报使用与恢复](../daily-report-operations.md)

## 一、背景与目标

报告库已有 188 份日报（`daily_reports.raw_json` 合计约 5.8 MB）和少量周月报。现有 `list` 只能按日期枚举，无法回答“上次这个审批方案是怎么定的”。F1 提供离线、零模型、只读的历史检索：

1. 中文关键词检索报告正文，命中到具体事项而非整份报告。
2. 支持日期范围、报告类型、来源标签过滤。
3. 文本与 JSON 两种输出，JSON 供脚本或后续能力消费。
4. 不修改报告库、不影响日报发布与恢复路径。

## 二、非目标

- 不做归档库全文检索（归档正文在磁盘对象里，索引成本另算，列为后续增量）。
- 不做语义检索/向量/模型重排；不调用模型。
- 不提供写入、编辑、重新发布；检索只读。
- 不改变 `daily_reports`/`weekly_reports`/`monthly_reports` 表结构。

## 三、总体方案

新增只读检索模块与独立索引库：

- 索引库：`config.paths.db_dir/report_search.sqlite3`（与 `reports.sqlite3` 分开，避免改动报告库）。
- 数据来源：只读打开 `reports.sqlite3`，读取三张报告表的 `raw_json` 与标签字段。
- 索引结构：FTS5 虚表（`tokenize='trigram'`），每行对应一个“检索片段”（概览条目、事项、待确认、周/月报章节），带报告标签、类型、日期、标题、来源标签。
- 索引同步：`search` 默认检查索引新鲜度（源库行数与 `max(updated_at)`），过期则自动重建；`--reindex` 强制重建。全量重建数据量小（约 5.8 MB 正文、数千片段），可接受。
- 查询：≥3 个字符用 FTS5 `MATCH`（短语加引号转义），<3 个字符用 `LIKE` 回退（中文短词常见），结果统一排序、截断、输出。

选择 trigram 的原因：SQLite `unicode61` 不做中文分词，整句会变成单个 token；trigram 支持任意子串匹配，适合中文关键词。已核对 `libsqlite3-sys 0.38.2` 的 bundled SQLite 启用 `SQLITE_ENABLE_FTS5`，无需新增依赖。

## 四、详细设计

### F1-1 命令行

```text
ai-daily-report.exe search --query <关键词> [--from YYYY-MM-DD] [--to YYYY-MM-DD]
    [--kind daily|weekly|monthly|all] [--source S001] [--limit 20]
    [--format text|json] [--reindex]
```

- `--query` 与 `--reindex` 二选一（`--reindex` 可单独执行）。
- `--from`/`--to` 只作用于日报（周月报无自然日标签；带日期过滤时自动排除周月报）。
- `--source` 接受来源标签 `S001`（大小写不敏感），按片段来源标签过滤。
- 退出码：0 成功；1 运行错误（数据库缺失、索引失败、空查询）；参数用法错误沿用 CLI 既有约定（clap 退出码 2）。

### F1-2 索引库结构

```sql
CREATE TABLE index_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE VIRTUAL TABLE report_search USING fts5(
    report_key UNINDEXED,   -- daily:日期；weekly:week_label；monthly:year_month
    kind UNINDEXED,         -- daily|weekly|monthly
    report_label UNINDEXED, -- 展示用标签
    report_date UNINDEXED,  -- 日报为 YYYY-MM-DD，周月报为空
    title,                  -- 事项标题或章节标题
    body,                   -- 片段正文（含文件名/来源提示）
    sources UNINDEXED,      -- 空格分隔的 S001 S003
    tokenize='trigram'
);
```

`index_meta` 记录 `schema_version`、源库行数与 `max(updated_at)`（三表合并后的最大值），用于新鲜度判断。

### F1-3 片段抽取

- 日报：`content.overview[]`、`content.matters[]`（标题 + background/details/status，合并为一段）、`content.pending[]`；兼容 v2 的 `summary_items`/`verification_items`。没有 `content` 时用 `raw_json` 的正文文本字段兜底。
- 周报：`raw_json` 的七个章节字段（`completed_work` 等）各成片段。
- 月报：`raw_json` 的 `overview`/`completed_work`/`work_summary`/`next_plan`。
- 来源标签：按 `report.sources[]` 顺序生成 `S001`…，写入 `sources`。
- 空正文或坏 JSON：跳过该报告并计数，不影响其他报告；重建结果返回跳过数。

### F1-4 查询与输出

- 关键词：`chars().count() >= 3` 时 `MATCH` 短语（内部双引号转义），否则 `LIKE '%关键词%'`（转义 `%`/`_`）。
- 过滤：`kind`、`report_date` 范围、`sources LIKE`。
- 排序：先按报告日期倒序、再按片段出现顺序；`--limit` 截断（默认 20，上限 500）。
- 文本输出每命中一组：报告标签、类型、事项标题、带高亮标记的摘录、来源。
- JSON 输出：`{"query","total","results":[{"kind","report_label","report_date","title","snippet","sources"}]}`。
- 索引新鲜度过期时自动重建，并在 JSON 输出中带 `"index_rebuilt":true`。

### F1-5 边界与错误

| 场景 | 处理 |
| --- | --- |
| 报告库不存在 | `SEARCH_DB_MISSING`，提示先运行日报或检查配置 |
| 索引库写入失败 | `SEARCH_INDEX_ERROR`，不触碰报告库 |
| 关键词为空/仅空白 | clap 拒绝或 `SEARCH_QUERY_EMPTY` |
| 报告 JSON 损坏 | 跳过并计数，不中断 |
| 自动重建失败 | 返回错误并保留旧索引（下次再试） |
| 并发运行 | 索引库使用 busy timeout；重建在单事务内完成 |

## 五、测试与验收

1. 单元测试：片段抽取（日报 v3/v4、周报、月报、坏 JSON）；中文 2 字 LIKE 回退与 4 字 FTS 命中；`--from/--to/--kind/--source` 过滤；排序与 limit；JSON 结构。
2. 集成测试：临时目录建 `reports.sqlite3` 最小表与样例 `raw_json`，通过 `CARGO_BIN_EXE_ai-daily-report` 运行 `search` 命令验证文本/JSON 输出与退出码；重复运行验证索引复用与自动重建。
3. 只读验证：运行前后报告库文件大小与 `updated_at` 不变；索引库可删除重建。
4. 全量 `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked`。
5. 不调用模型、不联网、不改正式任务与配置。

## 六、风险与边界

| 风险 | 处理 |
| --- | --- |
| trigram 索引体积（约正文 1–2 倍） | 索引库独立，可删除重建；先按报告库规模测量 |
| 短中文词（<3 字）LIKE 扫描 | 片段数千行，可接受；超时上限与 limit 限制 |
| FTS5 在目标 SQLite 不可用 | 已核对 bundled 构建启用 FTS5；启动时检测，缺失则明确报错 |
| 索引与报告库不同步 | 每次查询前比较行数与 `max(updated_at)`，过期自动重建 |
| 周月报没有自然日标签 | 日期过滤时排除周月报；`--kind` 单独查询 |

已定决策：

- 索引库独立文件，报告库只读；不改报告写路径、不加触发器。
- 全量重建而非增量（数据量小，逻辑简单，避免一致性缺陷）。
- 不新增依赖；FTS5 使用 bundled SQLite。
