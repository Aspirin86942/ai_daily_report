# Scanner 预算感知解析 + 缓存策略 + PDF 分类 设计规格（修订版 v3）

> 状态：**Needs revision**（2026-08-08 两轮设计审查后；第一轮六阻断已回应，第二轮六阻断已按规范闭合，待复评）
> 日期：2026-08-08
> 决策范围：Rust scanner core 的准入计划、语义预算与安全 deadline、缓存策略、快照、PDF 分类、worker 会话、profile/schema 版本演进
> 首要目标：以真实目录证据驱动，消除「解析大量文件却只进少量上下文」的浪费，把 no-text PDF 从昂贵提取路径剔除，并保证**缓存无关确定性**
> 不涉及：ContextEnvelope 字段形状、报告 schema/模板、LLM 行为、office backend 迁移、OCR、daemon/USN

## 0. 审查与修订记录

| 轮 | 阻断/高项 | 本版处置 |
|---|---|---|
| R1-1 | 温扫 ≤150ms 与 discovery=272ms 矛盾 | 改为受 discovery 下限约束的目标（≤300ms 或相对比例，见 Part 6 实测后定） |
| R1-2 | 呈现顺序/缓存状态影响 context | **确定性准入计划**（nominal_priority + ContextBudgetModel，见 Part 1） |
| R1-3 | not_parsed+omit 与契约不兼容 | **完整状态矩阵**写入 spec（见 Part 2） |
| R1-4 | worker/cache 无法表达 PDF 分类 | 分类状态独立 + 类型化分类缓存（见 Part 3） |
| R1-5 | 快照键不足/复用旧 summary | 完整 provenance 键 + **context_artifacts 关系模型**（见 Part 5） |
| R1-6 | 批量 worker 无法逐文件超时 | **流式 session 契约 ai_daily_python_session_v1**（见 Part 7） |
| R2-1 | wall-clock deadline 与缓存无关确定性冲突 | **语义 quota 与安全 deadline 分离**（见 Part 1） |
| R2-2 | 准入顺序/预算估算无唯一事实源 | **nominal_priority + 唯一 ContextBudgetModel**（见 Part 1） |
| R2-3 | Timeout 无 decision 语义 | **完整 ParseStatus 状态矩阵**（见 Part 2） |
| R2-4 | 快照引用旧 context 无关系模型 | **context_artifacts / context_runs 表关系 + inspect-run 规则**（见 Part 5） |
| R2-5 | 冻结字段形状与新增 profile 字段冲突 | **scanner profile v2 演进，ContextEnvelope v1 冻结**（见 Part 8） |
| R2-6 | 流式 worker 版本留实施时决定 | **明确 ai_daily_python_session_v1，Office worker 不升版**（见 Part 7） |
| R2-7 | PDF 分类失败策略不确定 | **分类状态机 + 数值门禁**（见 Part 3） |
| R2-8 | 性能门禁可「少做工作」作弊 | **手工 acceptance 反作弊条件**（见 Part 9） |
| R2-9 | GC/备份/发布锁无执行入口 | **GC 入口 + 备份流程 + requirements.lock 溯源**（见 Part 4/8/10） |

## Problem Statement

### 真实语料与实测证据（2026-08-08，`D:\01- 工作`）

| 窗口 | 可解析文件 | office/PDF 类 | 解读 |
|---|---:|---:|---|
| 日报 (2d) | 33 | 32 | 冷扫秒级 |
| 周报 (7d) | 136 | 95 | 冷扫 3.7s |
| 月报 (30d) | 322 | 267 | 冷扫约 1 分钟量级 |
| 90d | 729 | 516 | 冷扫卡死 >5min，PDF 273 |
| 全年 | 1667 | 1565 | 冷扫预计 10min+ |

PDF 类型分布（pypdfium2 快速分类，1184 个）：30d 窗口 no-text 占 **78%**、90d 占 **75%**、全年 **779/1184（66%）**。

7 天窗口冷/温实测：冷扫 3735ms（parse 3387ms=91%，进上下文 14/136=10%）；温扫 347ms（discovery 272ms=78%）。单文件 parse：PDF ~2543ms、xlsx ~14ms、docx ~12ms、pptx ~18ms。

### 契约现实（已核实）

- 呈现顺序由 `decide_files` 决定，且优先级**依赖解析结果**（Error/Timeout/NotParsed → priority 80，`decision.rs:87`）——无法在解析前复制。
- 非 Success 一律转 action=Error；NotParsed 必须携带 Diagnostic（`decision.rs:56-60`）；计入 error_count（`store/mod.rs:1555`、`context_audit.rs:401`）。
- `ActiveRun.context_run_id()` 就是当前 `scan_run_id()`（`store/mod.rs:285-287`）——不能直接返回旧 context ID。
- `context_profile_hash` 仅含 `{engine_build, context}`（`context_audit.rs:413`）。
- `metadata_only` section 含多行字段（`compressor.rs:189`），205 个约 300 字符/section ≈ 撑满月报 60k 预算（`config.rs:67-70`）。
- `WorkerBackend` 仅 5 字面量（`scanner_contract.py:667`）；`WORKER_CONTRACT_VERSION` 为共享常量。
- scanner profile 为严格 v1；`requirements.lock` 由已删除的 `requirements.txt` 生成（`requirements.lock:1-2`）。

### 目标与成功标准

1. 90d 冷扫「卡死 >5min」→ 有界 ≤40s（语义 quota 保证质量 + 安全 deadline 防卡死）。
2. 月报 30d 冷扫 ~1min → 有界 ≤20s。
3. **缓存无关确定性**：空/部分/全缓存、且 worker/source/parser 一致且无安全 deadline 触发时，final_context、decisions、summary 语义字段一致。
4. no-text PDF 提取调用 0 次（`pdfplumber_invocations=0`），单文件 ~2.5s → ~0.3s。
5. 性能门禁反作弊：只有语义 quota 导致的 NotParsed，无安全 deadline 触发。
6. 温扫无变化：跳过 parse/cache/context/finalize 内容写放大（大窗口实测对比）；小窗口目标按三段实测后定为 ≤300ms 或相对比例。
7. fixed-corpus 门禁全绿；真实目录为**手工 acceptance**（非 CI 硬回归）。

## Solution

### 语义 quota 与运行安全 deadline 分离（贯穿全局）

**语义 quota（确定性）**——决定正常 NotParsed/Omit 集合，进入 profile、golden output、快照键：
- 文件数上限、PDF 提取数上限、总 inspected pages 上限、准入预测预算。
- 在同一输入 + profile 下结果确定，与 CPU 负载/缓存状态无关。

**运行安全 deadline（非确定性）**——只防卡死，不参与正常决策：
- wall-clock 总时限。触发后本轮为 **Partial 或明确可重试失败**，**不生成可复用快照**，**不作为缓存无关确定性的正常结果**。
- 审计标记 `stage_deadline_exhausted`（warning/error 语义按阶段定）。

三态缓存一致性门禁**限定作用域**：仅当 worker build、source version、parser 结果一致，且无安全 deadline 触发、无进程崩溃、无瞬时 I/O 错误时成立。

### Part 1：确定性准入计划（唯一事实源）

**nominal_priority（与解析结果无关）**：
```text
nominal_priority = path category（.pytest_cache/.logs/.data 等）+ extension category（office/pdf/text/other）
```
- 准入顺序与最终呈现顺序**都固定使用 nominal_priority**。
- 解析失败只改变 action/reason，**不再改变顺序**（否则 fresh parse 失败与 cache hit 成功会产生不同全局顺序）。

**ContextBudgetModel（纯内部 seam，准入与 Compressor 共用）**：
```text
reserved_chars(file) >= rendered_chars(file)
sum(reserved) + fixed_sections + footer_reserve <= global_max_chars
```
- `reserved_chars(file)` 覆盖真实 section 全部成本：路径与 Markdown 标题、action/reason/backend/lane、input/output chars、代码围栏与换行、**metadata section 多行**（不假设为零）、以及全局摘要/提示/解析问题 footer 与 omitted summary 保留空间。
- 准入在解析前用 `reserved_chars` 计算，冻结 admitted 集合。
- **不变量**：准入后的成功文件若被 Compressor 因全局预算改 Omit，视为 `BUDGET_MODEL_MISMATCH` 内部错误（panic/error 路径），**不是允许存在的正常分支**。
- 因保守预留导致的少量未准入是确定性与质量的取舍，接受并记录。

**准入执行**：`BudgetedContextScheduler` 按 nominal_priority 顺序执行 admitted 文件的解析；cache hit 复用、miss 执行。同一目录+profile 三态缓存一致性由此成立（admitted 集合只依赖 discovery+profile+分类）。

### Part 2：ParseStatus → action 完整状态矩阵

规范性表格（实施不得偏离）：

| ParseStatus | Diagnostic | Context action | summary 计数 | 快照资格 |
|---|---|---|---|---|
| Success | 无 | keep / compress / metadata_only | success_count | 合格 |
| Error | 必须 | error | error_file_count | 不合格 |
| Timeout | 必须 | error | timeout_count | 不合格 |
| NotParsed | 无 error diagnostic，必须携带 budget_reason | omit | 派生 not_parsed_count | 合格（语义 quota 导致） |

计数等式（contract）：
```text
decision_error_count = error_file_count + timeout_count
not_parsed_count = source_file_count - success_count - timeout_count - error_file_count
```
- `has_error` 仅当 `parse_status == Error`；Timeout 独立走 error+timeout_count，**不得落入 Keep/Compress**。
- NotParsed 允许无 Diagnostic，用 `budget_reason`（`global_budget_exceeded` / `semantic_quota_exhausted`）。
- **删除**「成功解析后因全局预算而 Omit」的正常兼容分支（正确准入下不应出现；出现即 `BUDGET_MODEL_MISMATCH`，见 Part 1）。
- 同步改动：`decision.rs`、`ContextFileEvidence::validate`、`context_audit.rs` extension metrics（NotParsed 不再计 error）、`store/mod.rs` 计数校验。
- 渲染：省略摘要 `input_chars` 用 size_bytes 近似，标注 `~`。

### Part 3：PDF 分类状态机（独立于 Context action）

分类状态：`text_in_parse_window | no_text_in_parse_window | not_classified_by_budget | unknown | error`（只在 `pdf_max_pages` 窗口内判定，不称 image_only）。

| 情况 | 分类结果 | 是否缓存 | 后续 |
|---|---|---|---|
| 解析窗口内发现有效文字 | text_in_parse_window | 是 | 进入确定性提取准入 |
| 完整检查窗口仍无文字 | no_text_in_parse_window | 是 | metadata，`pdfplumber_invocations=0` |
| 加密/确定性损坏 | error | 否或短期负缓存 | Diagnostic |
| 分类器超时/崩溃/瞬时 I/O | unknown | 否 | 当前 run Partial，不生成快照 |
| inspected-pages 语义 quota 耗尽 | not_classified_by_budget | 否 | 按确定性 quota 处理 |

类型化分类缓存：classifier profile hash、分类状态、page count、inspected pages、classifier build、source version。

**数值门禁（独立于提取替换门禁）**：
- parse 窗口内 text 的 false-negative 必须为 **0**。
- `no_text_in_parse_window` 误判上限（语料上 ≤ 0.1%）。
- unknown rate 与 failure rate 上限。
- 文字只在 max_pages 之后时，期望状态为 `no_text_in_parse_window`（不是全文件 no_text）。

分类阈值：固定常量（默认）。若要按 profile 调整，走 scanner profile v2 正式演进（见 Part 8），不静默扩展 wire JSON。

### Part 4：成本感知缓存 + 有界淘汰 + GC

- **昂贵结果始终缓存**：PDF 提取内容、PDF 分类状态、office 结果。
- **cheap 缓存有界淘汰**：byte cap、age、source/profile generation；**不因单次 Context omit 删除**。
- `last_accessed` 采用**时间桶更新**（同一行一天最多更新一次），避免 cache hit 引入写放大。
- **GC 执行入口**（无 daemon）：finalize 后**按毫秒预算做 opportunistic GC**；另提供显式 `maintenance` 子命令做物理压缩与深度清理。
- **物理压缩**：SQLite `DELETE` 只增 freelist；`VACUUM`/`incremental_vacuum` 作为迁移事务后**独立、可失败**维护步骤。
- 「快照命中」与「parse-cache 全命中」是两个不同指标，分开度量。

### Part 5：快照快速路径（关系模型 + provenance）

**表关系**（最小可落地）：

```text
context_artifacts
  artifact_id          PK
  final_context
  context_sha256
  semantic counts / content metadata

context_runs
  context_run_id       PK            # 当前 run 自己的 ID（= 当前 scan_run_id）
  artifact_id          FK            # 可与旧 run 共享
  reused_from_context_run_id         # provenance（可空）
  当前 run 的 timings / status
```

**审计行处理（明确选择）**：`inventory / file_results / decisions` **复制到当前 run**（审计兼容、inspect-run 语义不变、级联删除不变）；`final_context` 通过 `artifact_id` 引用**不重复存储**。O(N) 写放大的残余通过 Part 4 GC/物理压缩缓解；引用式审计集列为后续优化，不在本版。

**inspect-run 规则**：`inspect-run(scan_run_id)` 返回当前 run 的 rows + `reused_from_context_run_id` 指向的 artifact 内容；`finalize` 校验当前 run 的 rows 与当前 summary 一致（`store/mod.rs:1500` 要求保留）。

**快照键**（完整 provenance）：
`canonical logical request（去 request_id）+ 有序 discovery 结果 + 归一化 discovery issues + scanner profile v2 + report_mode + engine build + route-stack worker builds + classifier build/profile + 语义 quota 值`。

**worker 身份**：**保留 live worker handshake**（成本小、保留 worker 可用性语义）；快照只跳过 parse/cache/context/finalize 内容写。跳过 handshake 需 release manifest 身份，标为后续。

**快照资格**：仅无安全 deadline、无 unknown 分类、无 transient error、worker/provenance 完整的确定性终态可作 snapshot source。

### Part 6：温扫目标与三段实测

- 先**冻结 handshake / DB / envelope 三段实测**（当前 warm 347ms = handshake ~40ms + discovery 272ms + cache/context/finalize ~35ms）。
- 再决定小窗口目标：绝对 ≤300ms 或相对改善比例（以实测为准）。
- 大窗口：跳过 parse/cache/context/finalize 内容写的收益用实测对比记录。

### Part 7：长驻流式 PDF worker session（明确契约版本）

- **版本决策（不留给实施）**：
  - 现有单文件 `version`/`parse` 与 `ai_daily_worker_v1` 保持不变。
  - Python document worker 新增独立命令 `session`，独立契约 `ai_daily_python_session_v1`。
  - Worker handshake 新增显式 capability；scanner 仅在 capability 存在时用 session，否则走 v1 one-shot。
  - **Rust Office worker 不升版**。
- **session 不变量**：
  - 一条 NDJSON request ↔ 一条 response，按 request ID 配对。
  - 每个 session 同时最多一个 in-flight request。
  - 每条 frame、stdout、stderr、解析正文都有独立上限。
  - 每文件继续 source-version 前后校验。
  - 超时杀整个 Job Object 并重启 session；**不得对同一超时文件无限 fallback**。
  - 参数：`session_concurrency`、`max_requests_per_session`（替代 batch_size）、idle TTL、内存/RSS 回收策略。
  - v1 one-shot 保留为 fallback。

### Part 8：scanner profile v2 + schema foundation + 备份

- **profile 版本（明确选择）**：冻结 `ContextEnvelope` 输出 v1；正式演进 `scanner_profile_request_v2` / `normalized_scanner_profile_v2`，承载语义 quota（`max_expensive_extractions`、`max_total_inspected_pages`、classifier policy/version、安全 deadline 配置）。这些参与 cache/snapshot identity。
- **schema foundation 一次性完成**：准入/分类/快照/缓存/GC 所需全部表与列一次迁移，不在多个 Task 中逐步改 `user_version`。
- **备份/回滚（明确选择）**：升级前用 **SQLite online backup**（或独占 scanner lease 后 checkpoint+备份），**必须含 WAL**；备份验证 `PRAGMA integrity_check` + 文件哈希 + 恢复烟测。回滚会丢失升级后新增 run 的数据窗口，需在发布门禁声明。旧 release 不可直接读升级后 DB（`TooNew`）。
- 分类缓存、快照指纹、语义 quota、deadline 字段全部纳入 foundation。

### Part 9：性能门禁（反作弊）与真实目录手工 acceptance

**性能通过条件**：
- 运行安全 wall-clock guard **未触发**（`stage_deadline_exhausted_count == 0`）。
- 只允许**确定性语义 quota** 导致的 NotParsed。
- golden corpus 的 included/omitted/classified 集合完全符合预期。
- text-bearing PDF 的最低覆盖数量/优先级覆盖率达标。
- 性能报告记录：profile hash、worker builds、缓存状态、admitted/extracted/classified 数量。

**真实目录手工 acceptance（非 CI）**：
- 固定 start/end date + profile；使用隔离 scan DB。
- 明确 cold 定义（清 parse/classification/snapshot 缓存 vs 只清 DB）。
- ≥3 次运行，报告中位数与最大值。
- 记录 CPU、磁盘、git commit、worker builds。
- 证据只保存聚合指标，**不提交真实路径/文件名/正文**。

### Part 10：依赖与发布

- pypdfium2 一旦生产 import 即声明为**直接依赖**（`uv add`）。
- **requirements.lock 溯源**：现有 lock 由已删的 `requirements.txt` 生成，需改为从 `pyproject.toml` 导出的发布投影，确定命令（如 `uv lock && uv export --format requirements-txt --output-file requirements.lock --no-hashes` 或等价），并验证 Windows CI/部署与 worker build fingerprint 一致。

### 保持冻结（不修改）

- `ContextEnvelope` 字段形状（输出 v1）。
- 报告 schema、模板内容、LLM 调用次数与 provider 行为。
- office backend 与 fallback policy。
- scanner 权限边界、Python 侧 `report_runner`/`context_scheduler`/`rust_context_client` 接口。
- **OCR / USN / daemon**：明确出范围。

## Testing

### BudgetedContextScheduler interface 测试
- 三态缓存一致性（限定作用域：worker/source/parser 一致且无安全 deadline）。
- 语义 quota：被排出的文件 `not_parsed` + `budget_reason`，included 集合确定。
- 安全 deadline：触发 → Partial/可重试失败，不生成快照，`stage_deadline_exhausted` 可审计。
- `BUDGET_MODEL_MISMATCH`：准入后 Compressor 再改 Omit 触发内部错误。
- 完整状态矩阵：Error/Timeout/NotParsed/Success 各 action、计数、Diagnostic 约束。
- 分类状态机：六种情况（text/no-text/error/unknown/quota）各自缓存与后续行为。
- 快照：同目录两次 run → 引用 artifact + `reused_from_context_run_id`、当前 run 重算耗时、summary reconcile；任一变更不命中。
- 缓存：cheap 有界淘汰、昂贵必缓存、GC/物理压缩独立可失败。

### 门禁
- fixed-corpus cold/warm 门禁全绿。
- PDF 分类门禁（数值）+ 提取替换门禁（独立）。
- 真实目录手工 acceptance（Part 9 反作弊条件）。

### 验证命令（Windows）

```bash
uv run pytest
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
uv run python main.py doctor --strict
git diff --check
```

## Acceptance Criteria

- [ ] 90d 冷扫有界 ≤40s 且 `stage_deadline_exhausted_count == 0`；30d ≤20s。
- [ ] 三态缓存一致性（限定作用域）成立；admitted 集合只依赖 discovery+profile+分类。
- [ ] 完整状态矩阵生效：Error/Timeout/NotParsed/Success 的 action、Diagnostic、计数与快照资格符合 Part 2 表。
- [ ] `decision_error_count = error_file_count + timeout_count`；`not_parsed_count` 派生；extension metrics 不再把 NotParsed 计为 error。
- [ ] `BUDGET_MODEL_MISMATCH` 为内部错误路径（非正常分支）。
- [ ] PDF 分类：ground truth 无文字 PDF 分类正确、`pdfplumber_invocations=0`；数值门禁（text false-negative=0 等）通过；unknown/error 不进缓存。
- [ ] 快照：context_artifacts/context_runs 关系落地，inspect-run 语义不变，当前 run 重算耗时且 summary reconcile。
- [ ] 流式 session（ai_daily_python_session_v1）：逐请求 deadline、杀会话重启、v1 fallback、无无限 fallback。
- [ ] scanner profile v2 落地、ContextEnvelope v1 冻结；语义 quota 进 profile/快照键。
- [ ] schema foundation 一次性完成；备份含 WAL + integrity_check + 恢复烟测；旧 release 不可直接读升级后 DB 已声明。
- [ ] 缓存：cheap 有界淘汰、昂贵必缓存、时间桶 last_accessed、GC/物理压缩独立可失败。
- [ ] pypdfium2 直接依赖 + requirements.lock 溯源确定。
- [ ] fixed-corpus 全绿；Rust workspace 测试、pytest 全绿、doctor --strict 通过。

## Out of Scope

1. `ContextEnvelope` 字段形状变更。
2. 报告 schema、模板、LLM prompt 与 provider 行为变更。
3. office backend 或 office fallback policy 变更。
4. scanner 权限边界、Python 侧报告接口变更。
5. **OCR**：no-text PDF 内容提取不在本 spec。
6. **USN Journal / 持久目录监视器**：快照前置变化检测不在本 spec。
7. **daemon**：GC 用 finalize 后机会式 + `maintenance` 子命令，不引入常驻进程。
8. 跨载体报告事务、Web/GUI/queue。
9. 引用式审计集（快照不复制 rows 的变体）——列为本版后优化。

## Implementation Decisions

- **ID-1 语义 quota 与安全 deadline 分离**：quota 决定正常集合（确定性、进 profile/指纹），deadline 只防卡死（Partial/可重试，不生成快照）。
- **ID-2 nominal_priority 与解析结果无关**：准入与呈现顺序固定；解析失败只改 action/reason。
- **ID-3 唯一 ContextBudgetModel**：`reserved_chars >= rendered_chars`；`BUDGET_MODEL_MISMATCH` 为内部错误。
- **ID-4 完整状态矩阵**：Error/Timeout/NotParsed/Success 各自 action/Diagnostic/计数/快照资格，计数等式为 contract。
- **ID-5 分类状态机**：text/no-text/error/unknown/quota 六态，各自缓存与后续；数值门禁。
- **ID-6 缓存与呈现解耦 + 有界淘汰 + GC**：昂贵必缓存、cheap 淘汰、时间桶 last_accessed、opportunistic GC + maintenance。
- **ID-7 快照关系模型**：context_artifacts/context_runs，审计行复制、正文引用、当前 run 重算耗时。
- **ID-8 流式 session 契约**：ai_daily_python_session_v1，Office 不升版，v1 fallback。
- **ID-9 scanner profile v2**：ContextEnvelope v1 冻结，profile v2 承载 quota/deadline/classifier。
- **ID-10 schema foundation 一次性 + 备份回滚**：online backup 含 WAL + integrity + 恢复烟测。

## 建议实施顺序

1. 写规范到代码与测试：状态矩阵、计数等式、nominal_priority、ContextBudgetModel、quota/deadline 分离。
2. 一次性 schema/worker contract foundation + profile v2 + 生产 DB 备份与回滚策略 + requirements.lock 溯源。
3. pypdfium2 直接依赖 + 单文件 PDF 分类器 + 独立数值门禁。
4. BudgetedContextScheduler：三态缓存一致性 + 语义 quota + 安全 deadline + BUDGET_MODEL_MISMATCH 测试。
5. 成本感知缓存 + 有界淘汰 + GC + 物理压缩。
6. 快照：context_artifacts/context_runs + 完整 provenance + 三段实测定目标。
7. 流式 session（ai_daily_python_session_v1）；门禁存档后条件性替换 PDF 提取器。
8. 批量 SQL 与 discovery 微调（最后，按 profile 再做；SQL 分块/临时表，保持 cache miss reason 优先级）。

## 主要风险与缓解

| 风险 | 缓解 |
|---|---|
| quota 保守导致少装文件 | reserved_chars 全成本建模；确定性优先，接受少量未准入 |
| deadline 与确定性冲突 | quota/deadline 分离；deadline 触发不进正常结果/快照 |
| 状态矩阵改动破坏消费方 | 计数等式与矩阵进 spec；error_file_count 不再含 not_parsed 需在消费方同步 |
| 分类误伤稀疏文本 | 数值门禁（false-negative=0）+ 六态状态机 |
| 快照引用旧 context 模型不清 | context_artifacts/context_runs 关系 + inspect-run 规则写入 spec |
| 流式 session 泄漏/崩溃 | Job Object 超时杀 + v1 fallback + 无无限 fallback |
| 性能门禁被「少做工作」作弊 | stage_deadline_exhausted_count==0 + golden 集合精确 + 覆盖门槛 |
| profile v2 与冻结冲突 | ContextEnvelope v1 冻结，profile v2 正式演进 |
| 备份漏 WAL | online backup/checkpoint+备份，integrity+hash+恢复烟测 |
| 锁溯源断 | requirements.lock 改为 pyproject.toml 导出投影并验证 |

## 决策记录

- 2026-08-08 用户授权：重开 scanner DB schema / cache identity 冻结边界，做 v2 迁移。
- 2026-08-08 用户确认：未解析文件 input_chars 用 size 近似。
- 2026-08-08 用户确认：no-text PDF 暂不做 OCR。
- 2026-08-08 两轮设计审查后：第一轮六阻断已回应，第二轮六阻断已按本版规范闭合；状态保持 Needs revision，待复评。
