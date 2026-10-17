# 历史报告检索实现计划（子项目 F1）

- 设计：[2026-10-18-history-search-design.md](2026-10-18-history-search-design.md)
- 日期：2026-10-18
- 基线：`main` `540e5e7`（B/E 已合并）；本计划在 `history-search` 分支执行

## 全局约束

- 不推送 Git、不部署生产、不改正式任务与配置、不调用真实模型、不联网。
- 报告库只读；新增索引库 `report_db_dir/report_search.sqlite3`，可删除重建。
- 不新增第三方依赖；使用 rusqlite（bundled SQLite 已启用 FTS5）。
- 最终闸门：`cargo fmt --manifest-path rust/Cargo.toml --all -- --check`、`cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings`、`cargo test --manifest-path rust/Cargo.toml --workspace --locked`。

## 任务 1：命令行与分发

- `rust/report_app/src/cli.rs`：新增 `Search(SearchArgs)` 子命令：
  - `--query <String>`（与 `--reindex` 互斥且至少给一个）
  - `--from/--to`（`NaiveDate`，现有 `types::parse_date`）
  - `--kind`（`daily|weekly|monthly|all`，默认 `all`）
  - `--source <String>`（S001 形式）
  - `--limit`（默认 20，1..=500）
  - `--format`（`text|json`，默认 `text`）
  - `--reindex`（bool）
- `rust/report_app/src/main.rs`：加载配置后分发到 `search::run`（不打开 App/Store 写路径）。
- 帮助文本使用中文，与现有命令一致。

## 任务 2：索引库与重建

- 新模块 `rust/report_app/src/search.rs`（`lib.rs` 中 `pub mod search;`）。
- 索引库路径：`config.paths.db_dir.join("report_search.sqlite3")`。
- 打开报告库：`Connection::open_with_flags(... READ_ONLY)`，路径 `config.paths.db_dir.join("reports.sqlite3")`；不存在时报 `SEARCH_DB_MISSING`。
- 建表：`index_meta` + FTS5 虚表 `report_search`（列见设计 F1-2，`tokenize='trigram'`）。
- 新鲜度：读取三表行数与 `max(updated_at)`（列不存在时用行数即可），与 `index_meta` 比较；不同则重建。`--reindex` 强制。
- 重建：单事务 `DELETE FROM report_search` → 逐报告抽取片段 → 批量插入 → 写 meta；坏 JSON 跳过计数。
- 启动时检测 FTS5 可用性（`pragma compile_options` 或建表失败），不可用时明确报错。

## 任务 3：片段抽取

- 日报：优先解析 `raw_json.content`：
  - `overview[]`、`matters[]`（title + background/details/status 文本合并）、`pending[]`；
  - 兼容 v2：`summary_items`/`verification_items` 数组字符串；
  - 无 `content` 时从 `raw_json` 已知文本字段兜底（`work_summary` 等）。
- 周报：`completed_work`、`self_growth`、`improvement_actions`、`work_summary`、`next_plan`、`support_needed`、`other_notes`，每项一个片段。
- 月报：`overview`、`completed_work`、`work_summary`、`next_plan`。
- 来源标签：按 `raw_json.sources[]` 顺序生成 `S001`…；缺失时为空。
- 片段标题：日报事项用 `matter.title`，概览/待确认用固定前缀（如「概览」「待确认」）；周月报用章节名。
- 纯文本化：去掉换行、压缩空白；单片段长度上限 4000 字符（截断并标记）。

## 任务 4：查询、过滤与输出

- `query` 非空白；`chars().count() >= 3` 用 `MATCH '"<转义短语>"'`；否则 `LIKE '%<转义>%'`（转义 `\`、`%`、`_`）。
- 过滤：`kind`、`report_date` 范围（仅日报）、`sources LIKE '%S001%'`。
- 排序：`report_date DESC`（周月报按 `report_key` 倒序）→ 片段标题/顺序；`limit` 截断。
- 摘录：FTS 路径用 `snippet()`（`[`/`]` 标记），LIKE 路径手工截取命中前后各 40 字符。
- 文本输出格式（示例）：

```text
2026-10-16 · 日报 · 采购审批方案(新)调整
  …将现状与建议方案[集中]呈现…
  来源：S001
```

- JSON：`{"query":..., "total":N, "index_rebuilt":bool, "results":[{"kind","report_label","report_date","title","snippet","sources"}]}`
- `--reindex` 单独执行时输出重建摘要（JSON 或文本按 `--format`）。

## 任务 5：测试

- 新 `rust/report_app/tests/search.rs`：
  - 临时目录内手写最小 `reports.sqlite3`（`daily_reports`/`weekly_reports`/`monthly_reports` 表 + 样例 `raw_json`），调用 `search::run`，断言：
    - 中文 4 字词走 FTS 命中正确片段；2 字词走 LIKE 命中；
    - `--from/--to` 只过滤日报；`--kind weekly` 只返回周报；`--source S001` 过滤；
    - `limit` 与排序；JSON 结构；`--reindex` 重建并更新 meta；
    - 坏 JSON 被跳过且不中断；报告库缺失报 `SEARCH_DB_MISSING`。
  - 只读性：运行前后报告库文件字节不变；索引库删除后可重建。
- CLI 覆盖：用 `CARGO_BIN_EXE_ai-daily-report` 直接启动进程，验证 text/json 输出、退出码（运行错误 1、用法错误 2）与报告库字节不变；不新增测试依赖。
- 全量测试保持通过；不得依赖本机 `data/`。

## 任务 6：文档

- `README.md`：使用示例加一行 `search`，说明零模型、只读索引。
- `docs/daily-report-operations.md`：新增「历史检索」小节（命令、过滤、索引重建、只读边界）。
- `docs/plans/README.md`：加入本设计与计划的状态行。
- 完成后回填设计/计划状态与自检记录。

## 任务 7：验收

1. 全量 fmt/clippy/test 通过。
2. 在本机真实报告库上只读试运行（`--query 审批 --kind daily --limit 5`），确认命中合理、报告库未被修改（可用 `git status`/文件大小核对；不提交任何数据）。
3. 记录重建耗时与索引库大小。
4. 不合并、不推送，由主代理复核后合并。

## 自检记录

| 项目 | 结果 | 说明 |
| --- | --- | --- |
| 索引重建 | 通过 | 真实报告库副本：190 份报告（188 日报 + 1 周报 + 1 月报）、3,392 个片段、跳过 64 份无正文报告；重建 1.56 秒，索引库 6,946,816 字节；缓存命中检索 25 毫秒 |
| 中文检索 | 通过 | 4 字词 `供应商承认` 命中 2026-10-16 事项与待确认片段；2 字词 `审批` 走 LIKE 回退；单元测试覆盖 FTS/LIKE 两条路径、转义与短词 |
| 只读性 | 通过 | 真实库副本与单元测试均核对：运行前后 `reports.sqlite3` 长度与 SHA-256 不变；索引库独立可删除重建 |
| 过滤与排序 | 通过 | `--from/--to` 仅日报、`--kind` 周/月报、`--source S001`、`limit`、日期倒序均有测试 |
| 全量闸门 | 通过 | fmt 0、clippy `-D warnings` 0、全量测试 504 通过 / 0 失败 / 1 项既有 `#[ignore]` 跳过（55 组） |
| 退出码 | 已调整 | 参数用法错误沿用 clap 退出码 2（移除按子命令扫描 argv 的特殊判断）；运行错误 1 |
| 遗留风险 | 记录 | 新鲜度若行数与最大更新时间都不变则需 `--reindex`；单片段超 4,000 字截断；短词 LIKE 扫描 3,392 行可接受；待用户验收后再合并 |
