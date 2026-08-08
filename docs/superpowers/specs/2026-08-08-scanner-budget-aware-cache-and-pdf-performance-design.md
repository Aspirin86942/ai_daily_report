# Scanner 预算感知解析 + 缓存策略 + PDF 分类 设计规格（修订版 v4）

> 状态：**Needs revision**（2026-08-08 第三轮复评后修订；实现语义已收口，但同口径 wall-clock baseline 与独立复评尚未完成，暂不恢复 Ready for implementation）
> 日期：2026-08-08
> 决策范围：Rust scanner core 的准入计划、语义预算与安全 deadline、缓存策略、快照、PDF 分类、worker 会话、profile/schema 版本演进
> 首要目标：以真实目录证据驱动，消除「解析大量文件却只进少量上下文」的浪费，把 no-text PDF 从昂贵提取路径剔除，并保证**缓存无关确定性**
> 不涉及：ContextEnvelope 字段形状、报告 schema/模板、LLM 行为、office backend 迁移、OCR、daemon/USN

## 0. 审查与修订记录

| 轮 | 阻断/高项 | 本版处置 |
|---|---|---|
| R1-1 | 温扫 ≤150ms 与 discovery=272ms 矛盾 | 冻结为 7d snapshot warm median ≤330ms/max ≤400ms（见 Part 6） |
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
| R2-9 | GC/发布锁无执行入口 | **GC 入口 + requirements.lock 溯源**（见 Part 4/10） |
| R3-1 | quota 在 cache hit、并发完成顺序下仍可能漂移 | **两阶段不可变计划 + nominal charge**（见 Part 1） |
| R3-2 | deadline 的 Partial/Error、未启动文件和 cache write 未冻结 | **按触发阶段冻结终态与提交规则**（见 Part 2） |
| R3-3 | 现有 planner reject 与 PDF 分类状态未映射到完整矩阵 | **source disposition + classifier + ParseStatus 三表合一**（见 Part 2/3） |
| R3-4 | v1 handshake 不变却新增 capability，严格契约冲突 | **保留 version v1，新增 Python-only session-version/session v1**（见 Part 7） |
| R3-5 | artifact 仍会被 final_envelope_json 重复，复制 rows 会伪造当前耗时 | **envelope metadata 重建 + artifact 语义行 + 当前执行行分离**（见 Part 5） |
| R3-6 | Scheduler interface 与 ContextBudgetModel 所有权不清 | **恢复单一深 module interface，Compressor/预算模型内聚**（见 Solution） |
| R3-7 | 昂贵结果“始终缓存”导致总库仍无界 | **始终尝试写入、全类型硬上限、引用保护与有界 GC**（见 Part 4） |
| R3-8 | 分类/性能/发布投影门禁仍有未定值 | **冻结判定函数、样本下限、统计口径与唯一导出命令**（见 Part 3/9/10） |
| R3-9 | cache mutation 等到 finalize 才写，与“失败后保留 cache”冲突 | **inventory 前置 upsert + receipt 型独立 cache transaction + terminal COMMIT 线性化**（见 Solution/Part 2） |
| R3-10 | omitted group/count 成本非逐文件可加，原 delta 公式可能越界 | **独立 20% reservation + worst-case detail slots + catch-all**（见 Part 1） |
| R3-11 | classifier result provenance 与当前执行页数混淆，no-text backend/lane 未冻结 | **result/run/nominal 三分 + parser/classifier audit 分离**（见 Part 3/5） |
| R3-12 | warning metadata 可爆量，审计须先于 apply | **open_for_upgrade 审计先行 + 257 条 bounded projection**（见 Part 2/8） |
| R3-13 | “pdf_max_pages 默认 5”破坏 weekly/monthly 现有默认 2 | **保留 daily=5、weekly/monthly=2；真实目录以 summary_pdf_max_pages=5 显式 override**（见 Part 8/9） |
| R3-14 | PDF 五态仍未冻结 page/cache/pages/attempt 的逐态约束 | **新增 PdfClassificationAuditV1 完整字段矩阵**（见 Part 3） |
| R3-15 | 347ms 旧值来自 engine summary，不能直接证明新 harness wall-clock 的 330ms | **标注口径断点 + 先做 timer-only 可比基线门禁，阈值不自适应放宽**（见 Part 6） |
| R3-16 | 固定 2,000ms reserve 容易被误读为百万行终态写入 SLA | **明确为调度尾部保护；冻结事务内工作、极端配置语义与适用性能规模**（见 Solution/Part 6/8） |
| R3-17 | Version v2、maintenance 与新 Python operation Diagnostic 仍可由实现者自选字段 | **冻结严格字段、状态不变量与旧 WorkerDiagnosticV1 转换 seam**（见 Part 4/5/7） |
| R3-18 | migrated v1 run 缺 attempt/transport/classification/final diagnostic，无法诚实填充 FileAuditV2 | **保留 v1 replay；v2 inspect 对 legacy provenance 明确 fail closed**（见 Part 5/8） |
| R3-19 | max_candidate_files 不限制 discovery 总数，可能超过 1,000,000-row Inspect/终态上限 | **冻结 engine-owned source-file ceiling，超限在 inventory 前 fail closed**（见 Part 2/5/8） |
| R3-20 | classifier 使用 unicodedata，但 build identity 未含 Python/Unicode DB 版本 | **纳入 classifier build 与 classifier-version，阻止跨运行时误命中**（见 Part 7） |
| R3-21 | 现有 DB 未启用 auto_vacuum，incremental_vacuum 可能成功但无效果 | **新库预设 INCREMENTAL；升级库迁移后转换，未转换时明确 mode unavailable**（见 Part 4/8） |
| R3-22 | maintenance Diagnostic 无合法 stage，aggregate warning 自身字段未冻结 | **scanner-side 新增 maintenance stage；aggregate 固定 internal/any-retryable/null provenance**（见 Part 2/4/8） |
| R3-23 | cache miss reason 要求保留优先级但未定义，classification miss 无原因字段 | **新增分类 miss reason，并冻结两类 cache 的唯一判定树**（见 Part 3/4/5） |
| R3-24 | work deadline 与 absolute deadline 混称，SQLite COMMIT 超界后无法撤销 | **冻结双 deadline 术语与 cache/terminal 两个线性化点**（见 Solution/Part 2） |
| R3-25 | FileAuditV2 与 execution_metrics 只列字段，仍可出现不同统计口径 | **补齐 parse provenance 矩阵、类型/nullability 与逐项计数定义**（见 Part 5） |
| R3-26 | benchmark 私有“禁用 snapshot”开关可能进入生产或成为作弊入口 | **改为可校验的 cache-only seed DB 克隆，不给 scanner 增加旁路**（见 Part 6） |
| R3-27 | outcome 已形成但 finalization 预校验失败时，PreOutcomeFailure 无法表达 | **TerminalFailure 覆盖 pre/post-outcome 且只允许最小诚实 Error record**（见 Solution/Part 2） |
| R3-28 | v1→v2 只有内部 open_for_upgrade，没有显式审计/写入入口 | **新增严格 upgrade-db audit/apply；普通 open 对 v1 fail closed**（见 Part 5/8） |
| R3-29 | 仅用 mtime+size 的 source_version 会复用同尺寸/保时间戳的旧内容 | **保留 worker v1 source_version，新增 engine-owned source_guard_v2 进入全部 cache/snapshot identity**（见 Solution/Part 4/5/7） |
| R3-30 | full_vacuum 整库重写且需恢复点，成本收益不匹配 | **删除 full_vacuum；maintenance 只留 gc/incremental_vacuum；回滚由运维保留升级前 DB 副本**（见 Part 4/8） |

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

7 天窗口冷/温实测：冷扫 3735ms（parse 3387ms=91%，进上下文 14/136=10%）；温扫 347ms（discovery 272ms=78%）。这两个数来自现有 `ContextSummary.total_duration_ms`，不含新版 harness 要求的完整进程/transport/response validation 外层时间，只能解释 engine 内阶段，不能与 Part 6 的 `benchmark_wall_ms` 直接比阈值。单文件 parse：PDF ~2543ms、xlsx ~14ms、docx ~12ms、pptx ~18ms。

### 契约现实（已核实）

- 呈现顺序由 `decide_files` 决定，且优先级**依赖解析结果**（Error/Timeout/NotParsed → priority 80，`decision.rs:87`）——无法在解析前复制。
- 非 Success 一律转 action=Error；NotParsed 必须携带 Diagnostic（`decision.rs:56-60`）；计入 error_count（`store/mod.rs:1555`、`context_audit.rs:401`）。
- `ActiveRun.context_run_id()` 就是当前 `scan_run_id()`（`store/mod.rs:285-287`）——不能直接返回旧 context ID。
- `context_profile_hash` 仅含 `{engine_build, context}`（`context_audit.rs:413`）。
- 当前 `source_version` 只由 `mtime_ns:size_bytes` 组成（`rust/discovery/src/lib.rs`），同尺寸且保留 mtime 的替换会碰撞；worker v1 wire 又冻结该格式，因此需要 scanner-side guard，而不是偷偷改旧字段。
- `metadata_only` section 含多行字段（`compressor.rs:189`），205 个约 300 字符/section ≈ 撑满月报 60k 预算（`config.rs:67-70`）。
- `WorkerBackend` 仅 5 字面量（`scanner_contract.py:667`）；`WORKER_CONTRACT_VERSION` 为共享常量。
- scanner profile 为严格 v1；`requirements.lock` 由已删除的 `requirements.txt` 生成（`requirements.lock:1-2`）。

### 目标与成功标准

1. 90d 冷扫「卡死 >5min」→ 三次 cold median ≤40s/max ≤50s，且内部 45s deadline 不触发（语义 quota 保证质量 + 安全 deadline 防卡死）。
2. 月报 30d 冷扫 ~1min → 三次 cold median ≤20s/max ≤25s，且内部 25s deadline 不触发。
3. **缓存无关确定性**：空/部分/全缓存、且 worker/legacy source version/SourceGuardV2/parser 一致且无安全 deadline 触发时，final_context、decisions、summary 语义字段一致。
4. no-text PDF 提取调用 0 次（`pdfplumber_invocations=0`），单文件 ~2.5s → ~0.3s。
5. 性能门禁反作弊：只允许计划内 semantic/policy NotParsed，无 runtime NotParsed 或安全 deadline 触发。
6. 温扫无变化：7d snapshot warm median ≤330ms/max ≤400ms；30d/90d 相对 parse/classification-cache-only warm median 至少改善 20%，且语义输出不变。
7. fixed-corpus 门禁全绿；真实目录为**手工 acceptance**（非 CI 硬回归）。

## Solution

### 架构：BudgetedContextScheduler 深 module

外部 interface 只保留一个执行入口：

```text
BudgetedContextScheduler::execute(ScheduledRunInput) -> Result<BudgetedScanOutcome, SchedulerFailure>
```

`ScheduledRunInput` 只包含：当前 run ID/started_at、不可变 discovery snapshot、`NormalizedScannerProfileV2`、已校验的 worker identities，以及 run.rs 在 begin_run 时由 `total_deadline_ms` 唯一推导的 monotonic `AbsoluteDeadline`/`WorkDeadline`。二者不是第二份可配置数值；构造器校验同一 origin、`work=absolute-2,000ms` 与 profile 一致。`BudgetedScanOutcome` 一次返回：当前 run 的 inventory/file results/decisions、已提交 cache write receipts、artifact draft、diagnostics、metrics 与 terminal intent；调用方不得在返回后重新决定 action、计数或准入集合。

deadline、per-file Error/Timeout、`BUDGET_MODEL_MISMATCH` 等**已定义的业务终态都返回 `Ok(BudgetedScanOutcome)`**，由 terminal intent 表达 Success/Partial/Error；`Err(SchedulerFailure)` 只表示在形成可 validate outcome 之前的 adapter/contract/internal failure，并必须携带唯一 scanner-side Diagnostic 与 retryable。若 `begin_run` 尚未成功，run.rs 返回 IDs=null 的空 Context Error 且不持久化；若 `begin_run` 已成功，run.rs 必须以该 Diagnostic 尝试提交 IDs=当前 run 的空 Context Error terminal record。只有该 terminal COMMIT 也未发生时才进入 abandon cleanup。run.rs 不得补造 per-file rows，也不得把可提交的 post-begin Error 任意改成 Abandoned。

module 所有权：

- `BudgetedContextScheduler` 内部拥有 nominal ranking、`ClassificationPlan`、`ContentAdmissionPlan`、分类/cache/parser 合并、状态转换、Context 渲染和 deadline 终态。
- `ContextBudgetModel` 与确定性 `Compressor` 都是 Scheduler 的内部实现，不形成跨 module 的公共 seam；二者共用同一渲染计数函数。
- parser/classifier executor 与 `CachePort` 作为本地可替换 adapter 注入 Scheduler；生产 adapter 执行进程/SQLite，测试 adapter 使用内存结果与临时时钟。`CachePort` 只接受已完成且 source-version 后验校验通过的成功结果。
- `FinalizationStore` 位于 Scheduler 返回后的 terminal seam，由薄 run shell 注入；它只校验并事务化应用 `TerminalRecord = Outcome | TerminalFailure`，不重新计算业务规则。`TerminalFailure` 固定含 `phase=pre_outcome|post_outcome`、原始 scanner-side Diagnostic 与截止失败点已观测的 bounded execution metrics，只允许空 file rows、零业务 counts/decisions、`artifact_id=null`。`post_outcome` 只用于**打开 terminal transaction 前**发现 outcome/store invariant 无法安全持久化；原 outcome 不得部分降级或摘取几行。若最小 Error record 也未 COMMIT，则进入 Abandoned。transaction 已打开后的 statement/COMMIT 失败不能再另开一个“成功的 Error”覆盖，统一 rollback/Abandoned。`CachePort` 只做 inventory/cache lookup 与 receipt 型短事务；生产中可由同一 `ScannerStore` 实现两个窄 trait。run.rs 的顺序固定为：静态 request/profile/path 校验 → `begin_run`/idempotent replay → 创建 monotonic deadline → bounded parallel worker handshakes → discovery → Scheduler → terminal finalization；只有静态校验可在 deadline/lease 之前完成。
- Scheduler interface 是主测试面；旧 planner/decision/compressor 的内部测试只保留算法级边界用例，不复制一套端到端状态机断言。

### 语义 quota 与运行安全 deadline 分离（贯穿全局）

**语义 quota（确定性）**——决定正常 NotParsed/Omit 集合，进入 profile、golden output、快照键：
- `max_candidate_files`、`max_pdf_text_extractions`、`max_total_pdf_classification_pages`、Context 字符预算。
- quota 使用 **nominal charge**：cache hit 与 miss 收取相同语义名额；实际 inspected pages、是否启动进程和真实耗时只记 execution metrics，不返还或追加语义名额。
- 在同一 discovery snapshot + profile + classifier result 下结果确定，与 CPU 负载、线程完成顺序和缓存状态无关。

**运行安全 deadline（非确定性）**——只防卡死，不参与正常决策：
- `total_deadline_ms` 从 `begin_run` 后立即开始，覆盖 worker handshake、discovery、classification、parse、context 与 terminal finalization。固定预留最后 2,000ms 给 envelope/finalization：`WorkDeadline = AbsoluteDeadline - 2,000ms`；run.rs 与 Scheduler 都检查同一对 derived deadlines。全文中“work deadline 触发”专指停止/终止重工作并尝试形成 Partial/Error；“absolute deadline 耗尽”专指已无合法 terminal 提交窗口并进入 rollback/Abandoned，二者不得混称为一个可自由解释的 deadline。2,000ms 是停止启动重工作的**尾部保护**，不是“任何允许的 1,000,000 行审计都能在 2 秒提交”的 SLA；只有 Part 6/9 冻结规模的 corpus 才有性能承诺，极端合法 profile 若不能在 absolute deadline 前提交就按 Part 2 回滚并 Abandoned，不能偷偷延长 deadline。该值进入完整 profile/snapshot provenance，但不进入单文件 parse-cache key，也不提供正常 NotParsed 名额。
- work deadline 触发后立即停止启动新工作、终止 in-flight worker Job Object，并按 Part 2 的唯一终态表生成 Partial 或 Error；本轮永不生成可复用快照。
- 已完成且通过 source-version 后验校验的 classification/parse cache writes 可以提交，便于重试；未完成结果与**可复用 snapshot draft** 不提交。带 warning 的 Success 与 Partial context 只作为不可复用 payload artifact 在 finalization 保存。

持久化边界固定为三段，禁止实现时合并：

1. `begin_run` 独立取得 lease、建立 running attempt，然后才创建 monotonic deadline；store open/`begin_run` 继续受现有 SQLite busy timeout 约束，但不倒扣尚未创建的 deadline。
2. discovery 完成后，`CachePort.prepare_inventory` 先按固定最大 batch size，用一个或多个独立短事务 upsert 全局 `file_inventory`/`last_seen_run_id`，因为 parse/classification cache 对 inventory 有 FK；只有全部 discovery rows 都成功才返回 completed receipt 并允许 cache lookup。中途失败可留下已提交的全局 inventory prefix，但它不是 current-run file truth，后续按 `ON DELETE SET NULL`/orphan 规则处理，禁止据此合成截断 audit。随后 Scheduler 把已验证的成功 classification/parse 结果按有界 batch 写入其他**独立短事务**。每个 cache statement/COMMIT 前必须检查 `remaining_to_work_deadline>0`；SQLite COMMIT 若成功即是该 receipt 的线性化点，即使返回时刚越过 work deadline 也不得声称未写入，随后停止新 cache batch并进入 work-deadline 终态处理。空间不足/busy 可跳过并返回 receipt warning。`BudgetedScanOutcome` 只携带 committed/skipped receipt，不携带等待 finalization 的 cache mutation。因此 terminal transaction 回滚不会撤销已提交 cache，也不会把未完成结果写入 cache。
3. terminal transaction 之前完成 outcome validation、canonical JSON/hash、SQL 参数构造和需删除/保护 ID 的只读选择；这些准备工作同样计入 tail reserve，并在每个有界 batch 前检查 absolute deadline，耗尽时不打开 terminal transaction、直接走 abandon。事务内只允许带索引的 invariant recheck、必要且有界的 retention 删除、prepared/batched audit/artifact inserts、terminal status 与 lease delete，不再做 JSON 渲染、文件 I/O、worker 调用或可选 cache write。每个 insert batch 前再次检查 absolute deadline，耗尽即 rollback；同步为 artifact 腾挪空间的 terminal-run/orphan 删除与 artifact insert 仍在同一事务，要么一起提交，要么一起回滚。事务校验的是 DB inventory 与本轮 immutable discovery snapshot 一致；文件系统一致性点就是 discovery 读取的 source_version，snapshot 路径不承诺原子文件系统快照，运行中随后发生的变更由下一轮 discovery 观察。

所有 handshake、classification、parse、cache batch 和 context 阶段的 effective timeout 都是 `min(该操作自身 timeout, monotonic remaining_to_work_deadline)`；remaining≤0 时不得启动新工作。finalization 的 SQLite busy timeout 同样不得超过 remaining_to_absolute_deadline。SQLite `COMMIT` 是不可抢占系统调用，因此 COMMIT 前再检查 remaining>0；若 COMMIT 成功则按 Part 2 的线性化规则接受，若未成功则 rollback/abandon。不得把 2,000ms reserve 伪装成底层 fsync 的硬实时保证。

三态缓存一致性门禁**限定作用域**：worker/classifier build、legacy source version、SourceGuardV2、profile 与 parser/classifier 结果一致，且无安全 deadline、进程崩溃或瞬时 I/O 错误。超出该作用域必须可审计，但不冒充正常确定性输出。

### SourceGuard v2：不把 mtime+size 当内容身份

现有 `source_version=mtime_ns:size` 的 wire 格式继续给 frozen worker v1 使用，但不得单独决定 v2 cache/snapshot 命中。engine discovery 为每个 accepted file 另生成 `SourceGuardV2 {kind, guard_sha256}`：

- Windows 首选 `windows_file_id_change_time_v1`：从同一 opened handle 取得 canonical volume serial、128-bit file ID、size、last-write time 与 change time，按固定字段顺序/domain separator 做 SHA-256；Unix 首选 `unix_inode_ctime_v1`：device/inode/size/mtime_ns/ctime_ns 同样 canonical hash。只要任一 required field/API 缺失、返回平台定义的 unsupported/invalid-zero sentinel 或数值无法无损规范化，就固定回退 `content_sha256_v1`，流式读取完整 source bytes 做 SHA-256，不以 filesystem 名称启发式选择，也不使用首尾采样冒充完整 guard。
- 若 metadata guard 与 full-content fallback 都无法形成，记录 `kind=unavailable,guard_sha256=null`，该文件固定为 retryable `Error/error/SOURCE_GUARD_UNAVAILABLE`，不启动 classifier/parser，也不做 parse/classification cache lookup/write；已有其他合法非空 context 时 run 为 Partial，否则 Error。不可用不能退回“仅信 mtime+size”继续命中旧 cache。
- guard 在 discovery 后、消费 cache/snapshot 或启动 worker 前复核，并在 cache value/worker result进入 Scheduler 前后验复核；snapshot 命中要对全部 artifact rows 复核。任一次变化都丢弃刚取得的 value/result并按 `SOURCE_VERSION_CHANGED` retryable file error 处理，即使 legacy source_version 文本未变。
- parse/classification cache key、inventory exact guard、artifact provenance 与 snapshot key 都包含 `source_guard_policy=source_guard_v2 + kind + guard_sha256`；v1 parse-cache rows 因没有 guard 在显式 schema migration 中计数并清空，不得迁成可命中的伪 guard。`source_version_changed` miss reason 同时覆盖 legacy source_version 或 strong guard 变化，不新增一个猜测性 reason。
- `FileAuditV2` 增加有序字段 `source_guard_kind`、nullable `source_guard_sha256`；artifact eligible row 保存二者。execution metrics 增加 `source_guard_content_hash_file_count`、`source_guard_unavailable_count`、`source_guard_bytes_read`，均为实际本轮 non-negative u64；第一个按发生 fallback 的唯一文件计数，metadata guard 不计 bytes，完整 hash 每次读取按 bytes 累加。真实目录证据必须单列这三项，防止 fallback I/O 被藏进 discovery。

该 guard 面向正常本地/同步盘变更，不声明抵抗能伪造 file ID/change time 或制造 SHA-256 collision 的对手；这类对抗式完整性不在本 spec。source guard 的额外 wall time仍完整进入 Part 6/9 门禁，不能为过性能阈值关闭。

### Part 1：确定性准入计划（唯一事实源）

#### 1.1 完整 nominal rank

`priority_policy_version = budget_nominal_v2`。先把 relative path 的 `/` 转成 `\`，trim 首尾分隔符，以 Unicode lowercase 生成 `path_key = "\\" + lower_path + "\\"`。priority **按以下第一条命中项**决定：

| priority | 命中条件 |
|---:|---|
| 70 | path segment 为 `.pytest_cache`，或路径包含 `\data\benchmarks\` |
| 60 | path segment 为 `logs` |
| 20 | extension 属于 `.doc/.docx/.pdf/.ppt/.pptx/.xls/.xlsm/.xlsx` |
| 30 | extension 属于 `.md/.txt` |
| 50 | 其他 discovery 已接纳 extension |

完整排序键固定为：

```text
(priority, relative_path.to_lowercase(), relative_path, file_identity)
```

准入、quota 分配、解析结果归并、decision 和最终呈现全部使用该键。解析失败只改变 status/action/reason，不改变位置。planner 与 decision 不得各自保存一份排序实现。

#### 1.2 两阶段不可变计划

**阶段 A：`ClassificationPlan`（任何 PDF 分类 I/O 前冻结）**

1. 先按 Part 2 处理无 I/O 即可判定的 policy/invariant reject。
2. 对剩余候选按 nominal rank 分配前 `max_candidate_files` 个 candidate slots；其余固定为 `NotParsed/semantic_file_quota_exhausted`。
3. 每个获得 candidate slot 的 PDF 按顺序尝试预留 **完整 `pdf_max_pages`**，而不是实际提前发现文字时的页数。剩余页数不足则固定为 `not_classified_by_budget`。
4. classification cache hit 与 miss 都消耗同样的 `pdf_max_pages` nominal charge；miss 可并发执行，单次 effective timeout 为 `min(pdf_classification_timeout_ms, remaining_to_work_deadline)`，结果按 nominal rank 归并，完成顺序不得改变后续名额。

**阶段 B：`ContentAdmissionPlan`（分类完成后、任何正文 parse I/O 前冻结）**

1. `ContextBudgetModel` 先冻结 bounded omitted base；随后按 nominal rank 单趟考虑每个仍可准入的文件。
2. 普通文件/no-text metadata 的 `reserved_delta` 放得下就 admitted；放不下则 `NotParsed/global_context_budget_exceeded`，并继续考虑后续较小文件，不做第二轮回填。
3. `text_in_parse_window` PDF 只有在 `reserved_delta` 放得下且仍有 extraction slot 时才同时 admitted 并消耗一个 slot；cache hit 与 miss 都占一个。字符放不下时 reason=global budget 且**不**消耗 slot；字符放得下但 slot 用完时 reason=`pdf_text_extraction_quota_exhausted`。
4. `no_text_in_parse_window` admitted 后固定生成成功的 metadata-only draft，不占 extraction slot；若 metadata section 也放不下则按 global budget Omit。
5. 两个计划一旦冻结，cache 只决定“复用还是执行”，不得改变 ParseStatus、action、reason、排序或其他文件的名额。

#### 1.3 唯一 ContextBudgetModel

模型与 renderer 使用同一个 Unicode scalar 计数函数（等价于 Rust `chars().count()`），不以 UTF-8 bytes 或 token 估算。先冻结 `OmittedSummaryPlan`，并为**整个 omitted summary** 固定预留 `omitted_summary_reservation = min(12_000, floor(global_max_chars × 20%))`；该预留不能被正文 admission 借用，最终未用字符也不二次填充。

`OmittedSummaryPlan` 在不知道最终 global-budget reason 时也必须可构造，因此规则固定为：先在预留中放 mandatory header + 一个 catch-all aggregate row；再按 nominal rank 使用每个文件的 `max_omitted_row_chars`（最长允许 reason/error code、真实 path/extension 与整数上界）预选 detail slots，下一行放不下即停止。最终只渲染这些 slots 中确实 omitted 的文件，admitted 文件留下空余且**不由后续文件补位**。detail 之后按 `(reason, extension)` canonical order 放 aggregate rows；放不下的 group 合并到唯一 catch-all row。renderer 每次检查总 summary `<= omitted_summary_reservation`，因此 group 数、十进制位数和 group 消失造成的非加性变化都不会反向影响 admission。完整 per-file 审计仍在 DB/Inspect，不靠 `file_context` footer 承载。

随后计算固定 base 与每个可准入文件的 route-specific 最大 section 预留：

```text
base_chars = exact(header + fixed_sections + preexisting_bounded_error_sections)
             + omitted_summary_reservation
reserved_delta(file) = max(success_section_max,
                           metadata_section_max,
                           bounded_error_section_max)
base_chars + sum(admitted.reserved_delta) <= global_max_chars
```

- `*_section_max` 必须从 normalized route/parser limits 推导，不读取 cache 或真实解析结果；成本覆盖路径/Markdown 标题、action/reason/backend/lane、最大正文、input/output chars、围栏/换行、metadata 多行和固定提示。omitted detail/aggregate/catch-all 的全部成本只由独立 reservation 承担，禁止再按文件相减。
- `preexisting_bounded_error_sections` 只包含 ContentAdmissionPlan 冻结前已确定的 invariant/classifier Error/Timeout；后续 admitted parser failure 已由该文件的 `bounded_error_section_max` 预留，禁止重复计费。
- `base_chars > global_max_chars` 时返回非重试 `CONTEXT_FIXED_SECTIONS_OVER_BUDGET`，不尝试生成越界/空成功 context；正常 profile 的 header + aggregate footer 必须有 fixture 证明可放入。
- arbitrary Diagnostic message 不嵌入 `file_context`；正文只渲染有契约上限的 error code/path/backend，每个失败文件的 final Diagnostic message 留在 current audit/Inspect v2，其他 warning 使用下述 bounded projection，因而 error section 与 metadata 都有有限上界。
- `file_context` 禁止嵌入 request_id、scan/context run ID、cache/snapshot status、duration、wall-clock timestamp 等运行态字段；这些只进入 audit/Inspect，保证 artifact 可跨 run 复用。
- admitted 文件的真实渲染必须满足 `rendered_chars <= reserved_chars`。违反时返回非重试 `BUDGET_MODEL_MISMATCH` 内部 Error，不 panic、不静默 Omit、不截断其他文件。
- 保守预留造成的空余预算允许存在，记录 `reserved_chars`、`rendered_chars` 与差额以便后续调优，但不得二次填充较低优先级文件。

### Part 2：ParseStatus → action 完整状态矩阵

#### 2.1 无 I/O disposition

| 当前 planner 情况 | 新 ParseStatus | action/reason | Diagnostic | run/快照语义 |
|---|---|---|---|---|
| `FileTooLarge` | NotParsed | omit / `file_size_policy` | 无 error Diagnostic | 正常 policy skip，可快照 |
| `LegacyExtensionDisabled` | NotParsed | omit / `legacy_extension_disabled` | 无 error Diagnostic | 正常 policy skip，可快照 |
| `UnsupportedExtension`（已被 discovery 接纳） | Error | error / `profile_route_invariant` | 必须 | Partial/Error，不可快照 |
| `UnsupportedBackend` | Error | error / `profile_route_invariant` | 必须 | Partial/Error，不可快照 |
| SourceGuard metadata/full-hash 均不可用 | Error | error / `source_guard_unavailable` | `SOURCE_GUARD_UNAVAILABLE`，retryable=true | Partial/Error，不可快照；不启动 cache/classifier/parser |
| candidate/file/page/extraction/context quota 排出 | NotParsed | omit / 对应 semantic reason | 无 error Diagnostic | 正常 semantic skip，可快照 |

engine-owned `MAX_SOURCE_FILES_PER_RUN=1,000,000` 是完整 discovery snapshot/current audit/Inspect 共用的硬上限，不是可配置 quota。discovery 接纳第 1,000,001 个 source file 时立即停止且不返回截断 snapshot，产生非重试 `SOURCE_FILE_LIMIT_EXCEEDED` run-level Error；不执行 `prepare_inventory`、不合成一百万条半真 rows，ContextSummary 文件计数全 0，只在 execution metrics 记录 `discovery_observed_file_count=1,000,001`。它不能映射成 `semantic_file_quota_exhausted`，也不能通过增大 `max_candidate_files` 绕过。

`budget_reason` 不新增 ContextEnvelope/FileAudit wire 字段；统一写入持久化及 inspect 输出中的 `ContextDecision.reason`。允许值分三类：

- semantic：`semantic_file_quota_exhausted`、`pdf_classification_page_quota_exhausted`、`pdf_text_extraction_quota_exhausted`、`global_context_budget_exceeded`；
- policy：`file_size_policy`、`legacy_extension_disabled`；
- runtime：仅 `runtime_deadline_exhausted`，只允许出现在 Partial/Error run，永不具备快照资格。

非 budget 的 decision reason 也冻结：成功 Keep/Compress 沿用 `small_file_keep|large_log_tail|large_document_summary|medium_text_compress`；普通 size-based metadata 为 `file_size_policy`，no-text metadata 为 `pdf_no_text_in_parse_window`；route/source guard/source change Error 分别为 `profile_route_invariant|source_guard_unavailable|source_version_changed`；其余 classifier/parser Error 或 Timeout 统一为既有 `parse_error`，精确阶段/类型只看 final Diagnostic，禁止再造并行 reason taxonomy。action 与 reason 的非法组合在 Scheduler outcome validation fail closed。

#### 2.2 ParseStatus/action/计数

| ParseStatus | Diagnostic | Context action | summary 计数 | 单文件快照资格 |
|---|---|---|---|---|
| Success | 无 error Diagnostic | keep / compress / metadata_only | success_count | 合格 |
| Error | 必须 | error | error_file_count | 不合格 |
| Timeout | 必须 | error | timeout_count | 不合格 |
| NotParsed（semantic/policy） | 无 error Diagnostic，reason 必须在允许列表 | omit | 派生 not_parsed_count | 合格 |
| NotParsed（runtime） | run-level deadline Diagnostic；file row 无伪造 error | omit | 派生 not_parsed_count | 不合格 |

计数等式是 contract，不是报表端推断：

```text
decision_error_count = error_file_count + timeout_count
not_parsed_count = source_file_count - success_count - timeout_count - error_file_count
included_file_count = success_count
omitted_file_count = not_parsed_count
source_file_count = included_file_count + omitted_file_count + decision_error_count
```

- `has_error` 仅表示 ParseStatus::Error；Timeout 单独映射 error action/timeout_count，绝不落入 Keep/Compress。
- `ContextDecision.error_code` 对 Success 与所有 NotParsed 固定为空；Error/Timeout 必须等于该 file row `final_diagnostic.error_code`。parser fallback 后最终 Success 的历史失败只在 warnings/fallback provenance，不能把 decision 冒充 Error。
- 成功后因全局预算改 Omit 的兼容分支删除；出现即 `BUDGET_MODEL_MISMATCH`。
- extension metric 的 `file_count = success_count + error_count + timeout_count + not_parsed_count`，其中 `not_parsed_count` 由前三项派生，不扩展 v1 字段。
- `ContextDecision.input_chars`：有成功正文 parser result 时取该正文 Unicode scalar 数；no-text metadata、NotParsed、Error、Timeout 等没有可信正文时使用 discovery `size_bytes` 近似整数，呈现时显示 `~`。`output_chars` 对 keep/compress/metadata 及 Partial 中实际呈现的 error section 取该文件 section 的 scalar 数；omit 与 Error Envelope 中未渲染的 file rows 固定 0，aggregate/detail footer 是 run-level 输出，不强行分摊给文件。`ContextSummary.input_chars=sum(decision.input_chars)`，但 summary output 始终等于完整 `file_context.chars().count()`，不得错误要求等于 decisions.output_chars 之和。

对 **v2 schema 下新创建的 run**，`ContextEnvelope.warnings` 与 InspectRunResponse v1/v2 的 warnings projection 虽然 schema 上限为 100,000，本版运行规范固定最多 257 条：按 `(run_level_first, file_nominal_rank_or_sentinel, stage, error_code, canonical_message)` 保留前 256 条，其中无 file_path 的 deadline/projection/cache/session 等 run-level warning 排在任何 per-file warning 之前；剩余按 `(stage,error_code,retryable)` 聚合成 1 条 scanner-side `DIAGNOSTICS_AGGREGATED` warning，只含在 4,096-char message 上限内可放入的 canonical group counts，余组折叠为 `other_count`，不拼接原 message/path。每个失败文件的 final Diagnostic 另留在 current audit，并由 InspectRunResponseV2 返回；artifact semantic rows 仍只存 error_code。这样 Partial 的 warning 不变量、lossy v1 投影提示和新 final envelope metadata 大小不依赖错误文件数量。v1→v2 migrated terminal row 是唯一例外：原 warnings 原样重放，既不聚合也不重新取得 snapshot 资格，见 Part 8。

`DIAGNOSTICS_AGGREGATED` 是从 full current diagnostics 派生的 output-only row，不作为下一次 projection 的输入，也不重复写成一条 full diagnostic；自身固定 `stage=internal`、`retryable=被折叠 diagnostics 中任一为 true`、`file_path/backend=null`。Envelope 重建和 inspect 重放必须幂等地产生同一 warning list。它只属于可演进的 scanner-side ErrorCode，绝不进入 frozen `WorkerDiagnosticV1` 或旧 `ai_daily_transport` response。

#### 2.3 run 终态与安全 deadline

下表中的“触发 deadline”若未特别写明，均指 **WorkDeadline**；它应在仍有 2,000ms tail reserve 时停止重工作并形成终态。只有明确写 `AbsoluteDeadline` 的行才进入 Abandoned 语义。operation 自身 per-file timeout 仍是普通 Timeout，不等同于 run-level `STAGE_DEADLINE_EXHAUSTED`。

| 情况 | 未启动/in-flight 文件 | EngineStatus / RunStatus | Diagnostic | cache/artifact |
|---|---|---|---|---|
| work deadline 在 handshake/discovery 完成前触发 | 尚无完整 inventory，不合成 file rows | Error / Error | 实际 Process/Discovery stage 的 `STAGE_DEADLINE_EXHAUSTED` error，retryable=true | 无新 cache/artifact |
| discovery 已完成，但 `prepare_inventory` 尚未返回 completed receipt 即触发 work deadline | 不合成 current file rows；已提交 inventory prefix 只算全局 cache metadata | Error / Error | Cache stage 的 `STAGE_DEADLINE_EXHAUSTED` error，retryable=true | 未提交 batch 回滚；已提交 prefix 可保留；无 artifact |
| 仅 Success + semantic/policy NotParsed | 按冻结计划 | Ok / Success | 可有非错误 warning | 提交 cache；生成 payload artifact；仅 warnings 为空时可获 snapshot key |
| 任一正常 per-file Error/Timeout/unknown，且已有非空 context | 对应 Error/Timeout | Partial / Partial | warning，file error 保留 | 提交已验证 cache；生成不可复用 payload artifact |
| 任一正常 per-file Error/Timeout/unknown，且无法构造合法非空 context | 对应 Error/Timeout | Error / Error | canonical primary file Diagnostic 作为 Envelope error，其余走 bounded warnings；file error 全保留 | 提交已验证 cache；无 artifact |
| work deadline 触发且 Scheduler 已能构造合法非空 context draft | 未启动 → runtime NotParsed；in-flight → Timeout | Partial / Partial | `STAGE_DEADLINE_EXHAUSTED` warning，retryable=true | 只提交触发前已完成且 source-version 验证通过的 cache；生成不可复用 payload artifact |
| work deadline 触发且尚不能构造合法非空 context draft | 未启动 → runtime NotParsed；in-flight → Timeout | Error / Error | `STAGE_DEADLINE_EXHAUSTED` error，retryable=true；file_context 为空 | 可提交已验证 cache；无 artifact |
| terminal COMMIT 前观察到 absolute deadline，或 finalization transaction 未提交 | transaction rollback；不伪造 terminal rows | Abandoned；CLI 返回 retryable Store/Transport Error | Context/Internal stage | 走独立 abandon cleanup；同 request 可按现有 abandoned-retry 规则重试 |
| `BUDGET_MODEL_MISMATCH`、`CONTEXT_FIXED_SECTIONS_OVER_BUDGET` 或 Scheduler 无法构造合法 envelope | 不再继续 | Error / Error | Internal/Context error，retryable=false | 形成规范 Error outcome；已独立提交的 cache 不删除 |
| outcome/store invariant 在打开 terminal transaction 前拒绝原 outcome | 不摘取原 rows，改用 `TerminalFailure(post_outcome)` 的空 rows/零业务 counts | Error / Error | 原始 invariant Diagnostic，retryable=false | 原 artifact draft 不写；已独立提交的 cache 不删除；最小 Error 也未 COMMIT 才 Abandoned |

只要 `begin_run` 已成功且 terminal Error COMMIT 成功，Error Envelope 也必须返回 `scan_run_id=context_run_id=当前 scan_run_id`，并写一条 `context_runs`（`artifact_id=null`）；只有 begin 前静态拒绝才允许两个 ID 为 null。handshake/discovery/prepare_inventory 或其他 pre-outcome failure 不得因为 file rows 为空就跳过 terminal Error 持久化。表中的 Abandoned 只表示**任何** terminal COMMIT 都未发生，不是实现者可选择的第四种正常结果。

`prepare_inventory` 返回 completed receipt 后，即使 work deadline 发生在 cache lookup/write，完整 discovery rows 已有 FK，可按后两条 work-deadline 规则处理：尚未得到 cache evidence、也未启动 worker 的文件是 runtime NotParsed；对 PDF，此时 `pdf_classification=null`，不能伪造 budget/unknown result。只有真正 in-flight 的 classifier/parser worker 才是 Timeout；已完成 parse 但 cache receipt 未提交的文件保留完成状态，仅该 cache write 记 skipped。deadline 的 stage 使用实际 `Cache`，或将 `classification/parse/context` 内部阶段投影到现有 `DiagnosticStage::Parse`/`Context`；不得由实现者自行选择 Partial/Error。Partial 必须满足现有 `ContextEnvelope v1` 的非空 context + warning 不变量，Error 必须输出空 context + error Diagnostic。

当 Error Envelope 需要从多个 per-file failures 选一个 primary error 时，唯一顺序为 `(file nominal rank, diagnostic.stage, diagnostic.error_code, canonical_message)`；取第一条作为 `error`，其余只进入 bounded warning projection，current file audit 仍各自保留 final Diagnostic。run shell/store 不得按线程完成顺序或最后一次错误选择 primary。

handshake/discovery/prepare_inventory 前失败而没有 current file rows 的 Error envelope，其 ContextSummary 六个文件计数全部为 0；已观察的 discovery 数只进 execution metrics/Diagnostic，禁止写一个无法与 rows 勾稽的 `source_file_count`。

terminal transaction 的成功 COMMIT 是唯一线性化点：COMMIT 一旦成功，即使系统调用返回时刚越过 absolute deadline，已提交的 Success/Partial/Error 仍是权威结果，不得再改写成 Abandoned；外层 wall-clock 门禁会把该样本判失败。若 COMMIT 未发生，`abandon_run_after_failed_finalize` 使用 zero-wait lock 的独立短事务，同时把 scan run/attempt 标为 Abandoned 并删除 lease；只有该事务成功才主动释放 lease。cleanup busy/失败时保留 lease，让既有 TTL reclaim 同时完成 abandon，禁止留下 `running row + no lease` 的损坏状态。

同步修改面：`decision.rs`、`ContextFileEvidence::validate`、`context_audit.rs`、`store/mod.rs`、Rust/Python contract fixtures 及所有计数消费方。

### Part 3：PDF 分类状态机（独立于 Context action）

PDF classification **result** 状态固定为五种：`text_in_parse_window | no_text_in_parse_window | not_classified_by_budget | unknown | error`。它只描述 `min(page_count, pdf_max_pages)` 窗口，不使用 image-only/scanned-document 等全文件结论。nullable audit 还允许“没有可采信 result”：PDF 若在 classification I/O 前已被 source disposition/candidate-file quota 排出，虽获 page slot 但 runtime deadline 在 cache evidence/worker start 前触发，或 worker 返回后 source-version 双检失败导致结果被丢弃，都不创建 classification result。三者分别由 semantic/policy reason、`runtime_deadline_exhausted`、`SOURCE_VERSION_CHANGED` final diagnostic 消歧，不能伪装成 `not_classified_by_budget`、unknown 或 error。只有真正 in-flight 的 classifier 被 deadline 终止才创建 unknown/Timeout。

#### 3.1 有效文字判定 `pdf_text_presence_v1`

对窗口内每页调用 pypdfium2 text-page 提取，按 Unicode scalar 逐个检查。若至少存在一个字符满足以下全部条件，则整份文件为 `text_in_parse_window`：

1. 不是 Unicode whitespace；
2. `unicodedata.category` 不属于 `Cc/Cf/Cs/Co`；
3. 不是 `U+FFFD` replacement character。

不设最小词数；标点、旋转文字、隐藏 OCR 文字层均保守判为 text，宁可多走一次提取也不允许丢正文。`U+0000`、格式控制符和纯空白不算文字。检查到第一枚有效字符可停止实际检查，但 Part 1 的 nominal charge 仍为完整 `pdf_max_pages`。

#### 3.2 分类到 parse/action 的唯一映射

| 情况 | 分类状态 | 分类缓存 | ParseStatus/action | run/快照 |
|---|---|---|---|---|
| 窗口内发现有效文字，获得 extraction slot | text_in_parse_window | 是 | 继续 `pdf_text_v1`；按结果 Success/Error/Timeout | 由 parse 结果决定 |
| 窗口内发现有效文字，未获 extraction slot | text_in_parse_window | 是 | NotParsed/omit/`pdf_text_extraction_quota_exhausted` | 正常，可快照 |
| 完整窗口无有效文字 | no_text_in_parse_window | 是 | Success/metadata_only；`parser_backend=pdf_metadata_v1` | 正常，可快照；`pdfplumber_invocations=0` |
| 未获 classification page slot | not_classified_by_budget | 否 | NotParsed/omit/`pdf_classification_page_quota_exhausted` | 正常，可快照 |
| classifier per-file timeout | unknown | 否 | Timeout/error | 非空 context → Partial，否则 Error；不可快照 |
| classifier 崩溃/瞬时 I/O | unknown | 否 | Error/error，retryable=true | 非空 context → Partial，否则 Error；不可快照 |
| 加密、格式确定性损坏 | error | **否** | Error/error，retryable=false | 非空 context → Partial，否则 Error；不可快照 |
| classifier 后验 source-version 不一致 | null（结果丢弃） | 否 | Error/error/`SOURCE_VERSION_CHANGED`，retryable=true | 非空 context → Partial，否则 Error；不可快照 |

本版明确**不做负缓存**；`unknown/error` 都不写 classification cache，消除 [Part 2] 与 acceptance 的歧义。后续若引入确定性负缓存，必须另升 classifier policy/schema version。

parser 与 classifier provenance 不得混写：

- 在**非 snapshot 执行**中，未调用正文 parser 的 policy/semantic/runtime NotParsed、classifier Error/Timeout 均记录 `parser_backend=not_parsed`、`worker_lane=not_parsed`、`parse_cache_status=not_applicable`；classifier 实际运行 lane/transport 只进入 `pdf_classification`。
- 在非 snapshot 执行中，no-text metadata-only 是 Scheduler 在 Rust 内生成的成功内容，固定 `parser_backend=pdf_metadata_v1`、`worker_lane=rust_core`、`parse_cache_status=not_applicable`。`pdf_metadata_v1` 是 scanner-owned virtual parser backend，只用于 audit/context provenance，**不得**加入共享 `WorkerBackend`、`WorkerParseRequest/Response v1` 或 Python worker supported_backends。snapshot current row 的 parse cache status 统一按 Part 5 记录为 snapshot，但 backend/lane 仍保持上述语义 provenance。
- 只有真正启动/复用正文 parser 时才记录该 parser backend/lane 和 parse cache fresh/miss；in-flight parser timeout 保留实际启动的 backend/lane，尚未启动的 deadline row 仍为 not_parsed/not_parsed。

类型化 classification cache key **固定且只包含** `file_identity + source_version + SourceGuardV2 + classifier_profile_hash + classifier_build`；`classifier_profile_hash` 的 canonical payload 为 policy version、`pdf_max_pages`、`pdf_classification_timeout_ms`，不含全局 page quota/session 生命周期。成功 value 固定保存状态、page_count、`result_examined_pages`、source guard、classifier identity 与 cached_at；只有 text/no-text 两种成功状态可写入。当前 run 另记 nullable `run_inspected_pages`：只有 logical operation 的**每次 attempt**都能确认读页数时，才等于各 attempt 确认值之和；任一 timeout/crash/坏 transport 可能已读页但无法报告时，整项必须为 null，即使后续 retry 成功也不能只填最后结果页数。fresh/snapshot/not_eligible 固定为 0。`nominal_charged_pages` 来自当前冻结计划，不能从缓存 value 复制。result/run/nominal 三者不得用同一个 `actual_inspected_pages` 字段混淆。

`PdfClassificationAuditV1` 是 Inspect/FileAuditV2 的严格当前执行形状，字段/顺序固定为 `{status,page_count,classification_cache_status,classification_cache_miss_reason,result_examined_pages,run_inspected_pages,nominal_charged_pages,duration_ms,transport,attempt_count,classifier_build,classifier_profile_hash}`。三个 page 字段均为 nullable u64；nominal/duration/attempt 为 non-negative u64且 attempt≤3；cache status=`fresh|miss|snapshot|not_eligible`，transport=`session|one_shot|snapshot|not_applicable`；两个 identity 字段是 64-char lowercase SHA-256。artifact row 只保存其不可变 `PdfClassificationProvenanceV1 {status,page_count,result_examined_pages,nominal_charged_pages,classifier_build,classifier_profile_hash}` 子集，绝不保存 cache status/miss reason/run pages/duration/transport/attempt；snapshot current audit 再按下表重建零执行字段。令 `window_pages=min(page_count, normalized.parse.pdf.max_pages)`；逐态约束固定如下：

| status / 来源 | cache status | page_count / result pages | 当前 run execution | nominal charge |
|---|---|---|---|---:|
| text/no-text，classification cache hit | fresh | page_count 非空；text 为 `1..window_pages`，no-text 必须等于 `window_pages` | run pages=0、duration=0、transport=not_applicable、attempt=0 | `pdf_max_pages` |
| text/no-text，本轮执行成功 | miss | 与上行相同，result pages 为最终 typed result 确认值 | 所有 attempts 页数可观测时 run pages=其总和，否则 null；duration 为全部 attempts 墙钟和、transport=产生结果的 session/one_shot、attempt=1..3 | `pdf_max_pages` |
| text/no-text，context snapshot | snapshot | page_count/result pages 从 artifact 保留 | run pages=0、duration=0、transport=snapshot、attempt=0 | `pdf_max_pages` |
| unknown/error，本轮得到 typed failure result | miss | page_count 可空；result pages 为 `0..pdf_max_pages` | run pages 按所有 attempts 是否可观测求和/null；duration/transport/attempt 同本轮执行成功 | `pdf_max_pages` |
| unknown，timeout/crash/protocol failure且无 typed result | miss | page_count/result pages/run pages 均为 null | duration 为全部 attempts 墙钟和、transport=最后一次 attempted transport、attempt=1..3 | `pdf_max_pages` |
| not_classified_by_budget | not_eligible | page_count=null，result pages=0 | run pages=0、duration=0、transport=not_applicable、attempt=0；不做 cache lookup | 0 |

反向约束同样成立：fresh/snapshot 只允许 text/no-text；not_eligible 只允许 not_classified_by_budget；unknown/error 不得 fresh/snapshot，且必须有 FileAuditV2.final_diagnostic。classification cache status=miss 时 miss reason 必须取 Part 4 非空允许值，fresh/snapshot/not_eligible 时固定为空字符串。所有非 null `pdf_classification` 都必须携带当前 preflight 已验证的 classifier build/profile hash。非 PDF、pre-classification source disposition/candidate reject、runtime-before-start 与 classifier result 被 source-version guard 丢弃，一律 `pdf_classification=null`，由 ContextDecision/final Diagnostic 唯一消歧，不得套用上表零值对象。

classifier wire 独立于共享 worker v1：Python worker 新增必需的 `classifier-version` 与 one-shot `classify-pdf` 命令，使用严格 `ai_daily_pdf_classifier_v1` request/response；Part 7 session 的 `classify_pdf_v1` operation 复用同一 typed payload/result。profile 允许 PDF 时，classifier-version 缺失或不匹配必须 preflight fail closed，不允许绕过分类直接批量提取。

#### 3.3 独立数值门禁

固定 classifier corpus manifest 至少包含：窗口内 text 30 份、窗口内 no-text 100 份、确定性 error 5 份；稀疏单字符、CJK、旋转、mixed image/text、隐藏 OCR layer、空白页、文字仅在 max_pages 后、加密和损坏各至少 3 份。门禁口径：

- `false_negative = ground_truth_text_in_window → no_text`，必须 **0/全部**；
- `false_positive_rate = ground_truth_no_text_in_window → text / ground_truth_no_text_in_window`，必须 ≤0.1%；分母不足 1000 时等价为 0 个误判；
- 对所有预期可读 fixture：`unknown_rate = 0`、`unexpected_error_rate = 0`；
- 对确定性 error fixture：状态匹配率 100%，不得变成 no-text；
- 文字仅在 `max_pages` 之后时，ground truth 明确是 `no_text_in_parse_window`。

分类门禁只决定 classifier 是否可进入生产；提取 backend 替换仍使用独立质量/速度门禁，二者不能互相代替。

### Part 4：成本感知缓存 + 有界淘汰 + GC

“昂贵结果始终缓存”在本版精确定义为：**成功后始终尝试写入，不代表永不淘汰**。`cache_retention_v1` 是本版 engine-owned 固定策略，不新增一套无 wire 来源的部署配置；下表数值是 v1 常量并由 VersionResponseV2/maintenance 回显，调参必须另升 policy version。它不影响语义结果，也不进入 parse/context identity：

两类 cache lookup 都在任何 access-bucket 更新前按下列唯一树产生 miss reason；exact row 存在但 hash/schema/value validation 失败是 Store/RunCorrupt Error，不得降级成 miss：

1. exact key 合法存在 → status=fresh，reason=`""`；
2. exact miss，且存在同 `file_identity + source_version + SourceGuardV2` 的其他 identity row → parse 为 `parser_identity_changed`，classification 为 `classifier_identity_changed`；
3. 否则存在同 `file_identity` 的任意 cache row → `source_version_changed`（包括 legacy version 未变但 guard 改变）；
4. 否则 `prepare_inventory` 报告该 inventory 在本轮前已存在 → `entry_absent_or_evicted`；
5. 否则 → `new_file`。

parse cache status=miss 时 `cache_miss_reason` 必须是 `new_file|source_version_changed|parser_identity_changed|entry_absent_or_evicted`；classification status=miss 时 `classification_cache_miss_reason` 必须是 `new_file|source_version_changed|classifier_identity_changed|entry_absent_or_evicted`。非 miss 一律空字符串。identity_changed 同时覆盖 normalized parser/classifier profile、timeout、backend/build 等 key 输入变化，不凭猜测再细分；本版无 negative cache，因此 full_v2 run 不产生 `error_cache`，该旧 literal 只保留在 frozen v1 wire 与 migrated row 的 legacy columns。

| 载体 | v1 固定硬上限/保留 |
|---|---:|
| parse cache | 1 GiB |
| classification cache | 128 MiB |
| context artifacts（含 artifact rows） | 512 MiB |
| completed/abandoned run audit | 2 GiB logical bytes、最多 500 runs、最多 90 天，任一先到即淘汰 |
| opportunistic GC | 每次 finalize 10ms admission budget |

`cache_retention_v1` 的 eviction key 也冻结，升/降 rank 必须升级 policy version。按 tuple 升序删除，最后两个 tie-break 永远是 `cached_at_ms, primary_key_bytes`，保证同库同状态选择一致：

| cache | eviction tuple 前缀（越小越先删） |
|---|---|
| parse | `(generation_rank, recompute_rank, last_accessed_bucket, cached_at_ms, primary_key_bytes)` |
| classification | `(generation_rank, result_examined_pages, last_accessed_bucket, cached_at_ms, primary_key_bytes)` |

`generation_rank=0` 仅表示 entry 的 contract/schema/policy version 已不被当前 release 接受，或其 engine/worker/classifier build 已不是 live registered build；不同但仍合法可再次请求的 profile 值不得误判为 stale。其余为 `1`。parse `recompute_rank` 固定为：`light_text_v1=1`、`rust_xlsx_bounded_v1=2`、`rust_office_oxide_v1=2`、`python_sharepoint_text_v1=3`、`python_office_v1=4`、`pdf_text_v1=10`；新 parser backend 未升级 retention policy 时 fail closed 为 profile route invariant，不能临时选 rank。classification 只有成功 text/no-text entry，实际读页越少越先淘汰。不同 cache 各守自己的 byte cap，不跨表比较 rank。

- parse/classification byte cap 使用各 cache row 持久化的 `entry_size_bytes` 求和；artifact cap 使用 parent 持久化的 `artifact_size_bytes`，一次精确覆盖 parent 的 UTF-8/BLOB/canonical JSON 与全部 owned file/decision row payload，child 不再重复计费；terminal audit cap 使用 terminal `scan_runs.audit_size_bytes`，覆盖该 run/attempt/diagnostic/current file/current decision/stage/extension/context-run/metadata 的 exact logical payload，明确不含其引用的 artifact、全局 inventory 或 cache。三种 size 都不含不可稳定分摊的 SQLite page/index overhead，只由 Store 在 insert/migration 时计算，不接受 wire 输入；dedup guard 同时比较 artifact_size。显式 maintenance 对所有受 cap rows/owned groups 重算并比较，任一不符使 integrity_check=failed；物理 DB bytes/freelist 另行报告和压缩。
- parse/classification cache 写入将超过对应硬上限时，在对应 work-phase cache transaction 内按上述唯一 tuple 淘汰；单 entry 大于载体 cap 时直接 skipped。仍因无可删 entry、SQLite busy 或 deadline 无法腾出空间时，跳过该 cache write、返回 skipped receipt warning，但不改变本轮 context。
- Success/Partial 的 payload artifact 是 durable envelope 的必需数据，不能按 cache miss 静默跳过。artifact 将超上限时先同步删除 retention 允许的旧 runs/orphans；若仍因 pinned 引用无法容纳，当前 finalization 返回 Store Error，不写一个无法重放的 terminal run。
- PDF/Office/分类成功结果优先于 cheap text 保留；所有类型最终都受硬上限约束。单次 Context omit 永不成为删除理由。
- `last_accessed_bucket` 使用 UTC 日期，同一行同一天最多更新一次；批量 hit 在一个事务中更新。
- 被任何 `context_runs.artifact_id` 引用的 artifact 及其语义 rows 不得淘汰；删除 terminal run 后，只有引用计数为零的 artifact 才进入 orphan GC。
- snapshot hit 的 finalization 必须先建立当前 `context_runs.artifact_id` 引用，并在同一事务把所选 artifact/source run 加入临时 protected set，再执行 retention/orphan sweep；禁止在“旧引用已删、当前引用未建”的窗口误回收刚命中的 artifact。source run 可在后续 GC 被删并触发 `reused_from_context_run_id SET NULL`。
- terminal run GC 不级联删除全局 inventory/parse cache。v2 将 `file_inventory.last_seen_run_id` 改为 nullable FK `ON DELETE SET NULL`；inventory 只有在无 parse/classification cache、artifact file、current run row 引用且超过 90 天未见时才可 orphan GC。
- run retention 覆盖 Success/Partial/Error/Abandoned 及其 attempts；先删除超过 90 天且不在 protected set 的 rows，再按 `(finished_at_ms ASC, scan_run_id ASC)` 删除最老 rows，直到 terminal count≤500 且 `sum(audit_size_bytes)≤2,147,483,648`。为当前 terminal record 腾挪时使用“现存总量 + 当前 prepared audit size”比较；若删尽所有未保护旧 run 后当前 record 自身仍超 cap，当前 finalization fail closed，不部分落 audit。Abandoned 的 finished_at_ms 固定为成功 reclaim/cleanup 时刻。带 live lease 的 Running 绝不删除；过期 Running 必须先按既有 reclaim 规则原子转 Abandoned，之后才可进入 retention。
- terminal COMMIT 成功且 remaining_to_absolute_deadline≥10ms 时，才在独立事务尝试 opportunistic age/orphan row GC；它使用 busy_timeout=0、带索引的单个有界 delete batch，并在开始下一 statement 前检查 10ms admission budget。该路径只形成 freelist，不调用 incremental/full vacuum。SQLite statement/COMMIT 不提供硬实时抢占；run.rs 可记录非持久 runtime span，但本 spec 不伪造一个精确、可重放的 per-run GC duration。busy/超时/overshoot 不回滚或改写已提交 terminal result，完整成本始终留在 `benchmark_wall_ms`，不能从性能门禁中扣除。

显式 `maintenance` 子命令取得独占 scanner lease 后做无时间上限深度清理、`PRAGMA integrity_check` 与所选物理压缩。**本版不做 `full_vacuum`**：整库重写且需恢复点，成本收益不匹配；物理收缩只通过 `incremental_vacuum` 在有 `auto_vacuum=INCREMENTAL` 的库上进行。两个 strict deny-unknown wire 冻结为：

```text
MaintenanceRequestV1 = {
  contract=ai_daily_scanner_maintenance, protocol_version=1,
  request_id, scan_db_path,
  mode=gc|incremental_vacuum, dry_run
}
MaintenanceSizeV1 = {
  parse_cache_logical_bytes, classification_cache_logical_bytes,
  context_artifacts_logical_bytes, terminal_audit_logical_bytes,
  database_file_bytes, wal_file_bytes, shm_file_bytes, total_physical_bytes,
  freelist_bytes,
  auto_vacuum_mode=none|full|incremental
}
MaintenanceDeletedV1 = {
  parse_cache_rows, classification_cache_rows, context_artifacts_rows,
  context_artifact_files_rows, context_artifact_decisions_rows,
  scan_runs_rows, scan_run_attempts_rows, run_diagnostics_rows,
  scan_file_results_rows, scan_stage_metrics_rows,
  scan_extension_metrics_rows, context_runs_rows, context_decisions_rows,
  file_inventory_rows
}
MaintenanceVacuumV1 = {
  mode=gc|incremental_vacuum,
  status=not_requested|skipped_dry_run|ok|error, pages_changed
}
MaintenanceResponseV1 = {
  contract=ai_daily_scanner_maintenance, protocol_version=1, request_id,
  status=ok|error, cache_retention_policy, before:MaintenanceSizeV1,
  after:MaintenanceSizeV1, after_complete, deleted:MaintenanceDeletedV1,
  pre_integrity_check=ok|failed,
  post_integrity_check=not_run|ok|failed, vacuum:MaintenanceVacuumV1,
  warnings, error:null|ScannerDiagnostic
}
```

所有 request/response 字段都 required，包括 nullable 字段。任何模式的 dry-run 都不得创建文件来“试写”，真实执行仍须重新校验并承担 TOCTOU 失败。

`cache_retention_policy` 与 VersionResponseV2 逐字段相同。单次 integrity check 只在 `PRAGMA integrity_check`、`foreign_key_check`、entry_size 全量重算、artifact hash/row-count/nullability invariant 全部通过时为 ok。maintenance 顺序固定为：取得独占 lease → before sizes → pre integrity → mode preflight →（dry-run 则结束）→ 深度 row GC transaction → 所选 vacuum → post integrity → after sizes。pre/mode preflight 失败时必须零删除、零 checkpoint、零 vacuum、post=not_run、before=after。status=ok 要求 error=null、pre=ok、post 对 dry-run 为 not_run/对非 dry-run 为 ok、vacuum.status 不为 error、after_complete=true。status=error 要求 error 非空且 `stage=maintenance`。warnings 使用新-run 的 257 条 scanner-side projection。合法 mode 的 dry_run 必须 deleted 全 0、before=after、after_complete=true、vacuum.status=skipped_dry_run，不能创建 sidecar、checkpoint、GC 或 vacuum；mode unavailable 即使 dry_run 也返回 error/vacuum.status=error。`mode=gc` 的非 dry-run vacuum.status=not_requested。

maintenance 只接受 v2 DB；v1 固定返回 `SCHEMA_UPGRADE_REQUIRED`，TooNew fail closed。

新建空 v2 DB 必须在创建第一张表前设置 `PRAGMA auto_vacuum=INCREMENTAL`。v1 升级后的 DB 未做整库 VACUUM 前通常仍为 none；此时请求 `mode=incremental_vacuum` 必须返回 `MAINTENANCE_MODE_UNAVAILABLE`/status=error，不能把 no-op 回报 ok。本版不提供自动转换（需整库 VACUUM），升级库的物理收缩列为已知限制，strict doctor 对未转换库提示受限。

深度 row GC 与 VACUUM 无法组成一个 SQLite transaction：先提交 GC transaction，再执行所选 vacuum。vacuum 失败时已提交的 GC 删除不回滚，MaintenanceResponse status=error 但 deleted/before/after 必须报告实际部分进展；这不回滚任何此前业务 terminal transaction，也不能用 status=error 把已删除 rows 隐藏成 0。
GC transaction 失败时不再启动 vacuum；任何 mutation phase 失败后仍尽力执行只读 post integrity/after sizing。若 DB 已不可读，after 复制最后一个完整 size snapshot（至少是 before）、`after_complete=false` 并返回 error；不得伪造 ok 或用零值冒充实测。
- opportunistic GC 不执行全量 `VACUUM`。`VACUUM`/`incremental_vacuum` 只在 migration 后独立维护步骤或 `maintenance` 中运行，失败不回滚已成功的业务事务。
- `snapshot_hit`、`parse_cache_all_hit`、`classification_cache_all_hit` 分开度量，不互相推导。

### Part 5：快照快速路径（关系模型 + provenance）

#### 5.1 schema v2 关系模型

```text
file_inventory
  file_identity              PK
  source_version             frozen worker-v1 mtime_ns:size
  source_guard_kind          NULLABLE; windows_file_id_change_time_v1|unix_inode_ctime_v1|content_sha256_v1|unavailable
  source_guard_sha256        NULLABLE
  CHECK migrated-v1 inventory 可两者皆空；full-v2 upsert 后 unavailable=>hash null，其他 kind=>hash 必填

context_artifacts
  artifact_id                PK
  snapshot_eligible          BOOL
  snapshot_key_sha256        NULLABLE, UNIQUE WHERE snapshot_eligible=1
  snapshot_key_json          NULLABLE canonical exact-match guard
  final_context              immutable, stored once
  context_sha256
  semantic_summary_json      counts + input/output/reserved/rendered chars，no timings/request_id
  artifact_size_bytes        parent + all owned semantic rows 的 exact logical bytes
  created_at_ms / last_accessed_bucket
  CHECK eligible => both snapshot key fields non-null
  CHECK ineligible => both snapshot key fields null
  context_sha256             adapter 在 insert/replay 两处校验 = SHA-256(final_context)

context_artifact_files
  artifact_id + file_identity PK
  artifact_id                FK ON DELETE CASCADE -> context_artifacts
  file_identity FK RESTRICT -> file_inventory
  immutable legacy source version + SourceGuardV2 + parse/classification/content provenance
  # no cache_status/cache_miss_reason/parse_duration_ms
  # no run_inspected_pages/duration/transport/attempt

context_artifact_decisions
  artifact_id + file_identity PK
  (artifact_id,file_identity) FK ON DELETE CASCADE -> context_artifact_files
  immutable action/reason/priority/input/output/truncated/error_code

context_runs
  context_run_id             PK/FK ON DELETE CASCADE -> scan_runs
                                      # 当前 run 自己的 ID（= 当前 scan_run_id）
  artifact_id                FK RESTRICT -> context_artifacts
                              # Success/Partial 必填，Error 必须为空
  reused_from_context_run_id FK ON DELETE SET NULL -> context_runs(context_run_id)
                              # 仅 snapshot_hit 必填，否则为空
  当前 run 的 status / semantic counts / timings / snapshot_hit

scan_runs
  final_envelope_metadata_json        # 不含 file_context
  audit_provenance_version            # migrated_v1 | full_v2
  audit_size_bytes                    # terminal run + owned current audit logical bytes
```

`context_artifacts` 同时承担非空 context 的去重 payload storage 与可选 snapshot source；所有 Success/Partial run 都引用一个 artifact，只有满足 5.4 的行才有 snapshot key。`final_envelope_metadata_json` 是内部 storage schema，不冒充 wire `ContextEnvelope`；它保存 request/engine/status/warnings/error 等小字段。幂等重放或 CLI 返回时，Store 用 metadata + 当前 `context_runs` summary + `context_artifacts.final_context` 重建并重新 validate `ContextEnvelope v1`。因此 Success/Partial run 不再在 `scan_runs` JSON 中重复正文；Error run 无 artifact，重建空 context。

`snapshot_eligible=true` 的 artifact 必须为每个 source file 各有一条 artifact file 与 decision row，且两个 row count 都等于 semantic summary 的 source_file_count；`false` 的带-warning Success、Partial、migrated payload artifact 必须完全没有这两组 rows，只承担当前 run 的正文重放，避免保存一套永不复用的不完整语义集。Store 在 insert 与 replay 两处校验这个双向约束，不能把“缺 rows 的 eligible artifact”当 cache miss 后继续。

eligible 非 snapshot 执行在 finalization 也按 snapshot key 去重：key 不存在才插入新 artifact；若已存在且 `snapshot_key_json`、context hash、semantic summary、artifact file/decision rows 逐字段一致，则当前 run 直接引用既有 artifact；任一字段不一致是非重试 `BUDGET_MODEL_MISMATCH`/store invariant，绝不覆盖旧 artifact。比较在 canonical field/order 下覆盖 nullable classification 字段，不能只比较 row count/hash。这保证重算或 cache-only benchmark 不会撞 UNIQUE、重复正文或掩盖“同 identity 却不同语义”。

#### 5.2 当前 run 审计语义

- v2 `scan_file_results` **不覆盖历史 cache 证据**：把旧 `cache_status/cache_miss_reason` 迁到 nullable `legacy_cache_status/legacy_cache_miss_reason`，另新增 nullable `parse_cache_status CHECK(fresh|miss|snapshot|not_applicable)` 与 v2 reason。`audit_provenance_version=migrated_v1` 时 legacy 两列必填、新列与 source guard 两列必须为空；`full_v2` 时恰好相反，SourceGuard kind 必填（unavailable 允许 hash null），且只有 `parse_cache_status=miss` 可携带 Part 4 非空 reason，其他三态必须为空。默认 Inspect v1 对 migrated row 原样读 legacy 列（含可能的 `error_cache`），对 full_v2 才执行 lossy projection；Inspect v2 只接受 full_v2。这样既不伪造历史，也不把旧 literal 塞进新 CHECK。
- `file_inventory.last_seen_run_id` 迁为 nullable `ON DELETE SET NULL`；删除 run 只级联其 current audit/decision rows，不删除仍被 cache/artifact 引用的 inventory。
- discovery 仍按当前 run upsert 全局 `file_inventory/last_seen_run_id`；不存在“把旧 inventory 冒充当前 discovery”的复制。
- 正常执行 run 把语义 file/decision rows 同时写入当前 `scan_file_results/context_decisions`；仅具备快照资格时再复制一份去除运行态字段的 artifact rows。
- snapshot hit 时，以 artifact rows 生成**当前 run** rows：所有行 `parse_cache_status=snapshot`、`cache_miss_reason=''`、`parse_duration_ms=0`、`parse_attempt_count=0`；parser backend、worker lane、source/content hash 与 classifier result provenance 来自 artifact。classification text/no-text 行重建为 cache status=snapshot、run pages=0、attempt=0、transport=snapshot；not_classified_by_budget 仍是 not_eligible/0/0/not_applicable；pre-classification reject 仍为 null。不得复制源 run 的 miss/hit、旧耗时或旧执行读页数。只有**非 snapshot 执行**中未产生正文 parser provenance 的 no-text metadata、policy/semantic/runtime NotParsed、classifier failure 等行使用 `parse_cache_status=not_applicable`；是否做过 lookup 另由 execution metrics 诚实记录。

`FileAuditV2` 的 parse provenance 不再留给实现者组合；逐态矩阵固定为：

| 当前文件来源 | parse cache / miss reason | backend/lane | transport / attempts / duration | final Diagnostic |
|---|---|---|---|---|
| context snapshot（包括 artifact 中的 semantic/policy NotParsed） | `snapshot / ""` | artifact 的语义 provenance | `snapshot / 0 / 0` | artifact 只保留 error_code；eligible snapshot 不含 Error/Timeout，因此当前值为 null |
| exact parse-cache hit | `fresh / ""` | cached parser provenance | `not_applicable / 0 / 0` | null |
| 本轮正文 parser 成功、Error 或 Timeout | `miss / Part 4 非空 reason` | 实际 parser provenance；Timeout 保留已启动 lane | `rust_in_process|session|one_shot / 1..3 / 所有 attempts 墙钟和` | Success 为 null；Error/Timeout 必填 |
| no-text metadata、policy/semantic NotParsed、classifier failure，或 work deadline 前正文 parser 从未启动 | `not_applicable / ""` | Part 3 的 `pdf_metadata_v1/rust_core` 或 `not_parsed/not_parsed` | `not_applicable / 0 / 0` | 服从 Part 2/3；runtime NotParsed 不复制 run-level error |

`parse_cache_status` 表示**最终文件内容的 parse 来源**，不是“是否执行过 lookup”：例如 lookup miss 后、worker 启动前触发 work deadline 的 runtime NotParsed 仍为 not_applicable，但该次 lookup 必须计入 `execution_metrics.parse_cache_lookup_count`/`parse_cache_all_hit=false`。`fresh` 明确等于 exact cache hit，不表示“本轮新鲜执行”。`failure_class/fallback_backend/fallback_reason_code` 保持 v1 的 parser-backend fallback 语义；session 重建或同 backend 的 one-shot transport fallback 只写 `parse_transport`、attempt metrics 与 scanner diagnostics，禁止塞进 backend fallback 字段。

- 当前 decisions 从 artifact 复制，当前 timings/worker handshake/snapshot metric 重新计算。`finalize` 继续校验当前 rows、当前 summary、artifact semantic summary 与重建 envelope 四者一致。
- snapshot lookup 的 SQL 必须同时选中 artifact 与至少一个已提交、status=Success、引用该 artifact 的 source `context_runs`；没有任何 source run 的 orphan artifact 不算 hit，先按 miss 重算。重算语义逐字段相同后可按 5.1 dedup 并由当前 run 重新建立引用，这不计 snapshot_hit、reused_from 仍为 null。
- `reused_from_context_run_id` 只在 `snapshot_hit=true` 时记录本次选择的 source run：从当前 transaction 开始前已提交、status=Success、引用该 artifact 的 `context_runs` 中按 `(finished_at_ms DESC, context_run_id DESC)` 取第一条；当前 run 不参与选择。正常重算并 dedup 到同一 artifact 的 run 保持 null。源 run 被后续 GC 删除时允许 `ON DELETE SET NULL`，`artifact_id` 仍保留完整语义 provenance。后续复用依赖 artifact-owned rows，不依赖最初源 run 永久存活。
- 删除最后一个引用 run 后，artifact 与其 rows 成为 orphan，由 Part 4 GC 删除。

#### 5.3 inspect 与观测 interface

`ContextEnvelope v1` 保持字段形状不变；现有 `inspect-run` 默认 v1 投影继续可用。为避免把 snapshot 冒充 parse-cache hit，同一命令新增 CLI 参数 `--response-version 2`，返回 deny-unknown 的严格 `InspectRunResponseV2`。必填字段及序列化顺序固定为 `{contract,protocol_version,response_version,request_id,scan_run_id,context_run_id,status,run_status,summary,stage_metrics,extension_metrics,files,decisions,warnings,error,artifact_id,reused_from_context_run_id,reuse_kind,execution_metrics}`；固定 `contract=ai_daily_context`、`protocol_version=1`、`response_version=2`。所有 v1-origin 业务字段沿用 v1 nullability/上限，只用 `FileAuditV2` 替换 `files` item；新增字段冻结为：

- `artifact_id/reused_from_context_run_id/reuse_kind`：Inspect status=ok 时，Success/Partial run 的 artifact_id 必填，Error run 必须为空；`context_snapshot` 仅 snapshot hit 且 reused_from 非空；`parse_cache` 仅无 snapshot、至少发生一次正文 parse lookup 且 `parse_cache_all_hit=true`；无 lookup、mixed hit/miss 或 Error run 的 reuse_kind 为 `none`，reused_from 必须为空。Inspect status=error 时 artifact/reused 均为空、reuse_kind=none、files/decisions 为空、execution_metrics 使用全零/null **error sentinel object**，沿用 v1 的非空 error Diagnostic；调用方必须先判 status，sentinel 绝不是被检查 run 的执行证据；
- `FileAuditV2` 是 deny-unknown object，字段/顺序固定为 `{relative_path,file_identity,source_version,source_guard_kind,source_guard_sha256,parse_status,parser_backend,worker_lane,parse_cache_status,cache_miss_reason,truncated,content_sha256,parse_duration_ms,failure_class,fallback_backend,fallback_reason_code,parse_transport,parse_attempt_count,final_diagnostic,pdf_classification}`。source guard 字段服从 SourceGuard v2 的 kind/hash 双向不变量；`parse_cache_status=fresh|miss|snapshot|not_applicable` 取代含混的 v1 cache status，`parse_transport=session|one_shot|rust_in_process|snapshot|not_applicable`；Error/Timeout 的 nullable final_diagnostic 必须非空，Success 与 semantic/policy NotParsed 必须为空，runtime NotParsed 只引用 run-level deadline diagnostic，不复制成伪造 file error。nullable `pdf_classification: PdfClassificationAuditV1` 的 nullability 和每个字段严格服从 Part 3 完整矩阵，不在 Inspect 另定义一套简化规则；
- `execution_metrics` 是严格 object，固定含 `discovery_observed_file_count,source_guard_content_hash_file_count,source_guard_unavailable_count,source_guard_bytes_read,candidate_file_count,admitted_file_count,classification_slot_count,confirmed_run_inspected_pages_total,unobserved_classification_attempt_count,nominal_charged_pages_total,extraction_slot_count,pdfplumber_invocations,snapshot_hit,parse_cache_lookup_count,classification_cache_lookup_count,parse_cache_all_hit,classification_cache_all_hit,stage_deadline_exhausted_count,session_restart_count,session_fallback_count,classify_attempt_count,parse_attempt_count,reserved_chars,rendered_chars,worker_handshake_ms,discovery_ms,snapshot_lookup_ms,current_run_audit_write_ms,terminal_precommit_ms,deadline_precommit_elapsed_ms,envelope_rebuild_ms,terminal_rows_written,peak_worker_rss_bytes`。`snapshot_hit` 为 bool，两个 `*_all_hit` 与 `peak_worker_rss_bytes` 为 nullable，其余全部是 non-negative u64；未知值不得用 0 代替。两个 `*_all_hit` 在对应 lookup_count=0 时必须 null，否则是 bool。Inspect operation 自身 `status=error` 的 sentinel 固定为所有 non-nullable numeric=0、三个 nullable 字段均为 null、`snapshot_hit=false`；它不代表被检查 run。

计数口径只有下面一套：

| metrics | 唯一含义 |
|---|---|
| `discovery_observed_file_count` | discovery 实际观察到的 accepted source 数；触发 source ceiling 时固定 1,000,001，其他完整 run 等于 `source_file_count` |
| 三个 `source_guard_*` | full-content fallback 的文件数、guard unavailable 文件数、所有 full hash attempts 实际读取 bytes 总和；metadata guard 均不计 bytes，retry/后验复核重复读取必须重复计 |
| `candidate_file_count` | ClassificationPlan 获得 candidate slot 的文件数；policy/invariant reject 与 file-quota 排出不计 |
| `classification_slot_count` / `nominal_charged_pages_total` | 获得完整 page reservation 的 PDF 数 / 这些 reservation 的页数和；cache hit 同样计，`not_classified_by_budget` 不计 |
| `admitted_file_count` | ContentAdmissionPlan 冻结为正文 parse 或 metadata-only 的文件数；后续 parser Error/Timeout 仍计，计划内 omit 不计 |
| `extraction_slot_count` | 获得 PDF text extraction slot 的 text PDF 数；parse cache hit 与 miss 都计 |
| `confirmed_run_inspected_pages_total` / `unobserved_classification_attempt_count` | 只求和所有非 null `run_inspected_pages`；每个无法确认页数的 classifier attempt 使后者 +1，禁止把 retry 成功前的未知工作当 0 |
| `pdfplumber_invocations` | 实际进入 `pdf_text_v1` 的 pdfplumber 调用次数，按 attempt 计；cache/snapshot/no-text 均为 0 |
| 两个 `*_cache_lookup_count` / `*_all_hit` | 每个 eligible file 的 exact-key lookup 计 1，miss-reason tree 的辅助查询不另计；任一 exact miss 即 all_hit=false，snapshot hit 跳过两类 lookup并得到 0/null |
| `classify_attempt_count` / `parse_attempt_count` | 实际启动的 logical-operation attempts 总数；cache/snapshot 为 0，classify 与 parse 分开累计 |
| `session_restart_count` / `session_fallback_count` | initial hello 后因故障或寿命策略实际替换 session 的次数 / capability 已宣告后，重试规则实际进入 one-shot 的 logical operation 数；capability absent 从头 one-shot 两者均不伪装为 fallback |
| `stage_deadline_exhausted_count` | 本 run 的 WorkDeadline run-level trigger 数，只允许 0 或 1；普通 per-file timeout 不计 |
| `reserved_chars` / `rendered_chars` | 最终冻结预算与实际 `file_context.chars().count()`；snapshot 从 artifact semantic summary 重建，形成计划前的 TerminalFailure 为 0/0 |
| `terminal_rows_written` | 成功 terminal transaction 内 current run/artifact/context/metric 的 INSERT/UPDATE logical rows；不含 retention DELETE、cache transaction 或 maintenance rows |
| `peak_worker_rss_bytes` | 本 run 所有 child worker 的可观测 peak RSS 最大值；未启动 child 为 0，Windows 计量失败为 null并产生 warning、使性能样本无效 |

timing 全部来自同一 monotonic clock，parallel span 按 wall interval 而非子任务耗时求和，彼此可包含/重叠，禁止用相加重建 total。`worker_handshake_ms` 是整批并行 preflight wall span；`discovery_ms` 必须等于 v1 summary/stage 的 discovery duration；`snapshot_lookup_ms` 是 lookup/strict guard span；`current_run_audit_write_ms` 是 terminal transaction 内 current audit insert statements 的 span；`terminal_precommit_ms` 从 transaction begin 到最终 metrics row 绑定前，包含 retention/artifact/audit/status 准备但不含该 metrics write 与 COMMIT；`deadline_precommit_elapsed_ms` 是同一绑定点距 deadline origin 的 elapsed；`envelope_rebuild_ms` 是 COMMIT 前 metadata+summary+artifact 重建及 validate span。其他新增 phase 不挤占/改名旧 StageName；不可抢占 COMMIT、post-commit GC、process exit 与 response validation 只由 harness `benchmark_wall_ms` 完整证明。

`audit_provenance_version=full_v2` 才允许返回 InspectRunResponseV2 status=ok。migrated v1 run 缺 parse attempt/transport、PDF classification 与可靠 per-file final Diagnostic；对它请求 `--response-version 2` 必须返回上述 strict status=error 形状，error 固定 `error_code=INSPECT_V2_PROVENANCE_UNAVAILABLE,stage=inspect,retryable=false,file_path/backend=null`，绝不以 0/null 冒充“本轮没执行”。默认 v1 inspect 与 ContextEnvelope replay 继续使用迁移前语义并原样可用。

v1 投影无法表达 snapshot/not-applicable 时，分别投影为 `fresh`/`miss`、duration=0，并附 `SNAPSHOT_REUSE_PROJECTED_AS_FRESH` 或 `PARSE_CACHE_NOT_APPLICABLE_PROJECTED_AS_MISS` warning；v2 `parser_identity_changed` 可无损投影为旧 `parser_profile_changed`，`entry_absent_or_evicted` 只能投影为 `new_file` 并附 `CACHE_MISS_REASON_PROJECTED_AS_NEW_FILE`。full_v2 file rows 的 SourceGuardV2 在 v1 没有字段，统一再附一条 run-level `SOURCE_GUARD_NOT_PROJECTED`，不能把 legacy source_version 冒充完整身份。四种 projection warning 都固定 `stage=inspect,retryable=false,file_path/backend=null`。它们是 inspect-v1 output-only diagnostics，和 aggregate row 一样不写回 full diagnostics、Envelope metadata 或 snapshot eligibility，但与原 warnings 合并后仍受 257 条投影上限。所有新 acceptance 必须读取 v2，禁止从 v1 投影推断 cache 类型/reason/source guard。`version` 的 command 名仍为 `inspect-run`，不改变冻结顺序。

scanner `version` 同样采用兼容投影：默认 `VersionResponse v1` 及其四个 supported_commands 逐字段不变。`version --response-version 2` 返回 deny-unknown 的严格 `VersionResponseV2`，必填字段/序列化顺序固定为：

```text
contract=ai_daily_context
protocol_version=1
response_version=2
binary_name=ai-daily-scanner
engine_version
engine_build
target_triple
supported_commands
office_worker_contract_version
python_worker_contract_version
accepted_scanner_profile_versions
inspect_response_versions
classifier_contract_versions
session_contract_versions
maintenance_contract_versions
upgrade_contract_versions
source_guard_policy=source_guard_v2
max_source_files_per_run=1000000
cache_retention_policy
```

除新增 `response_version` 外，`contract/protocol_version/binary_name/engine_version/engine_build/target_triple/office_worker_contract_version/python_worker_contract_version` 均沿用 v1 validation；canonical arrays/order 固定为：

```text
supported_commands = [version, doctor, build-context, inspect-run, maintenance, upgrade-db]
accepted_scanner_profile_versions = [scanner_profile_v1, scanner_profile_v2]
inspect_response_versions = [1, 2]
classifier_contract_versions = [ai_daily_pdf_classifier_v1]
session_contract_versions = [ai_daily_python_session_v1]
maintenance_contract_versions = [ai_daily_scanner_maintenance_v1]
upgrade_contract_versions = [ai_daily_scanner_upgrade_v1]
```

`cache_retention_policy` 也是 deny-unknown 的严格 object，字段/顺序固定为 `{policy_version=cache_retention_v1,parse_cache_max_bytes=1073741824,classification_cache_max_bytes=134217728,context_artifacts_max_bytes=536870912,terminal_audit_max_bytes=2147483648,terminal_run_max_count=500,terminal_run_max_age_days=90,opportunistic_gc_budget_ms=10}`。新 deployment/doctor/maintenance tooling 必须查询 v2，旧调用方继续读取 v1；不得把 v2 字段偷偷加入默认 v1 response，也不得用部署配置覆盖这些 v1 policy 常量。

#### 5.4 snapshot identity 与资格

snapshot key 的 canonical JSON 精确包含：去 request_id 的 logical request、有序 discovery rows（含 legacy source_version + SourceGuardV2 kind/hash）、归一化 discovery issues、完整 normalized scanner profile v2、report_mode、engine build、实际可达 route-stack worker contract/version/build、Python session capability/contract（或 one-shot marker）、classifier contract/build/profile。以 domain-separated SHA-256 建索引，命中后还必须逐字节比较 `snapshot_key_json`，不能只信 hash。

**worker 身份**：**保留 live worker handshake**（成本小、保留 worker 可用性语义）；快照只跳过 classification/parse lookup 与执行、Context 重算和新 artifact payload 写入，仍必须写当前 run audit/decision/context metadata 并完成 terminal finalization。跳过 handshake 需 release manifest 身份，标为后续。

现有 `begin_run` 对同 request_id terminal row 的 idempotent replay 语义保持不变：它直接返回原 Envelope，不创建当前 run、不做 live handshake，也不计 `snapshot_hit`。本节 snapshot 只指**新 request/current run** 跨 request 复用 artifact，二者的时延与审计不得混报。

**快照资格**：仅 EngineStatus::Ok，且 warnings 为空、无 runtime NotParsed、Error、Timeout、unknown/error 分类、安全 deadline，worker/provenance 完整，ContextBudgetModel 不变量通过。正常 semantic/policy NotParsed 允许存在。任何带 warning 的 Success、所有 Partial 都可以保存不可复用 payload artifact，但永不获得 snapshot key，也不能覆盖既有可复用 artifact；Error 无 artifact。

### Part 6：温扫目标与三段实测

- 所有性能阈值只使用 harness monotonic `benchmark_wall_ms`：从 Python client 即将 `CreateProcessW` 启动 scanner executable、且尚未写 stdin request 的时刻开始，到 child 退出、stdout/stderr framing、exit code 与 strict response schema 全部校验完成后结束；不包含启动 benchmark harness 自身。它包含 store open/begin、handshake、discovery、cache、context、terminal commit、post-commit opportunistic GC 与 response transport。`ContextSummary.total_duration_ms` 和各 stage metric 只用于拆分诊断，不能替代 pass/fail timer；`deadline_precommit_elapsed_ms` 只证明 COMMIT 前检查点，不能替代 wall timer 证明 commit/transport。
- benchmark 固定分别记录可持久重放的 `worker_handshake_ms`、`discovery_ms`、`snapshot_lookup_ms`、`current_run_audit_write_ms`、`terminal_precommit_ms`、`deadline_precommit_elapsed_ms`、`envelope_rebuild_ms`；不得把 discovery、handshake 或 terminal precommit 隐藏到其他阶段。post-commit GC/COMMIT 不虚构独立精确 persisted timer，但其成本不可从 `benchmark_wall_ms` 扣除。
- 在任何生产流程改动前，先做一个 timer-only harness 变更，并对**未改 scanner binary/build**的 7d cold/parse-cache-warm 各跑 3 次，保存同口径 `benchmark_wall_ms` 基线、child SHA/build、硬件和 source count。现有 347ms 是 engine summary，不能拿来推导 wall-clock；这个校准门禁不改变下面冻结的 330/400ms 产品阈值。若未改 scanner 的 process/transport floor 或新增必需 handshakes 已证明阈值物理不可达，本 spec 继续保持 Needs revision，先重新决策目标，禁止在实施中跳过 discovery/validation 或事后换 timer。
- 7d 小窗口固定为 2026-08-02..2026-08-08，并在 timer baseline 时冻结匿名 discovery/source-version manifest hash 与 source count；后续内容漂移必须像 Part 9 一样先更新 manifest 并人工批准，不能用不同 corpus 比前后。一个成功 cold run 后，在同一隔离 DB 用三个全新 request_id 连续运行 3 次 snapshot warm，除 request_id 外 logical request 完全相同。通过条件：每个 request_id 在运行前经 DB 查询证明不存在、每次创建互异的新 scan_run_id（这两项共同定义 harness-derived `idempotent_replay=false`）、median ≤330ms、max ≤400ms、3 次均 `snapshot_hit=true`、context hash 与 cold 完全一致。
- 330/400ms 是冻结的产品预算，不再声称由旧 347ms 同口径推导，也不保留“≤300ms 或相对比例，实施后再定”的开放选择。
- 30d/90d 不给 scanner 增加隐藏的“禁用 snapshot”开关。先从一次成功 cold run 生成 cache-only seed：scanner 退出、源 DB checkpoint/close 后，preparer **只读源 DB并复制**到本次 harness 新建的临时目录；绝不原地清理 cold/用户 DB。marker sidecar 固定记录 canonical harness root、canonical clone path、随机 nonce 与复制前源 SHA-256；preparer 重新 resolve 两个路径，要求 clone 是 root 的普通文件后代、不是 reparse point、与当前配置/default DB 不同且 nonce 匹配，任一不符 fail closed。随后 repository-owned schema-aware preparer 才在 clone 中删除 run/attempt/diagnostic/current-audit/context-run/artifact/lease rows，保留并逐 key 校验 inventory、parse cache 与 classification cache；要求 `integrity_check=ok`、run/artifact/lease count=0、两类 cache count/hash 与 cold 后一致，关闭连接并保存 seed SHA-256。
- parse/classification-cache warm 的 3 个样本各从同一只读 seed 克隆一个新 DB，每个 clone 只运行一次同一 logical request + 全新 request_id，并断言 `snapshot_hit=false`、两类 lookup_count>0/all_hit=true；snapshot warm 则在另一隔离 DB 完成一次 cold 后，用 3 个全新 request_id 连续运行。所有样本必须创建新 scan_run_id；snapshot warm median 至少比 cache-only warm 改善 20%，且 `final_context`/decisions/semantic counts 完全一致。benchmark preparer/marker/seed hash 进入证据，但不进入 production binary、profile 或 wire。
- 性能 SLA 只适用于 manifest 冻结的 7d/30d/90d source counts 与 Part 9 profile；schema 的 1,000,000-item validation ceiling 只表示可审计/可失败处理上限，不承诺 2,000ms terminal commit。若 discovery 本身回归导致 330ms 不可达，本 spec 不允许通过跳过可信 discovery 来作弊；USN/daemon 仍为另案。

### Part 7：长驻流式 PDF worker session（明确契约版本）

#### 7.1 版本与能力发现

- Office v1 version、Python v1 version、`classifier-version`、`session-version` 在一个 bounded parallel preflight batch 中启动，全部结束后再做交叉 build/contract 校验；不得四次串行 spawn 拉高 warm path。每个 handshake timeout 取现有 handshake 上限与 remaining_to_work_deadline 的较小值。profile 不允许 PDF 时可跳过后两项。
- 现有 `version`/`parse`、严格 `WorkerVersionResponse`、worker-side Diagnostic/ErrorCode literals 和 `ai_daily_worker_v1` **逐字段不变**，不添加 capability；Rust Office worker 完全不升版。实现时把当前复用的 Diagnostic Rust/Python type 拆成 wire 形状相同的 frozen `WorkerDiagnosticV1` 与可演进 scanner-side Diagnostic，禁止因 scanner 新 code 扩大旧 worker schema。`WorkerDiagnosticV1` 继续只被旧 worker version/parse 与旧 `ai_daily_transport` 使用；adapter 在进程 seam 将它显式翻译成 scanner-side Diagnostic。
- 并行 batch 可以同时 spawn，但逻辑校验顺序固定：只有 Python v1 version 成功后才接受 `classifier-version` 结果，并校验 `ai_daily_pdf_classifier_v1`、classifier build、pypdfium2 version 与 `pdf_text_presence_v1`；one-shot fallback 命令固定为 `classify-pdf`。profile 允许 PDF 时缺失即 preflight error，不以旧的直接 pdfplumber 路径降级。
- `classifier_build` 使用独立 domain-separated SHA-256，输入为冻结 classifier source allowlist、`pdf_text_presence_v1`、`sys.implementation.name`、`platform.python_version()`、`unicodedata.unidata_version`、exact pypdfium2/PDFium native versions 与 target triple；不使用含安装路径/编译时间的 `sys.version`。`classifier-version` 返回严格 `ClassifierVersionResponseV1 {contract=ai_daily_pdf_classifier, protocol_version=1, classifier_contract_version=ai_daily_pdf_classifier_v1, classifier_build, policy_version=pdf_text_presence_v1, python_implementation, python_version, unicode_data_version, pypdfium2_version, pdfium_version, target_triple}`，无其他字段。Python `worker_build` 继续使用现有算法，但其 source allowlist 同步包含新增 worker dispatch/session code。classifier build 改变使 classification cache/snapshot miss；worker build 改变使受影响 parse cache/snapshot miss，二者不能互相代替。
- Python worker 额外提供 `session-version`。同样只有现有 v1 version 成功后才接受其严格 `PythonSessionVersionResponseV1`：contract=`ai_daily_python_session`、protocol_version=1、session_contract_version=`ai_daily_python_session_v1`、worker_build、classifier_build、supported_operations=`[classify_pdf_v1,parse_v1]`。
- 旧 worker 对 `session-version` 返回 exit code 2，且 stdout 是严格 `ai_daily_transport` protocol v1、error_code=`INVALID_REQUEST` 的单个 response frame 时，视为 capability absent，整轮使用 v1 one-shot；其他非零、额外 stdout、坏 JSON 或 build 与 v1 version 不一致均为 handshake failure，不静默降级。
- `session` 启动后的第一条 stdout frame 必须是严格 `PythonSessionHelloV1 {contract=ai_daily_python_session, protocol_version=1, frame=hello, session_contract_version=ai_daily_python_session_v1, worker_build, classifier_build, supported_operations=[classify_pdf_v1,parse_v1]}`，且 build 与 preflight 完全相同；scanner 校验后才发送请求。

新 classifier/session wire 使用独立严格 `PythonOperationDiagnosticV1`，字段仍为 `{error_code,message,retryable,stage,file_path,backend}`，但 error_code allowlist 只允许 `INVALID_REQUEST|PARSER_START_FAILED|PARSER_TIMEOUT|PARSER_INVALID_PAYLOAD|PARSER_FAILED|SOURCE_VERSION_CHANGED|INTERNAL_ERROR`，stage 只允许 `request|parse|process`，message 为 1..4,096 chars，file_path 为 required-nullable absolute path，backend 为 required-nullable 1..1,024 chars；classifier/session adapter 再翻译为 scanner-side Diagnostic。它不是 `WorkerDiagnosticV1`，也不允许 `STAGE_DEADLINE_EXHAUSTED`、cache/projection/aggregate 等 scanner-only code 由 child 伪造。

one-shot `classify-pdf` 的严格 wire 冻结为：

```text
PdfClassifierRequestV1 = {
  contract=ai_daily_pdf_classifier, protocol_version=1, request_id,
  file_path, source_version, max_pages, policy_version=pdf_text_presence_v1
}
PdfClassifierResultV1 = {
  status=text_in_parse_window|no_text_in_parse_window|unknown|error,
  page_count:null|u64, result_examined_pages:null|u64,
  diagnostic:null|PythonOperationDiagnosticV1
}
PdfClassifierResponseV1 = {
  contract=ai_daily_pdf_classifier, protocol_version=1, request_id,
  status=ok|error, result:null|PdfClassifierResultV1,
  error:null|PythonOperationDiagnosticV1
}
```

response `ok` 必须 result 非空/error=null；text/no-text 的 result diagnostic=null 且满足 Part 3 页数不变量，unknown/error 的 diagnostic 非空且 retryable 分别为 true/false。response `error` 必须 result=null/error 非空，只表示 request/operation transport 无法形成 typed result，参与 7.3 的 bounded retry。classifier command 本身永不返回 `not_classified_by_budget`，该状态只由 Scheduler 在未启动 child 时产生。

#### 7.2 NDJSON wire

每行一个严格 JSON object、UTF-8、以 `\n` 结束，禁止 BOM、日志或多余 stdout。请求 envelope 固定为：

```text
{contract=ai_daily_python_session, protocol_version=1, request_id, operation, payload}
operation = classify_pdf_v1 | parse_v1
```

- `parse_v1.payload` 是完整现有 `WorkerParseRequest v1`，session request_id 必须与内嵌 request_id 相同。
- 本 spec 只允许 `parse_v1` session 执行 `pdf_text_v1`；Python Office/SharePoint 路由继续 v1 one-shot，避免把 office transport 改动偷带入范围。
- `classify_pdf_v1.payload` 固定包含 absolute file_path、source_version、max_pages、`pdf_text_presence_v1` policy version。
- response 固定为 `{contract=ai_daily_python_session,protocol_version=1,request_id,operation,status,result,error}` 的 strict tagged union：outer `status=ok` 要求 result 非空/error=null，表示 transport/operation 已完整执行并携带对应 typed result；typed `PdfClassifierResultV1` 的 `unknown/error` 或 frozen `WorkerParseResponseV1.status=error` 仍放在 outer ok 中，是一次完成的 domain result。parse_v1 的 nested WorkerParseResponseV1 request_id/contract/protocol 必须与 outer/request 完全匹配。outer `status=error` 要求 result=null/error=`PythonOperationDiagnosticV1`，只表示 session/transport 失败，才参与本节 transport retry。重复、未知或错配 request_id 视为 protocol corruption。session classifier result 与 one-shot result 逐字段相同，不得出现第二套页数/nullability 语义。
- 每个 session 同时只允许一个 in-flight request；并发通过 session pool 获得，不在单个进程内 multiplex。

上限：hello/request/classification response 每 frame 1 MiB；parse response 沿用现有 `worker_response_capture_limit(request)`；stderr 由独立 reader 持续排空，每个 in-flight request（含启动 hello）累计最多 1 MiB。任一 frame/stderr 超限、非 UTF-8、EOF 半帧、非预期 stdout 或 reader 阻塞都杀 session。

#### 7.3 生命周期、timeout 与 fallback

- 默认 `session_concurrency=min(max_workers,4)`，允许 1..8；`max_requests_per_session=128`、idle TTL=30s、RSS recycle threshold=512 MiB。每个 session/one-shot child 各自放入独立 Windows Job Object，杀一个超时请求不得连带杀掉 pool 中其他 session。达到任一 recycle 条件时只在当前 response 完整接收后优雅重建。
- 每请求保留 worker v1 legacy source-version 前置/后置校验；scanner adapter 另在进程 seam 前后复核 SourceGuardV2。任一不一致都丢弃结果并按 source-changed 语义处理。
- classification 使用 `pdf_classification_timeout_ms`，parse 使用现有 route per-file timeout；二者都再受 remaining_to_work_deadline 截断。operation timeout 或 total deadline：杀该 child 的 Job Object，当前 operation 对应文件记 Timeout，重建 session；**该 logical operation 不再 one-shot 重试**。
- session start/EOF/protocol corruption/crash：重建并重试当前 logical operation 最多 1 次；第二次仍失败时，仅对 retryable 且非 timeout 的 transport failure 允许对应 `classify-pdf` 或现有 `parse` one-shot 1 次。attempt 上限按 logical operation 计算：`classify_pdf_v1` 最多 3 次，后续 `parse_v1` 另最多 3 次；一个 text PDF 会依次产生两个独立 operation，禁止把两者含混成“单文件共 3 次”。绝无递归 fallback。
- session capability absent 时从第一份文件起使用 `classify-pdf` one-shot + 现有 `parse` one-shot，不计为 degradation；capability 已宣告却运行失败时必须审计，不能把整轮无声切回 one-shot。
- `batch_size` 从配置、profile、文档和测试中删除，唯一寿命计数参数为 `max_requests_per_session`。

### Part 8：scanner profile v2 + schema foundation

#### 8.1 profile wire 与兼容

- `BuildContextRequest` 外层字段形状和 `ContextEnvelope v1` 输出保持不变；`scanner_profile` 成为严格 tagged union：`scanner_profile_v1 | scanner_profile_v2`。
- 新增 `RawScannerProfileV2` / `NormalizedScannerProfileV2`。v1 请求继续接受，但立即使用下表冻结默认值归一化为 v2；数据库、hash、inspect v2 只保存 normalized v2，不再产生新的 normalized v1。
- Raw v2 的新增叶子允许省略并使用 report_mode 默认表，但和 v1 一样拒绝显式 null/unknown field；Normalized v2 的全部叶子必填且使用 canonical field/order/set 校验。
- Python `extract_scanner_profile` 的选择规则固定：显式配置出现任一 v2-only leaf 时输出 `schema_version=scanner_profile_v2`，否则继续输出 v1；`schema_version` 仍由 extractor 生成，不作为 Dynaconf 用户叶子。Raw v2 是 v1 叶子的严格超集，禁止拆成第二套 settings block。无论输入 v1/v2，Rust 都是默认值与 normalized v2 的唯一所有者。
- v2 新增字段及范围：

| 字段 | 范围/默认来源 | identity 作用 |
|---|---|---|
| max_candidate_files | 1..1,000,000 | semantic + snapshot |
| max_pdf_text_extractions | 0..100,000 | semantic + snapshot |
| max_total_pdf_classification_pages | 0..10,000,000 | semantic + snapshot |
| admission_policy_version=`budget_admission_v2` | 常量 | semantic + snapshot |
| classifier_policy_version=`pdf_text_presence_v1` | 常量 | classification cache + snapshot |
| pdf_classification_timeout_ms | 100..60,000；默认 2,000 | execution provenance + classification cache + snapshot |
| total_deadline_ms | 5,000..3,600,000（含固定 2,000ms finalization reserve） | execution provenance + snapshot；不进 parse cache |
| session_concurrency | 1..8 | execution provenance + snapshot |
| max_requests_per_session | 1..10,000 | execution provenance + snapshot |
| session_idle_ttl_ms | 1,000..600,000 | execution provenance + snapshot |
| session_rss_limit_bytes | 64 MiB..8 GiB | execution provenance + snapshot |

v1→v2 默认映射：

| report_mode | max_candidate_files | PDF classification pages | PDF text extractions | total deadline |
|---|---:|---:|---:|---:|
| daily | 96 | 80 | 8 | 10,000ms |
| weekly | 192 | 100 | 12 | 15,000ms |
| monthly | 384 | 370 | 16 | 25,000ms |

session 默认统一为 `min(max_workers,4)`、128 requests、30,000ms idle、512 MiB RSS；`pdf_classification_timeout_ms` 对三种 report mode 都默认为 2,000ms。现有 PDF 页数归一化保持兼容：daily 的 `pdf_max_pages` 默认 5，weekly/monthly 的 `summary_pdf_max_pages` 默认 2；Part 9 的 monthly 真实目录 acceptance 显式设置 Raw v2 `summary_pdf_max_pages=5`，并断言 normalized `parse.pdf.max_pages=5`。改变窗口页数会改变 classification profile/hash。`priority_policy_version` 升为 `budget_nominal_v2`，`compression_policy_version` 升为 `markdown_context_v2`。

`max_candidate_files=1,000,000` 等范围上界是防溢出的 wire validation ceiling，不是默认 deadline 内的吞吐承诺；独立的 engine-owned `MAX_SOURCE_FILES_PER_RUN=1,000,000` 保证完整 discovery/current audit 不超过 contract collection 上限，并由 VersionResponseV2 回显。normalized validation 只保证数值/交叉字段合法。实现仍必须能对合法极端输入有界地 rollback/Abandoned、保留 lease recoverability，并让 Inspect/maintenance 诊断，而不是预分配无界内存或延长 `total_deadline_ms`。只有 Part 6/9 manifest 规模受性能 SLA 约束。

parse-cache identity 只包含会改变单文件 parser 内容/状态的 route profile、per-file timeout、worker build、legacy source version 与 SourceGuardV2；candidate/global context quota、session 生命周期和 total deadline 不污染已成功的单文件内容 cache。snapshot identity 使用**完整 normalized v2 + SourceGuardV2 discovery rows**。

#### 8.2 一次性 schema foundation

同一个 user_version migration 一次加入：Part 2 reason/status 约束、classification cache、artifact/files/decisions、current snapshot audit status/diagnostic、envelope metadata、`audit_provenance_version`、cache access/size/source-guard 字段、inspect v2 所需 metrics，以及 `schema_migration_history`。该表每个 user_version 仅一行，v2 行固定保存 `origin=created_empty|upgraded_v1`、nullable `upgrade_request_id`（upgraded_v1 必填、created_empty 必须为空）、engine_build、committed_at_ms；升级路径在**同一个 v1→v2 transaction** 中插入，作为迁移事实源。Rust/Python scanner-side ErrorCode 同步加入 `STAGE_DEADLINE_EXHAUSTED`、`BUDGET_MODEL_MISMATCH`、`CONTEXT_FIXED_SECTIONS_OVER_BUDGET`、`PROFILE_ROUTE_INVARIANT`、`SOURCE_FILE_LIMIT_EXCEEDED`、`SOURCE_GUARD_UNAVAILABLE`、`MAINTENANCE_MODE_UNAVAILABLE`、`SCHEMA_UPGRADE_REQUIRED`、`SCHEMA_MIGRATION_FAILED`、`DIAGNOSTICS_AGGREGATED`、`SNAPSHOT_REUSE_PROJECTED_AS_FRESH`、`PARSE_CACHE_NOT_APPLICABLE_PROJECTED_AS_MISS`、`CACHE_MISS_REASON_PROJECTED_AS_NEW_FILE`、`SOURCE_GUARD_NOT_PROJECTED`、`INSPECT_V2_PROVENANCE_UNAVAILABLE`；scanner-side DiagnosticStage 新增 `maintenance`。frozen `WorkerDiagnosticV1` 的 ErrorCode/DiagnosticStage sets 均不变。不得在后续 Task 再补一张本 spec 已知必需的表或先用自由字符串占位。

旧 v1 terminal rows 的迁移规则：事务内解析并 validate `final_envelope_json`，Success/Partial context 抽取到 payload artifact，正文从 metadata JSON 移除；旧 Envelope 的 warnings 原样保留，不用新 257 条运行规范改写历史 replay；对应 scan run 固定 `audit_provenance_version=migrated_v1`。由于旧行缺少 v2 provenance，**所有迁移 artifact** 均标记 `snapshot_eligible=false`，Inspect v2 按 Part 5 fail closed；新建 run 固定 `full_v2`。任何一行无法解析则整次迁移失败并保持旧 user_version。

v1 parse cache 没有 SourceGuardV2，不能安全投影；upgrade audit 先只读计数，apply 在同一个 v1→v2 transaction 中删除全部 legacy parse-cache rows，再迁移 inventory（guard kind/hash 暂为空，待下次 full-v2 discovery upsert）。transaction 失败则删除与 schema 变更一起回滚；成功 response 必须回显 detected/invalidated 等量。cache 可从 source 重建，不把旧 cache 偷装成新 identity。

#### 8.3 升级、回滚与维护

schema 升级与普通业务 open 严格分开。新增 `upgrade-db` command；两个 deny-unknown wire 冻结为：

```text
UpgradeDatabaseRequestV1 = {
  contract=ai_daily_scanner_upgrade, protocol_version=1, request_id,
  scan_db_path, apply
}
UpgradeDatabaseResponseV1 = {
  contract=ai_daily_scanner_upgrade, protocol_version=1, request_id,
  status=ok|partial|error,
  source_user_version:null|u64, target_user_version=2, apply,
  schema_migrated, auto_vacuum_converted,
  legacy_parse_cache_rows_detected, invalidated_parse_cache_rows,
  pre_integrity_check=not_run|ok|failed,
  post_integrity_check=not_run|ok|failed,
  warnings, error:null|ScannerDiagnostic
}
```

response warnings 最多 257 条并使用 Part 2 scanner-side projection；upgrade diagnostics 固定 `stage=maintenance,file_path/backend=null`。`ok|partial` 必须 error=null，partial 必须 warnings 非空；error 必须 error 非空。两个 cache row count 为 non-negative u64，`invalidated<=detected`；v1 migration COMMIT 时二者必须相等，未 COMMIT 或 source 已是 v2 时 invalidated=0。

- `apply=false` 是只读 audit：用 read-only connection 校验 source version/schema/integrity、无 live lease，并统计 legacy parse cache，但不取得写 lease、不迁移；成功时两个 mutation bool=false、`invalidated_parse_cache_rows=0`、post=not_run、error=null。audit 结果不授权后续写入，`apply=true` 必须用新 request_id 并重新执行全部检查。
- `apply=true` 是唯一生产升级入口：先取得独占 lease，再调用私有 `open_for_upgrade`；它只配置 connection、重验 user_version/v1 schema，**不得调用自动 migrate**，直接执行 v1→v2 transaction。普通 `ScannerStore::open`/build-context/inspect 发现 v1 一律返回非重试 `SCHEMA_UPGRADE_REQUIRED`，不得自动转交或边启动边迁移。新建空 DB 可由普通 open 直接创建 v2。
- **工具不内置备份**（内置备份机制太重，本版删除）。回滚由运维侧承担：`apply` 前运维必须自行保留一份可恢复的升级前 DB 副本（复制 DB 及其 WAL/shm，或使用既有部署快照），工具不校验该副本。回滚应用时必须同时恢复该升级前 DB，会丢失升级后新增 runs，发布说明必须显式确认。旧 release 直接打开 v2 DB 继续 fail closed 为 `TooNew`。
- response 字段报告事实而非目标：只有连 DB header/user_version 都无法读取时 source version 才为 null、pre/post 都为 not_run；一旦可读就必须回显实值。`schema_migrated=true` 只表示 v2 transaction 已 COMMIT；post schema check/migration 失败用 `SCHEMA_MIGRATION_FAILED`。migration 与 post check 成功、auto_vacuum conversion 失败时返回 `partial`、error=null、warning 非空、`schema_migrated=true/auto_vacuum_converted=false`；两者都成功才为 `ok`。`status=error` 必须 error 非空并保留实际 mutation 字段。source 已为 v2 时 audit/apply 均为幂等 ok、无 mutation。TooNew 始终 fail closed。
- 迁移事务的原子性承担恢复安全：v1→v2 transaction 失败即整体回滚并保持旧 user_version（`schema_migration_history` 记录 upgrade request/engine build）；成功后旧 release 因 `TooNew` 不可读。运维侧升级前副本是唯一回滚手段。
- 实现与测试阶段只对临时 DB/副本调用 apply；真实配置 DB 的 audit 与 apply 是两个独立、需另行授权的发布操作。
- migration 提交后再次 integrity check；随后独立执行“设置 `auto_vacuum=INCREMENTAL` + full `VACUUM`”物理转换，不纳入 migration 事务。转换失败时 v2 业务 schema 仍有效，但 deployment/doctor 明确报告 incremental vacuum unavailable；只有成功后才能把 incremental maintenance 作为可用能力验收。

### Part 9：性能门禁（反作弊）与真实目录手工 acceptance

#### 9.1 fixed corpus 自动门禁

repository-local sanitized corpus manifest 冻结 discovery rows、classification truth、nominal rank、ClassificationPlan、ContentAdmissionPlan、included/omitted/reason 集合和 final_context SHA-256。parse cache 与 classification cache 分别取 `empty/randomized-partial/full`，覆盖 3×3 九种组合；partial subset/seed 固定写入 manifest。**每个组合都使用独立新 DB**，只按 manifest 预种该组合，运行前断言 artifact/run 表为空，因此正常 snapshot lookup 必须 miss；不使用 bypass 开关，也不沿用上一样本刚写入的 cache。其中 empty/empty 是 cold。snapshot 另在一个成功 cold run 后按 Part 6 测试。九种组合均须：

- 无 deadline 时九种 cache 组合的 semantic output 完全一致；
- `text_pdf_coverage = 成功提取或 parse-cache 命中的 admitted text PDF / ContentAdmissionPlan 中 admitted text PDF = 100%`（分母为 0 时按 100% 记，并单列 count=0）；
- 只有 manifest 指定的 semantic/policy NotParsed；
- safety guard 未触发，`pdfplumber_invocations` 等于获得 extraction slot 的 PDF cache misses，no-text 必须为 0；
- Part 3 classifier 数值门禁独立全绿。

#### 9.2 真实目录手工 acceptance（非 CI）

固定同一台机器、release build、`D:\01- 工作`、report_mode=monthly、RawScannerProfileV2 `summary_pdf_max_pages=5`（normalized `parse.pdf.max_pages=5`）：

| 场景 | 日期 | profile override | cold 目标 |
|---|---|---|---|
| 30d | 2026-07-10..2026-08-08 | `max_candidate_files=384; max_total_pdf_classification_pages=370; max_pdf_text_extractions=16; total_deadline_ms=25000` | median ≤20s，max ≤25s |
| 90d | 2026-05-11..2026-08-08 | `max_candidate_files=600; max_total_pdf_classification_pages=800; max_pdf_text_extractions=32; total_deadline_ms=45000` | median ≤40s，max ≤50s |

“cold”唯一含义：每个样本使用一个新建的隔离 DB，确认 parse/classification/artifact/run 表全空，并重启 scanner/Python worker 进程；不尝试清 Windows OS page cache，但必须在证据中声明。每个场景运行 3 个独立 cold DB，报告全部样本、median 和 max。

每个样本还必须满足：

- `stage_deadline_exhausted_count == 0`，无 runtime NotParsed、unknown、Error、Timeout；
- golden aggregate 的 admitted/classified/extracted/included/reason counts 一致；若真实目录内容在复评前变化，先生成只含匿名 hash/count 的新 acceptance manifest 并人工批准，不能放宽门槛；
- `text_pdf_coverage=100%`，no-text `pdfplumber_invocations=0`；
- v2 evidence 记录 normalized profile JSON/hash、engine/worker/classifier builds、source_guard policy/kind counts/content-hash bytes/unavailable count、session 参数、cache state、所有 quota nominal/actual counts、各阶段耗时和 peak RSS；source_guard_unavailable_count=0，session capability 必须 present，fallback_count=0。

随后按 Part 6 做 warm/snapshot 对比。证据只提交聚合值、匿名 corpus hash 与硬件/build 信息，禁止真实路径、文件名、正文或可逆映射。

### Part 10：依赖与发布

- 首个生产 import 与同一 commit 执行 `uv add "pypdfium2>=5,<6"`；不得继续依赖 pdfplumber 的传递解析结果。`pyproject.toml`、`uv.lock`、`requirements.lock` 和 Python worker build allowlist/fingerprint 同步更新。
- `DoctorResponse v1` 形状不变，但 doctor 用独立 read-only schema probe：v1 DB 返回命名 check `schema_upgrade_required=error` 并提示先运行 `upgrade-db apply=false`，绝不借 doctor 迁移；TooNew 同样 error。profile 允许 PDF 时新增 checks：classifier contract/build/pypdfium2 不匹配为 error；session capability absent 在普通 doctor 为 warning、在 Python `doctor --strict` 门禁中为失败。对 v2 DB 另检查 `PRAGMA auto_vacuum=INCREMENTAL`：普通 doctor 为 warning、strict 为失败，并提示升级库未做物理转换时 incremental maintenance 受限（见 8.3）。这样 one-shot 仍是运行时 correctness fallback，生产性能验收不会误把 capability absent 或 vacuum no-op 当健康。
- `requirements.lock` 的生成工具链冻结为 `uv 0.12.0`；升级 uv 必须在独立变更中重生成并审查投影。唯一生成命令为：

```powershell
uv export --frozen --no-dev --no-emit-project --no-header --format requirements.txt --output-file requirements.lock
```

不使用 `--no-hashes`，不包含 dev group 或 editable project；`--no-header` 消除临时输出路径导致的伪 diff。CI 用 uv 0.12.0 导出到临时文件并逐字节比较 tracked lock，再在 Windows 执行 `python -m pip install --requirement requirements.lock`、worker handshake、doctor 和 fixed corpus；版本/命令改变必须单独修改本规范与发布文档。

### 保持冻结（不修改）

- `ContextEnvelope` 字段集合、`contract/protocol_version=1` 与 required/nullability 形状；本 spec 只纠正计数/action 语义并显式扩展 **scanner-side** ErrorCode literals 与 `maintenance` DiagnosticStage。enum 扩展要求 Rust/Python context strict models 在同一 release lockstep 更新，旧 context client 不具备 forward compatibility；frozen worker v1 使用独立 WorkerDiagnosticV1，不受扩展影响。这与 Part 8 已要求“旧 release 回滚必须恢复 v1 DB”一致。`InspectRunResponseV2` 是独立观测 interface，不向 Envelope 塞字段。
- 默认 scanner `VersionResponse v1` 的字段与四项 command 投影不变；新增能力只由 `version --response-version 2` 发布。
- 报告 schema、模板内容、LLM 调用次数与 provider 行为。
- office backend 与 fallback policy。
- scanner 权限边界、Python 侧 `report_runner`/`context_scheduler`/`rust_context_client` 接口。
- 共享 `ai_daily_worker_v1` 的 version/parse wire；Python session 使用独立命令和独立 contract，Office worker 不升版。
- **OCR / USN / daemon**：明确出范围。

## Testing

### BudgetedContextScheduler interface 测试

- nominal rank table 与完整 tie-break golden；Error/Timeout 不改变位置；1,000,000 source 正常形成 snapshot，1,000,001 在 inventory 前以 `SOURCE_FILE_LIMIT_EXCEEDED`/零 summary fail closed。
- SourceGuardV2：Windows file-id/change-time 与 Unix inode/ctime canonical hash fixture、metadata API unavailable 时完整 SHA-256 fallback、同 size+mtime 内容替换必然 cache/snapshot miss、pre/post guard 改变丢弃结果、guard 全不可用固定 Error；upgrade audit/apply 的 legacy cache detected/invalidated count 等式、migration rollback 不删 cache，commit 后 cache 清空且 inventory guard 待 full-v2 upsert。bytes/count metrics 与 v1 lossy projection warning 勾稽。
- empty/random/full parse cache + empty/random/full classification cache 的组合测试：ClassificationPlan、ContentAdmissionPlan、decisions、semantic summary、context hash 完全一致。
- nominal charge：cache hit 不返还 candidate/page/extraction slot；并发 completion 任意置换不改变计划。
- ContextBudgetModel 对每种 renderer section 做 property test：`rendered_chars <= reserved_chars`；故障注入触发非 panic 的 `BUDGET_MODEL_MISMATCH` Error。
- 0/1/数千/1,000,000 个 omitted 文件验证 20% total summary reservation、worst-case detail slot 不回填、reason+extension/catch-all 聚合、固定 section overflow 与全局字符上限。
- source disposition、五种 PDF classification result、非 PDF/pre-classification reject/source-guard-unavailable/runtime-before-start/source-version-discard 五类 null audit、四种 ParseStatus、三类 NotParsed reason、计数等式逐项覆盖。
- 0/256/257/100,000 条 new-run warning 验证 bounded projection：最多 256 detail + 1 `DIAGNOSTICS_AGGREGATED`，group message≤4,096 chars；失败文件的 final diagnostic 仍可从 Inspect v2 审计；migrated v1 warning replay 另测原样不聚合。
- parser/classifier provenance matrix：parse snapshot/fresh/miss/not_applicable 对 backend/lane/miss reason/transport/attempt/duration/final Diagnostic 逐格验证；lookup miss 后 worker 未启动的 runtime NotParsed 为 not_applicable，但 lookup_count/all_hit 仍诚实；parser backend fallback 字段不接收 session transport fallback。PdfClassificationAuditV1 对 fresh/miss/snapshot/not_eligible 与五态逐格验证 page/result/run/nominal/duration/transport/attempt；cache/snapshot 的 run pages=0，timeout/crash 无 typed result 时 page/result/run pages=null，任一不可观测 attempt 后 retry 成功时 result pages 已知但 run pages 仍为 null；in-flight timeout 保留实际启动 lane。
- fake clock 在 classification/parse/context 前后触发 deadline，逐项断言 queued runtime NotParsed、in-flight Timeout、Partial/Error、Diagnostic severity、cache commit、不可复用 payload 与 snapshot 禁止规则。
- cache transaction 与 terminal transaction 故障注入：prepare_inventory 多 batch 只在全量完成后开放 lookup，prefix 不冒充 current audit；cache COMMIT 成功即 receipt 权威，跨过 work deadline 后停止新 batch；已提交 receipt 不被 finalize rollback 撤销。post-begin pre-outcome failure 与 transaction 前 post-outcome invariant failure 都提交 IDs=当前 run 的最小空 Error；transaction 已打开后的失败不二次覆盖，terminal COMMIT 未发生时 abandon/lease 原子 cleanup，cleanup 失败由 TTL reclaim 且不产生 running-without-lease。
- 1/1,000/1,000,000 audit-row synthetic outcome 验证事务外 canonicalization、bounded batch deadline check 与回滚；后两者不是 2,000ms SLA，超时必须 Abandoned/可 reclaim，不能部分 terminal commit 或无界预分配。
- snapshot miss/hit：artifact 只存一次正文；当前 rows 为 snapshot/0ms；命中后删除最初 source run 时由当前 Success run 继续充当 source；删除全部引用但暂不 GC 时 lookup 必须 miss，重算可逐字段 dedup 复活；最后 orphan 可 GC。
- test `CachePort` 返回 snapshot miss 后重算同 key：逐字段相同则 dedup 引用原 artifact 且 `snapshot_hit=false/reused_from=null`；相同 key 但任一 semantic row/hash 不同则 fail closed，不覆盖。该 fault seam 不暴露为 production CLI/profile 开关。
- 两类 cache exact/identity/source/evicted/new 五分支与 reason 优先级、v1 lossy reason projection、固定 eviction tuple/rank/tie-break、stale-generation 判定、cache/artifact/audit hard cap、单 entry/current audit 超 cap、pinned artifact、同日 hit 批量更新、写入空间不足 warning 均不改变 semantic output；所有 persisted logical size 全量重算一致。
- MaintenanceRequest/ResponseV1 对字段顺序/required-nullability、每个 mode、dry_run before=after/deleted=0/after_complete=true、pre-integrity fail 零写、partial failure 实际 counts、auto_vacuum none 时 incremental 明确失败、status/error/integrity/vacuum 不变量与独占 lease 做 strict fixture。fault-inject GC transaction 与 vacuum 的任一失败点：均不得进入后续 mutation phase，deleted/before/after 报告真实部分进展；已提交 GC 不回滚。

### Contract/schema 测试

- Rust/Python `scanner_profile_v1|v2` union、v1→v2 默认映射与 canonical hash fixture 一致。
- `ContextEnvelope v1` fixture 不增字段，新增 scanner-side ErrorCode/maintenance stage 的 Rust/Python schema 同步；WorkerDiagnosticV1 ErrorCode/DiagnosticStage sets 做 exact frozen fixture；PythonOperationDiagnosticV1 做独立 exact allowlist/translation fixture；scanner Version v1/V2、Inspect v1 lossy warning、Inspect v2 完整 provenance、Maintenance 与 UpgradeDatabase request/response 均做跨语言 fixture。
- `version/parse ai_daily_worker_v1` 的字段、枚举、required/nullability 与校验 fixture 不变；新增 dispatch source 会合法改变 `worker_build` 值，golden 更新只能触及该 identity 值及由它派生的 hash，不能加 capability/字段。`classifier-version/classify-pdf` one-shot、`session-version`、hello、classify/parse frame、typed-result/outer-error nullability、错配 ID、超长 frame、EOF、timeout 和每 logical operation 最多三次 attempt 单独测试。
- `upgrade-db apply=false` 零文件/零 DB mutation，普通 open/build-context 对 v1 返回 `SCHEMA_UPGRADE_REQUIRED`；apply=true 才能进入 `open_for_upgrade`，且先做只读 audit。覆盖 v1→v2、已是 v2 幂等、TooNew、坏 envelope 全量回滚保持旧 user_version、migration committed + vacuum partial、response 事实字段与真实配置 DB 路径拒绝；迁移事务失败不产生半迁移状态，migration 不重复。migrated_v1 默认 inspect/replay 可用，而 v2 inspect 固定返回 `INSPECT_V2_PROVENANCE_UNAVAILABLE`，不得伪造 execution 字段。

### 门禁

- timer-only harness 先在未改 scanner binary/build 上产出同口径 wall-clock baseline；fixture sleep/process test 证明 timer 覆盖 child spawn、response validation，且不读取 `ContextSummary.total_duration_ms` 作为 pass/fail。
- cache-only benchmark preparer 只读 cold source 并只修改 marker 指向的普通 clone；路径逃逸/reparse/nonce 错配/default 或用户 DB 均 fail closed，故障注入后 cold source hash 不变。删 run/artifact 后保持两类 cache key/hash 不变、integrity=ok、seed hash 稳定；三个 sample clone 均证明 snapshot=false、cache all-hit。
- fixed-corpus cold/warm 门禁全绿。
- PDF 分类门禁（Part 3 数值）与提取替换门禁独立存档。
- 真实目录手工 acceptance（Part 9 反作弊条件）。
- tracked `requirements.lock` 与冻结 uv export 命令逐字节一致。

### 验证命令（Windows）

```powershell
uv run pytest
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
uv run python main.py doctor --strict
uv lock --check
New-Item -ItemType Directory -Force .artifacts | Out-Null
uv export --frozen --no-dev --no-emit-project --no-header --format requirements.txt --output-file .artifacts\requirements.generated.lock
git diff --no-index -- requirements.lock .artifacts\requirements.generated.lock
git diff --check
```

## Acceptance Criteria

- [ ] timer-only harness 先对未改 scanner binary/build 建立 7d 同口径 wall-clock baseline；证据明确旧 347ms 仅为 engine summary，所有 pass/fail 只读 `benchmark_wall_ms`。
- [ ] 30d 三次 cold median ≤20s/max ≤25s；90d median ≤40s/max ≤50s；每次 `stage_deadline_exhausted_count=0` 且无 runtime NotParsed/Error/Timeout/unknown。
- [ ] 7d snapshot warm 三次 median ≤330ms/max ≤400ms；30d/90d snapshot warm 比 parse/classification-cache-only warm median 至少改善 20%；baseline 由带 marker 的隔离 seed DB clone 生成，不存在 production snapshot bypass。
- [ ] ClassificationPlan/ContentAdmissionPlan 在 empty/random/full 两类 cache 的所有组合下完全一致；cache hit 使用相同 nominal charge，并发完成顺序不影响结果。
- [ ] SourceGuardV2 在 legacy mtime+size 未变的内容替换上仍强制 cache/snapshot miss；metadata/full-hash/unavailable 三路径、pre/post guard、upgrade detected/invalidated legacy-cache 等式、FileAuditV2 与 guard I/O metrics 全部可审计，性能门禁不得关闭 guard。
- [ ] nominal rank、source disposition、PDF 五态、PdfClassificationAuditV1 的 page/result/run/nominal/cache/transport/attempt nullability、ParseStatus/action/Diagnostic/run status/快照资格完全符合 Part 1–3；不可观测页数为 null，不伪造 0。
- [ ] 所有计数等式成立；NotParsed 不进入 error metric，Success 不再在 Compressor 中正常 Omit；source file ceiling 与 Version v2 回显一致，超限不伪装成 semantic quota。
- [ ] `rendered_chars <= reserved_chars` property gate 全绿；`BUDGET_MODEL_MISMATCH` 返回非重试 Internal Error 且不 panic。
- [ ] 大规模 omitted 集合的 detail + group + catch-all 总和不超过 20% reservation，预选 detail 不回填；`file_context` 永不因 footer 自身超过 global budget。
- [ ] classifier corpus 达到样本下限，text false-negative=0、no-text false-positive≤0.1%、valid fixture unknown/error=0；classifier identity 含 Python/Unicode DB/pypdfium2/PDFium/target，任一改变都使 classification cache miss；no-text `pdfplumber_invocations=0`。
- [ ] snapshot artifact 只存一份正文；`scan_runs` 不再嵌入 file_context；当前 audit 为 snapshot/0ms；仍有 Success 引用时最初 source run 可 GC，零引用 orphan 不冒充 snapshot hit 且可回收/重算 dedup 复活。
- [ ] full_v2 run 的 InspectRunResponseV2 能审计 classification/quota/deadline/cache miss reason/session/artifact/finalization；parse provenance 矩阵、execution metric 类型/nullability/逐项统计口径、miss-reason tree 与 all-hit lookup-count 不变量全部通过。migrated_v1 run 的默认 v1 replay 可用且 v2 明确 fail closed；ContextEnvelope v1 与 worker version/parse v1 fixture 不增字段。
- [ ] scanner VersionResponse v1 投影逐字段不变；VersionResponseV2 按冻结字段唯一发布 maintenance/upgrade/profile/inspect/classifier/session 新能力，cache_retention_policy 与 MaintenanceResponseV1 完全一致。
- [ ] frozen WorkerDiagnosticV1、scanner-side Diagnostic、PythonOperationDiagnosticV1 三者的 allowlist/translation 不串线；`classifier-version/classify-pdf` 与 `session-version/session ai_daily_python_session_v1` 的逐 operation timeout、独立 Job Object kill、source-version 双检、classify/parse 各自 attempt≤3 和 capability-absent one-shot 全部通过。
- [ ] Raw/Normalized scanner profile v2 与 v1→v2 映射落地（含 2,000ms classifier timeout、daily=5/weekly-monthly=2 的现有 PDF 页数默认）；完整 v2 + SourceGuardV2 进入 snapshot key，成功单文件 cache 只含会改变该结果的 identity。
- [ ] v2 schema foundation 一次迁移完成；普通业务 open 对 v1 fail closed，`upgrade-db audit` 零写且与 apply 分开授权，apply 迁移原子回滚保持旧 user_version，回滚由运维保留的升级前 DB 副本承担；新库预设/升级库 post-step 最终均为 auto_vacuum=INCREMENTAL，未转换时 incremental maintenance/strict doctor 明确失败；旧 release 对新 DB 为 TooNew。
- [ ] parse/classification/artifact/audit 全部受硬上限或 retention 约束；pinned 引用不误删，非必需 cache 写失败不改变 context，必需 payload 无空间时 fail closed；WorkDeadline/AbsoluteDeadline 与 cache/terminal COMMIT 线性化不混淆，2,000ms 只作为 tail reserve，极端审计量超时能完整 rollback/Abandoned/reclaim。
- [ ] MaintenanceRequest/ResponseV1 的 gc/incremental/dry-run、auto_vacuum mode、integrity/vacuum/status/error 不变量与独占 lease 全部通过；GC transaction 与 vacuum 任一失败都不改写此前已提交的 terminal result，deleted/before/after 报告真实部分进展；opportunistic GC 无论成功/busy/overshoot 都不改写 terminal result，且全部时间保留在 benchmark wall-clock。
- [ ] pypdfium2 为直接依赖；冻结 uv export 可逐字节再生 requirements.lock，Windows install/worker fingerprint/doctor 通过。
- [ ] fixed corpus、Rust workspace、pytest、release build、doctor 和 diff check 全绿。

## Out of Scope

1. `ContextEnvelope` 字段形状变更。
2. 报告 schema、模板、LLM prompt 与 provider 行为变更。
3. office backend 或 office fallback policy 变更。
4. scanner 权限边界、Python 侧报告接口变更。
5. **OCR**：no-text PDF 内容提取不在本 spec。
6. **USN Journal / 持久目录监视器**：快照前置变化检测不在本 spec。
7. **daemon**：GC 用 finalize 后机会式 + `maintenance` 子命令，不引入常驻进程。
8. 跨载体报告事务、Web/GUI/queue。
9. 当前 run 完全不落 O(N) audit rows 的引用式 inspect 变体；本版仍生成诚实的 current rows，只把不可变语义来源放入 artifact。
10. PDF 正文提取 backend 的替换；本 spec 只增加分类与 session，任何替换仍受独立提取门禁约束。
11. 工具内置 DB 备份与自动恢复；回滚完全由运维保留的升级前 DB 副本承担，工具不校验、不自动清理。

## Implementation Decisions

- **ID-1 单一深 module**：BudgetedContextScheduler 一个外部 interface；预算模型与 Compressor 内聚，Store/parser/classifier 为 adapter。
- **ID-2 两阶段不可变计划**：ClassificationPlan 在分类 I/O 前冻结，ContentAdmissionPlan 在 parse I/O 前冻结；cache 使用 nominal charge。
- **ID-3 完整 nominal rank**：`budget_nominal_v2` 的 path/extension 表和四段 tie-break 是唯一排序实现。
- **ID-4 quota/deadline 分离**：quota 决定正常集合；deadline 只产生规范化 Partial/Error/runtime rows，永无快照。
- **ID-5 唯一 ContextBudgetModel**：renderer 共用精确字符计数，omitted summary 独占 ≤20% reservation、detail 不回填、group 溢出进 catch-all；失败返回 `BUDGET_MODEL_MISMATCH`，不 panic/二次填充。
- **ID-6 完整 disposition/state/count 矩阵**：planner reject、PDF 五态及 nullable page/result/run/nominal 证据、ParseStatus、run status 与计数等式全部冻结。
- **ID-7 PDF 分类**：`pdf_text_presence_v1` 保守判定；仅 text/no-text 正缓存，本版无负缓存，独立数值门禁。
- **ID-8 全类型有界缓存**：昂贵结果始终尝试写入但可淘汰；inventory 前置 upsert、receipt 型独立 cache transaction、固定硬上限、引用保护、日桶访问、10ms opportunistic admission budget + strict maintenance。
- **ID-9 artifact/当前执行证据分离**：正文与不可变语义存一次；artifact/current rows 用完整 FK 闭环，当前 run 保存 snapshot/0ms/0 attempts 等真实证据，Envelope 从 metadata 重建。
- **ID-10 Inspect v2**：ContextEnvelope v1 保持冻结；InspectRunResponseV2/FileAuditV2/execution metrics 严格字段唯一承载新性能/分类/快照/finalization 验收。
- **ID-11 Python classifier/session 独立 contract**：WorkerDiagnosticV1 保持冻结，新 operation 使用独立 Diagnostic；classifier-version/classify-pdf 是必需 one-shot，session-version/session 是可选加速，classify/parse attempt 各自≤3，Office 不升版。
- **ID-12 profile/schema/release 一次演进**：v1 归一化到 v2（保留 daily=5、weekly/monthly=2 页默认）、一次 schema migration、显式 upgrade-db audit/apply（工具不内置备份，回滚靠运维保留升级前副本）、唯一 uv export 命令。
- **ID-13 真实性能与终态适用边界**：pass/fail 只用 process-to-validation wall timer；旧 347ms 不可比，timer-only baseline 先行；2,000ms 是 tail reserve，不是 validation ceiling 的写入 SLA。
- **ID-14 source/audit 硬上限**：完整 source snapshot 最多 1,000,000 行；第 1,000,001 个在 inventory 前以 run-level Error fail closed，不截断、不转 semantic omit。
- **ID-15 双 deadline 与线性化**：WorkDeadline 只停止重工作并构造 Partial/Error，AbsoluteDeadline 决定能否 terminal COMMIT；cache/terminal COMMIT 成功各自为不可反悔的持久化点。
- **ID-16 观测与 benchmark 不设旁路**：FileAuditV2/metrics 逐项冻结；cache-only warm 由隔离 seed DB clone 证明，不向 production binary/profile 暴露 snapshot bypass。
- **ID-17 SourceGuard v2**：worker v1 的 mtime+size wire 保留，但 v2 cache/snapshot 必须再绑定 file-id/change-time 或完整 content SHA-256；guard 不可用 fail closed 为 per-file Error。
- **ID-18 maintenance 不提供整库重写**：删除 full_vacuum，避免无恢复点的 destructive VACUUM；物理收缩仅靠 incremental_vacuum 于已转换库。schema 升级回滚依赖迁移事务原子性 + 运维侧升级前 DB 副本。

## 建议实施顺序

1. 先落 timer-only harness 与 synthetic timer tests，对未改 scanner binary/build 记录 7d 同口径 baseline；若冻结阈值不可达则停止，spec 继续 Needs revision。
2. 落跨语言 contract fixtures：profile v2 union/default、Version/Inspect/Maintenance/Upgrade v2、新/旧 diagnostics、SourceGuard/FileAudit、classifier/session frames、Part 2 reason/status/count；此 Task 不改生产流程。
3. 实现 `upgrade-db` audit/apply、一次性 schema foundation/migration/envelope reconstruction；本阶段只在 fixture/临时副本做迁移验证，普通业务 open 对 v1 fail closed，禁止打开或迁移真实配置 DB。
4. 实现 SourceGuardV2 与纯 nominal rank、ClassificationPlan、ContentAdmissionPlan、ContextBudgetModel renderer/property tests。
5. `uv add pypdfium2`、实现 `classifier-version/classify-pdf` one-shot `pdf_text_presence_v1` classifier、classification cache 与独立质量门禁。
6. 组装 BudgetedContextScheduler，替换 run.rs 中分散状态机；穿过 module interface 完成 cache matrix、deadline 与状态勾稽测试。
7. 实现全类型 cache hard caps、日桶访问、opportunistic GC、独占 maintenance（gc/incremental_vacuum）。
8. 实现 artifact/metadata envelope/当前 audit rows、Inspect v2、隔离 cache-only seed preparer 与 snapshot warm；验证源 run 删除和 orphan 生命周期，确认 production binary/profile 无 snapshot bypass。
9. 实现 Python `session-version/session` 与 session pool；one-shot 仍为 capability absent/有限故障 fallback，不替换 PDF 提取 backend。
10. 使用隔离 DB 运行 fixed corpus、30d/90d 手工 acceptance、warm 对比与 release/requirements projection 全门禁；只在证据全绿后更新状态。
11. 若真实配置 DB 升级属于本次发布范围，先单独执行 `upgrade-db apply=false` 并交付 audit 目标，再经明确授权用新 request_id 执行 apply、strict doctor（运维已保留升级前 DB 副本用于回滚）；未获授权就停在“代码可发布、DB 未迁移”，不得把测试授权外推为生产写入。
12. 批量 SQL/discovery 微调仅在上述 correctness gate 后进行；SQL 必须分块/临时表并保持 cache miss reason 优先级。

## 主要风险与缓解

| 风险 | 缓解 |
|---|---|
| nominal charge 保守导致少分类/少装文件 | cache 不返还名额；记录 nominal/actual 差额，后续只通过 profile version 调参 |
| 同尺寸且保留 mtime 的内容替换复用旧 cache | SourceGuardV2 绑定 file-id/change-time，弱文件系统回退完整 SHA-256；不可用不命中且 per-file fail closed |
| deadline 产生非确定性部分结果或把成功 COMMIT 误报未写入 | Work/Absolute 双 deadline、cache/terminal 线性化点、唯一终态表；正常缓存一致性门禁明确排除 |
| 状态/计数改动破坏消费方 | ContextEnvelope v1 fixture + 所有消费方同步；Inspect v2 承担新增观测 |
| 稀疏/隐藏文字被误判 no-text | 单有效字符保守规则 + false-negative=0 独立门禁 |
| timeout/crash 被误报为 0 个 inspected pages | nullable run/result pages + confirmed total + unobserved attempt count |
| artifact 仍重复正文或伪造当前耗时 | 移除 final_envelope 正文、artifact semantic rows 与 current execution rows 分离 |
| 源 run 被 GC 后快照失效 | artifact-owned rows + source FK SET NULL；只按 artifact 引用计数回收 |
| session 泄漏/协议错帧/无限重试 | 单 in-flight、bounded frame/RSS/request count、Job Object、attempt 上限 3 |
| cache/审计表持续膨胀 | 所有载体硬上限/retention、引用保护、同步腾挪 + 10ms/maintenance GC |
| 性能通过靠少做或 deadline 截断 | exact golden sets、coverage=100%、每个样本无 guard/runtime/error/timeout |
| 旧 engine timer 被冒充新版 wall-clock baseline | timer-only 先行、记录未改 binary/build、pass/fail 禁读 summary timer |
| 为比较 cache warm 偷加 snapshot bypass | 仅克隆带 marker 的隔离 cache-only seed DB，production binary/profile 无旁路 |
| 极端合法审计量被误认为 2 秒写入承诺 | tail reserve 明确非 SLA；事务外准备、batch deadline check、完整 rollback/Abandoned |
| profile v2 与旧调用方冲突 | strict tagged union、冻结 v1→v2 默认、只持久化 normalized v2 |
| 升级后需要回滚而工具无内置备份 | 运维在 apply 前自行保留升级前 DB 副本（DB+WAL/shm 或部署快照）；迁移事务原子回滚；发布说明显式声明回滚会丢失升级后 runs |
| 启动新 binary 时意外迁移真实 DB | 普通 open 对 v1 返回 upgrade required；audit/apply 两次独立命令与授权，开发只动临时副本 |
| maintenance 无整库物理收缩手段（升级库 auto_vacuum=none） | 删除 full_vacuum；incremental_vacuum 明确返回 MAINTENANCE_MODE_UNAVAILABLE；strict doctor 提示未转换限制 |
| 升级/维护后无自动清理 | 工具不创建备份产物，无自动清理问题；临时副本由运维自行管理 |
| requirements.lock 无法再生或混入 dev | 唯一 `uv export --frozen --no-dev --no-emit-project --no-header --format requirements.txt` 命令与 CI byte compare |

## 决策记录

- 2026-08-08 用户授权：重开 scanner DB schema / cache identity 冻结边界，做 v2 迁移。
- 2026-08-08 用户确认：未解析文件 input_chars 用 size 近似。
- 2026-08-08 用户确认：no-text PDF 暂不做 OCR。
- 2026-08-08 用户要求状态保持 Needs revision，并在第三轮复评后授权直接修订 spec。
- 2026-08-08 v4 决定：cache 使用 nominal charge；deadline 终态/事务线性化不留实施选择；omitted summary 使用独立 reservation；共享 worker v1 不增 capability；parser/classifier、artifact/current execution provenance 分离；新增 Inspect v2 作为验收事实源；保留现有 report-mode PDF 页数默认。
- 2026-08-08 v4 收口：PDF 五态字段矩阵对不可观测页数使用 null；Version/Inspect/Maintenance 与 Python operation Diagnostic 严格化；migrated v1 缺 provenance 时 Inspect v2 fail closed；旧 347ms 与新 wall timer 断开，timer-only baseline 先行；2,000ms 明确为 tail reserve 而非百万行写入 SLA。
- 2026-08-08 v4 最终一致性修订：区分 WorkDeadline/AbsoluteDeadline 与两个 COMMIT 线性化点；TerminalFailure 可诚实表达 transaction 前的 post-outcome reject；补齐 parse/metrics 统计口径；删除 production snapshot bypass 设想，改用带 marker 的隔离 cache-only seed DB。
- 2026-08-08 v4 治理补丁：普通业务 open 不再自动迁移 v1；新增严格 `upgrade-db` read-only audit 与 separately authorized apply，开发/acceptance 只使用临时副本。
- 2026-08-08 v4 cache identity 补丁：保留 frozen worker source_version wire，但 cache/snapshot identity 必须额外使用 SourceGuardV2；同 size+mtime 变更不再误命中。
- 2026-08-08 用户决定：工具内置备份机制太重，本版删除；维护只保留 gc/incremental_vacuum（不做 full_vacuum），schema 升级回滚由运维保留的升级前 DB 副本承担。
- v4 完成后状态仍为 Needs revision；只有独立复评确认无新阻断，才可另行改为 Ready for implementation。
