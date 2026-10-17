# 日报可读性实现计划（子项目 B）

- 设计：[2026-10-18-readability-design.md](2026-10-18-readability-design.md)
- 日期：2026-10-18
- 关联：[当前状态](../current-state.md)、[定时链路可靠性实现计划](2026-10-17-scheduled-reliability-plan.md)

## 全局约束

- 只改本仓库；不推送 Git，不部署生产，不修改正式计划任务和正式配置（`config-rust`）。
- 不调用真实模型；不重跑历史日报；不修改 `data/` 下任何数据库与报告文件（只读分析除外）。
- 不改报告 schema 版本（3/4）、模型计划版本（4/5）、数据库结构、冻结输入结构。
- 保留审计要素：来源标签、引用标记、SHA-256、覆盖缺口、质量标记、`变化依据`（v4）。
- 模板编译字节与打包副本一致由 `operations.rs::TEMPLATES` 保证，不新增手工副本。
- 最终闸门：`cargo fmt --manifest-path rust/Cargo.toml --all -- --check`、`cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings`、`cargo test --manifest-path rust/Cargo.toml --workspace --locked` 全绿。

## 文件结构

```text
rust/report_app/src/reports.rs              evidence_text；渲染派生字段（统计、缺口分组、段落、目录）
rust/report_app/src/reports/data.rs         （不改结构；仅如需可加只读方法）
rust/report_app/tests/reports.rs            v3 渲染断言更新
rust/report_app/tests/templates.rs          转义单测更新
rust/report_app/tests/readability.rs        新增：v4 黄金渲染、缺口聚合、统计划、段落切分、目录、离线重渲染对照
rust/report_app/tests/fixtures/readability/ 新增：v4 输入 JSON 与黄金输出 MD
templates/evidence_report_template_v3.md    结构更新
templates/evidence_report_template_v4.md    结构更新（保留「变化依据」原样）
templates/evidence_compose_prompt_v4.md     提示词约束
templates/evidence_facts_prompt_v4.md       提示词约束
README.md / docs/daily-report-operations.md 文档更新
docs/plans/2026-10-18-readability-*.md      本设计/计划（状态与自检记录回填）
```

## 任务 1：`evidence_text` 转义最小化

**改动**：`reports.rs::evidence_text`。

规则：

1. 先做 `& < >` 实体化（保持现状）。
2. 再只对 `` \ ` * _ [ ] | `` 加反斜杠。
3. `#` 仅在文本行首（首字符，或换行折叠后等同于行首的位置不可发生）转义。
4. `+ ( ) { } !` 不转义。

**测试**（`tests/templates.rs` 或新文件）：

- `evidence_text("<tag> **重点**\r\n[x]")` == `"&lt;tag&gt; \\*\\*重点\\*\\*  \\[x\\]"`（既有断言保持等价）。
- `evidence_text("(新)")` == `"(新)"`；`evidence_text("+38909%")` == `"+38909%"`；`evidence_text("A#N/A")` == `"A#N/A"`；`evidence_text("#N/A")` == `"\\#N/A"`；`evidence_text("{x}!")` == `"{x}!"`。
- 换行仍折叠为空格；`&`/`<`/`>` 仍实体化。

**验收**：单测通过；`templates.rs::historical_templates_match_python_bytes` 仍通过（历史模板 fixture 不受影响）。

## 任务 2：渲染派生字段

**改动**：`reports.rs::render_daily`（在 `render` 前向 JSON 注入派生键，不改 `DailyReport` 字段与 `validate`）：

- `speed`：`{sources_total, sources_usable, sources_failed, facts_total, facts_confirmed, facts_pending, gaps_codes, gaps_total, quality}`。
- `gaps_grouped`：`[{code, message, labels: [], label_summary, count}]`，按 `code` 首次出现顺序；组内消息取首条；无来源条目 `labels: []`。
- `matters_view`：按现有顺序，每项附 `anchor`（`m1`、`m2`…）与 `paragraphs`（`background`/`details` 文本按句切分后数组）。切分规则：`>240` 字才切，在 `。`/`；` 后断，最多 3 段，向均衡长度靠拢；不增删字符。
- `toc`：`matters ≥ 10` 时生成 `[{anchor, title}]`，否则为空数组。
- 去重：同一事项内 `fact_ids` 与 `text` 完全相同的 `CitedText` 只保留一条（保持首次位置）。

**测试**：新 `tests/readability.rs` 用合成 `DailyReport` 覆盖：

- 统计三态（complete / partial / no_changes）。
- 同一 code 多来源合并为一行且来源汇总正确；无来源条目不合并。
- 547 字详情切成 2–3 段且拼接后与原文字符完全一致；≤240 字不切。
- 12 个事项生成目录与锚点；3 个事项不生成目录。
- 重复 detail 只出现一次。

**验收**：新测试通过，既有 `tests/reports.rs` 不受影响（或按新结构更新断言）。

## 任务 3：模板更新（v3、v4）

**v3/v4 共同结构**：

1. 标题下：如有 `speed`（`quality != no_changes`）输出速览行（材料/事实/缺口/质量）。
2. `## 内容概览` 不变（≤5 条）。
3. `## 事项目录`：仅当 `toc` 非空时输出，链接到 `#m<N>`。
4. `## 事项详情`：每个事项前 `<a id="m<N>"></a>`；`background`/`details` 输出 `paragraphs`（每段之间空行），段落尾部保留引用标签；`status` 行不变。
5. `## 待确认` 不变。
6. `## 来源与覆盖附录`：覆盖缺口改用 `gaps_grouped`（每行 `code` + 消息 + 来源汇总）。
7. v4 的 `## 变化依据` 一字不动。

**保持**：`no_changes` 与「暂不扩写」分支语义、`historical_reconstruction` 与 `partial` 提示、`missed_dates`、`user_notes`、生成时间行。

**测试**：

- 小型 v4 黄金文件（1–2 个事项、1 组缺口、1 条带 evidence 的事实、含转义字符）逐字节断言：`tests/fixtures/readability/v4-input.json` + `v4-expected.md`。
- 大型场景（12 个事项、长段、多来源缺口、重复详情）用属性断言（小节顺序、锚点数量、聚合行数、段落数、引用齐全），不写逐字节黄金文件。
- v3 黄金文件按同样方式新增或更新。

**验收**：黄金与属性测试通过；`templates.rs` 历史 fixture 测试不变。

## 任务 4：提示词约束

**改动**：`templates/evidence_compose_prompt_v4.md`、`templates/evidence_facts_prompt_v4.md`，按设计 B6 增补：

- compose：detail ≤3 句且 ≤200 字；每事项 ≤4 条 details；每段最多 3 个关键数字（不得改数字本身）；标题 8–24 字业务化、不用文件名；现状与建议方案同事项；技术性缺失合并待确认。
- facts：topic ≤24 字；detail ≤600 字；conditions ≤6 条。

**测试**：`operations.rs` 打包字节校验继续通过；单元/集成测试不依赖提示词全文比对（如有指纹断言按新字节更新，并在提交说明中写明）。

**验收**：全量测试通过；提示词文本人工复核与设计一致。

## 任务 5：渲染正文对照（合成 fixture，只读）

**测试**：测试不依赖本机数据库或 `data/`；从合成 fixture 复刻同样的密度场景（多个来源、同一缺口码、含 `+ ( ) [ #` 的文本），断言：

- 正文（附录之前）中 `\+`、`\(`、行中 `\#` 为 0，行首 `#` 仍被转义；`[集团]`、`(新)`、`+38909%` 原样保留。
- 同一缺口 code 只出现一次。
- 长段切分后文字拼接等于原文，引用只在最后一段。

**验收**：对照断言通过；不依赖本机 `data/`。

## 任务 6：文档

- `README.md`：日报呈现段落补充速览、目录、缺口聚合说明。
- `docs/daily-report-operations.md`：新增「报告结构」小节，说明可读性调整不改变审计要素。
- 本设计与计划：状态改为「已实现/已验收」，自检记录回填实际命令与结果。

**验收**：文档与实现一致；不夸大未验证内容。

## 最终验收

1. `cargo fmt --manifest-path rust/Cargo.toml --all -- --check` 通过（与既有代码一致，若仓库整体未格式化只能保证本次改动文件；以仓库现状为准）。
2. `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings` 通过。
3. `cargo test --manifest-path rust/Cargo.toml --workspace --locked` 全绿。
4. `git status` 只含本计划范围内的文件；不推送。

## 实现记录（2026-10-18）

- 渲染层：`reports.rs` 新增 `speed`、`gaps_grouped`、`toc`、`matters_view`、`overview_view`、`pending_view`；`evidence_text` 收敛为 `\` `` ` `` `*` `_` `|` 与行首 `#`。
- 模板：v3/v4 增加速览行、事项目录与锚点、段落切分渲染、缺口聚合；v4「变化依据」结构不变，仅消除无证据事实的多余空行。
- 测试：新增 `tests/readability.rs` 21 项；v4 黄金文件 `tests/fixtures/readability/v4-input.json` + `v4-expected.md`（LF 固定）；`templates.rs` 转义断言更新。
- 数据对照（2026-10-16 报告库 `raw_json`，只读统计）：146 个文本块的反斜杠转义 89 → 11；覆盖缺口 18 条 → 4 行；正文噪音 `\(`、`\+`、`[` 不再出现。
- 提示词：compose/facts v4 增补长度、数字密度、标题与技术性缺失归并约束；未新增硬校验。

## 自检记录

| 项目 | 结果 | 说明 |
| --- | --- | --- |
| 基线测试 | 通过 | 改动前 main 上 468 项通过（用户验收记录）；改动后全量 490 项列出，489 通过、0 失败、1 项按既有 `#[ignore]`（需 `AI_DAILY_TEST_BUNDLE`）跳过 |
| 转义单测 | 通过 | `tests/templates.rs` 覆盖 `(新)`、`+38909%`、`#N/A`、行首 `#`、`{x}!`、`[集团]`、换行与空串 |
| 黄金渲染 | 通过 | v4 输入→输出逐字节比对；无行尾空格、无连续空行、单结尾换行 |
| 缺口聚合 | 通过 | 同 code 合并、来源汇总、>6 来源缩写、无来源单独一行；真实样本 18→4 |
| 段落切分 | 通过 | >240 字切 2–3 段且拼接等于原文；≤240 字与无标点不切；引用只在末段 |
| 全量闸门 | 通过 | fmt/clippy/test 均 exit 0（2026-10-18 02:0x） |
| 遗留风险 | 记录 | 提示词未经真实模型联调；`变化依据` 体量待 10-18 首份 v4 日报测量；方括号不转义时 `[文字](链接)` 可能渲染为链接 |
