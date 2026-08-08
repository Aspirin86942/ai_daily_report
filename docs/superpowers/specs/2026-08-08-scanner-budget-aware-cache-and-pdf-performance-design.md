# Scanner 预算感知解析 + 缓存策略 + PDF 分类 设计规格（修订版）

> 状态：**Needs revision**（2026-08-08 设计审查后修订；六条阻断点与高优先级项已在本版闭合，待复评后决定是否恢复 Ready for implementation）
> 日期：2026-08-08
> 决策范围：Rust scanner core 的准入计划、解析预算、缓存策略、快照、PDF 分类、worker 会话与 schema 演进
> 首要目标：以真实目录证据驱动，消除「解析大量文件却只进少量上下文」的浪费，把 no-text PDF（扫描件）从昂贵提取路径剔除，并保证**缓存无关确定性**
> 不涉及：Rust CLI JSON wire 字段形状、报告 schema/模板、LLM 行为、office backend 迁移、OCR

## 0. 审查与修订记录

2026-08-08 设计审查发现 6 个阻断点与若干高优先级项，本版逐条闭合：

| # | 阻断/高项 | 本版处置 |
|---|---|---|
| 1 | 温扫 ≤150ms 与 discovery=272ms 矛盾 | 目标改为 **≤300ms**，明确 discovery 为下限；≤150ms 需 USN 前置检测，明确出范围 |
| 2 | 「解析顺序=呈现顺序」前提不成立；cache 状态影响 context | 改为**确定性准入计划**（仅依赖元数据+分类，与 cache 无关），冻结 golden output，新增空/部分/全缓存一致性门禁 |
| 3 | not_parsed+omit+非错误与契约不兼容 | **重定义 not_parsed 语义**，计数等式写入 spec（见 Part 3） |
| 4 | worker/cache 无法表达 PDF 分类 | 分类状态独立于 Context action，新增类型化分类缓存；承认需要 worker 契约演进 |
| 5 | 快照键不足；复用旧 summary 破坏 finalize 校验 | 完整 provenance 键 + `reused_from_context_run_id` + 当前 run 重算耗时 |
| 6 | 批量 list-in/list-out 无法逐文件硬超时 | 改为**长驻流式 NDJSON session**；batch_size 与并发解耦 |
| 7 | 字符预算不保证 40s | 新增总 wall-clock deadline、提取数上限、inspected pages 上限与 `stage_deadline_exhausted` 审计 |
| 8 | 缓存与呈现耦合、无 GC、SQLite freelist | 缓存与 Context inclusion 解耦，有界淘汰 + 独立物理压缩 |
| 9 | 分类门禁 ≠ 提取门禁；pypdfium2 非直接依赖 | 两套独立门禁；生产 import 即声明直接依赖并同步 requirements.lock |
| 10 | schema v2 破坏旧 release 回滚 | 升级前备份 + 发布门禁声明；一次性 schema foundation |

## Problem Statement

### 真实语料与实测证据（2026-08-08，`D:\01- 工作`）

**目录规模与窗口负载**

| 窗口 | 可解析文件 | office/PDF 类 | 解读 |
|---|---:|---:|---|
| 日报 (2d) | 33 | 32 | 冷扫秒级，缓存价值低 |
| 周报 (7d) | 136 | 95 | 冷扫 3.7s |
| 月报 (30d) | 322 | 267 | 冷扫约 1 分钟量级 |
| 90d | 729 | 516 | 冷扫卡死 >5min，PDF 273 |
| 全年 | 1667 | 1565 | 冷扫预计 10min+ |

**PDF 类型分布（pypdfium2 快速分类，1184 个 PDF）**

| 窗口 | PDF 数 | 有文字 | 无文字（扫描件） | no-text 占比 |
|---|---:|---:|---:|---:|
| 7d | 4 | 4 | 0 | 0% |
| 30d | 74 | 16 | **58** | **78%** |
| 90d | 273 | 68 | **205** | **75%** |
| 全年 | 1184 | 403 | **779** | 66% |

**7 天窗口冷/温实测**

| 指标 | 冷扫 | 温扫 |
|---|---:|---:|
| 总耗时 | 3735ms | 347ms |
| discovery | 273ms | 272ms（占 warm 78%） |
| parse | 3387ms（91%） | 0ms（全复用） |
| 进上下文 | **14 / 136（10%）** | — |

单文件 parse 耗时（实测）：PDF **~2543ms/个**（Python worker + pdfplumber）、xlsx ~14ms、docx ~12ms、pptx ~18ms。

### 当前问题（含审查确认的契约现实）

1. **解析浪费**：`run.rs` 对所有 cache miss 文件并行解析，`compressor.rs` 再按全局预算丢弃放不下的。实测 136 解析中 14 进上下文。
2. **no-text PDF 是纯浪费**：月报/季度窗口 75~78% PDF 是扫描件，pdfplumber 提取空文本却花 ~2.5s/个，是 90d 冷扫卡死的直接原因。
3. **缓存与呈现耦合且无淘汰**：当前 `parse_cache` 对每个成功解析文件写内容；`cache_writes` 在 compressor 得出 included 前已生成（`run.rs:632`）；无 GC/体积控制。
4. **呈现顺序与解析顺序都非 path 序**：`decide_files` 按 priority→lower path→path→identity 排序（`decision.rs:122-142`）；compressor 放不下的文件后仍继续尝试更短文件（`compressor.rs:103`）。
5. **NotParsed 无法表达预算省略**：非 Success 一律转 action=Error/reason=parse_error（`decision.rs:87,91-92`），NotParsed 必须携带 Diagnostic（`decision.rs:56-60`），且计入 error_count（`store/mod.rs:1555`、`context_audit.rs:401`）。
6. **worker/cache 无分类元数据**：`WorkerBackend` 仅 5 个字面量（`scanner_contract.py:667`），`WorkerParseResponse` 无 page count/分类字段，`parse_cache` 无 metadata（`store/schema.rs:115`）；`metadata_only` 是 Context action 而非 parser 状态。
7. **快照键不足**：`context_profile_hash` 仅含 `{engine_build, context}`（`context_audit.rs:413`），不含 report_mode/parse 配置/分类器/worker build/discovery issues/worker 可用性。
8. **finalize 要求严格一致**：`context.summary == envelope.summary` 且与 inventory/file_results/decisions 行一致（`store/mod.rs:1500-1520`）——快照不能复用旧 summary。
9. **worker 进程模型**：一进程一请求一 deadline；`WORKER_CONTRACT_VERSION` 为 scanner/office/python 共享全局常量（`parsers/mod.rs:23`）。
10. **schema 演进破坏回滚**：旧代码遇到高版本返回 `TooNew`（`store/schema.rs:239`）。

### 目标与成功标准

1. 90d 窗口冷扫从「卡死 >5min」→ 目标 **≤40s**（准入早停 + no-text PDF 分类）。
2. 月报 30d 冷扫约 1min → 目标 **≤20s**。
3. no-text PDF 不进提取路径：单文件成本 ~2.5s → ~0.3s（分类），提取调用 0 次（`pdfplumber_invocations=0`）。
4. 昂贵档解析受**确定性准入计划**约束：被准入排出的文件 `not_parsed`，不启动提取。
5. **缓存无关确定性**：同一目录+profile，空/部分/全缓存三种状态产生相同 final_context、decisions、summary 语义字段。
6. 温扫无变化：大窗口跳过 cache/context/finalize 写放大（实测对比）；小窗口目标 **≤300ms**（受 discovery ~272ms 下限约束）。
7. 现有 fixed-corpus 门禁保持全绿；新增「真实目录 90d 冷扫有界完成」回归（≤40s，无 timeout/error）。
8. 上下文质量不降：included 集合与内容确定；未解析/扫描件审计标记正确。

## Solution

### 架构：BudgetedContextScheduler 深模块

新增有明确所有权的深模块，把状态机从 `run.rs` 迁出：

```text
BudgetedContextScheduler.execute(cache_aware_plan, parser) -> BudgetedScanOutcome
```

内部拥有：确定性准入计划、cache hit/miss 合并、PDF 分类状态、昂贵工作 deadline、有序解析执行、Context decisions、cache mutations、audit provenance。

职责边界：
- `ParserScheduler`：只执行被选中的解析任务，不做准入判断。
- `Compressor`：保持纯确定性渲染，不启动 I/O。
- `ScannerStore`：只事务化应用 outcome，不参与业务判断。
- `run.rs`：只保留 preflight、discovery、调用与 finalization。
- 测试直接穿过该 interface，一次验证空/部分/全缓存、超时、分类失败、预算耗尽与 deadline。

### Part 1：确定性准入计划（缓存无关）+ 预算早停

**核心约束**：included/parsed 集合必须只依赖确定性输入（discovery 元数据 + profile + PDF 分类），**不依赖解析得到的正文长度**（cache 命中可知真实长度、miss 不可知，若用真实长度决定准入会产生 cache 状态相关的 context_sha256）。

**准入计划（admission plan）**：
1. 以 discovery 结果为输入，按**呈现顺序**（priority→lower path→path→identity，与 `decide_files` 一致）排序。
2. 对每个文件用**类型已知的 excerpt 上限**（`per_file_max_chars` / PDF `excerpt_max_chars` / office `document_excerpt_max_chars`）作为预测 section 大小。
3. 沿呈现顺序累计预测输出，**预测累计 ≤ global_max_chars 时准入**；首个超预算的文件及后续全部**排出**（`not_parsed`）。
4. 准入计划在解析前完整算好并冻结；cache 只影响「准入文件的结果是复用还是执行」，不影响准入集合。
5. 渲染使用真实正文（可能小于预测上限），但准入已固定 → context 确定。

**不采用「边解析边按真实长度早停」**：因为真实长度与 cache 状态相关，会破坏缓存无关确定性。改用预测上限的准入计划（确定性），并允许「预测保守导致少数短文件未被准入」——这是与确定性的取舍，接受。

**PDF 分类进入准入**：no-text 分类结果使 PDF 的预测 section 大小 = metadata 行大小（几乎为零），不会挤占预算；准入计划在分类之后计算。

**门禁**：同一目录+profile，空缓存、随机预热缓存、全缓存三种状态，最终 `final_context`、`decisions`、`summary` 语义字段必须一致（新增）。

**golden output**：新的确定性准入策略需冻结新的 golden fixture（含 included/omitted/not_parsed 分布）。

### Part 2：PDF 分类状态（独立于 Context action）

把两个概念彻底分开：
- **PDF 分类状态**：`text_in_parse_window | no_text_in_parse_window | unknown`（只在 `pdf_max_pages` 检查窗口内判定，故不称 image_only）。
- **Context action**：`keep | compress | metadata_only | omit | error`。

**流程**：PDF cache miss → 先分类（pypdfium2 文本层检测）：
- `text_in_parse_window` → pdfplumber 提取正文（正文解析结果入 parse_cache）。
- `no_text_in_parse_window` → 不调 pdfplumber（`pdfplumber_invocations=0`），产出 metadata 记录。
- 分类失败（加密/损坏/无法打开）→ `unknown` 或 error 兜底，不静默吞。

**类型化分类缓存**（新表/新行，独立于 parse_cache 正文缓存）至少包含：classifier profile hash、分类状态、page count、inspected pages、classifier build、file source_version。parse_cache 继续只保存真正的正文解析结果。

**分类阈值**：不是「可配置」浮点，而是**固定常量 + 可选 profile 演进**。固定常量为默认；若需要按 profile 调整，明确走 `NormalizedScannerProfileV1` 的正式演进（participate in parse/context hash），不静默扩展 wire JSON。

**验收措辞修正**：不再用「所有扫描件 action 都是 metadata_only」——扫描件数量可能挤满上下文预算。改为：**所有 ground-truth 无文字 PDF 分类正确，且 `pdfplumber_invocations=0`；其 Context action 依据全局预算为 metadata_only 或 omit**。

### Part 3：not_parsed 语义重定义（计数等式写入 spec）

在**不改变 CLI JSON 字段形状**的前提下重定义：

```
error_file_count     = 仅统计 parse_status=Error 的文件
timeout_count        = parse_status=Timeout
success_count        = parse_status=Success
not_parsed_count     = source_file_count - success_count - timeout_count - error_file_count   # 派生
```

其他规则：
- `ContextFileEvidence::validate`：允许 `NotParsed` **无 Diagnostic**，改用独立的 `budget_reason`（如 `global_budget_exceeded` / `stage_deadline_exhausted`）。
- `decide_files`：`has_error` 仅当 `parse_status == Error`；`NotParsed` 走独立的 `Omit` 分支（`action=omit`、reason=budget）。
- `Omit` 同时允许「成功解析后预算省略」与「预算原因未解析」两种来源，通过 file result 的 `parse_status`/`cache_status` 区分。
- 同步改动：`context_audit.rs` extension metrics（NotParsed 不再计入 error_count）、`store/mod.rs validate_context_relations` 计数、summary 语义。
- 渲染语义：省略摘要的 `input_chars` 用 size_bytes 近似，标注 `~`（已确认）。
- 上述等式为 contract，不得在实施时临场决定。

### Part 4：成本感知缓存 + 有界淘汰 + GC

缓存策略与 Context inclusion **解耦**：
- **昂贵解析/分类结果始终缓存**（PDF 提取内容、PDF 分类状态、office 结果）——重解析成本高。
- **cheap 缓存有界淘汰**：text-like 用 byte cap、age、last-accessed、source/profile generation 做淘汰，不因某次 Context omit 立即删除仍有效的解析结果。
- 缓存与呈现解耦后，「warm 复用全部」不再是契约；**快照命中**与 **parse-cache 全命中**是两个不同指标，验收措辞分开。
- **物理压缩**：SQLite `DELETE` 只增加 freelist 不缩小文件；物理压缩（`VACUUM`/`PRAGMA incremental_vacuum`）作为**迁移事务之后的独立、可失败维护步骤**，不阻塞业务。
- 新增 GC 规则：按 source generation / parse profile / last-accessed 清理旧行；GC 在后台/低峰执行。

### Part 5：快照快速路径（完整 provenance）

**快照键**至少包含：
`canonical logical request（去 request_id）+ 有序 discovery 结果 + 归一化 discovery issues + 归一化完整 scanner profile + report_mode + engine build + route-stack worker builds + classifier build/profile`。

**worker 身份**：不直接复用上次握手身份。选择**保留 live worker handshake**（成本小、保留「worker 可用性」语义，避免 Python worker 丢失/损坏/升级时返回旧内容）；快照只跳过 parse/cache/context/finalize 写放大阶段。若未来要跳过 handshake，须用不可变 release manifest / 本地 worker artifact fingerprint 替代——本版不做，标为后续。

**审计**：
- 快照只**引用原 `context_run_id`**，新增 `reused_from_context_run_id`，不再次复制 `final_context`（避免重复存储敏感正文）。
- 当前 run **重新计算自己的耗时与 provenance**（total/discovery/parse/compression duration 为本轮实测），summary 与当前 run 的 rows 重新 reconcile，满足 `store/mod.rs:1500` 校验。

**目标**：无变化温扫大窗口跳过 cache/context/finalize 写放大（实测对比）；小窗口 ≤300ms（受 discovery ~272ms 下限约束）。≤150ms 需要 USN 前置检测，**明确出范围**。

### Part 6：长驻流式 PDF worker session（替代批量）

放弃「列表输入/列表输出」（无法逐文件硬超时）。改为长驻、流式会话：
- 进程启动一次、import 一次（摊薄 Python 启动 + pdfplumber import）。
- 父进程**逐个发送 NDJSON request、逐个接收 response**。
- 每个请求仍有**独立 deadline**；超时或崩溃时**杀掉 session 并重启**；已收到的前序结果不丢失。
- 现有 v1 单文件命令保留为 fallback。
- `batch_size` 与 `worker_concurrency` 是**两个独立参数**，不都等于 `max_workers`。
- `WORKER_CONTRACT_VERSION` 为共享常量：新增会话契约作为独立版本（如 python document worker v2），office worker 是否同步升版在一次 schema/contract foundation 中统一决定，不临时单独 bump。

### Part 7：总耗时/数量/页数 deadline

字符预算不约束总耗时，需独立预算：
- expensive parse 总 wall-clock deadline；
- expensive extraction 数量上限；
- PDF 总 inspected pages 上限；
- deadline 到达后停止启动新任务；
- 明确 `stage_deadline_exhausted` 审计原因。

这些预算影响 decisions，**必须进入归一化 profile 与快照语义指纹**（与 Part 5 快照键一致）。90d ≤40s 以此作为可保证契约（而非仅字符早停的推断）。

### Part 8：schema v2 一次性 foundation + 回滚策略

- 一次性完成 schema foundation（含准入/分类/快照/缓存/GC 所需的全部表与列），**不在多个 Task 中逐步改变同一 `user_version`**。
- **回滚策略（明确选择）**：升级前备份生产 DB；应用回滚时连同 DB 一起恢复。发布门禁明确声明：**DB 升级后旧 release 不可直接使用**（`TooNew`）。
- 迁移事务化、失败回滚；旧库先迁移再运行；严格 doctor 校验迁移后 schema。
- 分类缓存、快照指纹、预算/页数上限字段全部纳入 foundation。

### Part 9：低风险调优（最后，按 profile 再做）

- `max_workers` 改为可配置上限 + 按 CPU 自动取值（`min(配置, cpu_count)`）。
- `attach_cache_evidence` 批量查询：**分块或临时表**，保持现有 cache miss reason 优先级，不构造超大 `IN (...)`。
- discovery 去除冗余 canonicalize（扩展名/忽略模式判断前置）。

### 门禁（gates）

#### PDF 分类门禁（独立，进入生产）
分类错误代价不对称（text→no-text 丢内容；no-text→text 仅损性能），必须独立门禁，覆盖：
稀疏文字、文字只出现在 `max_pages` 之外、mixed text/image pages、OCR 隐藏文字层、旋转文字、空白页、加密/损坏/超大页数、CJK 与异常控制字符。
只有在检查完整分类范围后才能称 no_text；只检查当前 `pdf_max_pages` 时状态为 `no_text_in_parse_window`。

#### PDF 提取替换门禁（仅候选替换时）
- 不共用 6 份小型合成语料（pdfplumber P50 ~7ms 与真实 ~2543ms 非同一分布）。
- 增加：逐文件最低质量、成功率 100%、P95/max、timeout rate、峰值内存、脱敏真实语料分层结果。
- 门槛沿用：gt_ratio ≥ 0.950、printable_ratio ≥ 0.980、P50 ≤ pdfplumber × 50%。

### 依赖与发布

- pypdfium2 当前是 pdfplumber 的**传递依赖**，非直接依赖。**生产代码一旦 import 即声明为直接依赖**（`uv add`），不等到替换提取器才加。
- 同步 Windows 发布链 `requirements.lock`（仅改 `pyproject.toml`/`uv.lock` 不会更新部署链）。

### 保持冻结（不修改）

- Rust CLI JSON wire 字段形状（`BuildContextRequest/ContextEnvelope` 等；数值语义按 Part 3 重定义属行为变更，形状不变）。
- 报告 schema、模板内容、LLM 调用次数与 provider 行为。
- office backend 与 fallback policy。
- scanner 权限边界、`report_runner`/`context_scheduler`/`rust_context_client` 的 Python 侧接口。
- **OCR**：no-text PDF 记为分类结果，不提取扫描件文字。

## Testing

### BudgetedContextScheduler interface 测试（穿过 seam）
- 空/部分/全缓存一致性：同一目录+profile 三态 → final_context、decisions、summary 语义字段一致。
- 预算耗尽：被准入排出的文件 `not_parsed`、无 Diagnostic、budget_reason 正确、included 集合确定。
- deadline：`stage_deadline_exhausted` 正确记录；deadline 后不再启动新任务。
- 分类：no-text PDF `pdfplumber_invocations=0`；text PDF 正常提取；分类失败兜底。
- 快照：同目录状态两次 run → 第二次引用原 context_run_id、重算耗时、summary reconcile；任一文件变更 → 不命中。
- 缓存：cheap 有界淘汰、昂贵必缓存、GC 清理、物理压缩独立可失败。

### 门禁回归
- 现有 fixed-corpus cold/warm 门禁全绿。
- 新增真实目录 90d 冷扫 ≤40s、无 timeout/error、warm 有界。
- PDF 分类门禁 + 提取替换门禁（独立，见上）。

### 验证命令（Windows）

```bash
uv run pytest
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
uv run python main.py doctor --strict
git diff --check
```

## Acceptance Criteria

- [ ] 90d 真实目录冷扫 ≤40s（含「不卡死」回归）。
- [ ] 月报 30d 冷扫 ≤20s。
- [ ] 同一目录+profile，空/部分/全缓存三态 final_context、decisions、summary 语义字段一致（缓存无关确定性）。
- [ ] 被准入排出的昂贵文件全部 `not_parsed`（不启动提取）；计数等式（Part 3）正确且进 extension metrics。
- [ ] no-text PDF 分类正确（ground truth）、`pdfplumber_invocations=0`；Context action 为 metadata_only 或 omit（按预算）。
- [ ] 快照键含完整 provenance；命中引用原 context_run_id + `reused_from_context_run_id`，当前 run 重算耗时且 summary reconcile。
- [ ] 流式 worker session：逐文件 deadline、超时杀会话重启、前序结果不丢失、v1 fallback 可用。
- [ ] 总耗时/数量/页数 deadline 生效并进入 profile 与快照指纹；`stage_deadline_exhausted` 可审计。
- [ ] 缓存解耦：cheap 有界淘汰、昂贵必缓存、GC/物理压缩独立可失败；「快照命中」与「parse-cache 全命中」分开度量。
- [ ] schema foundation 一次性完成；升级前备份、回滚恢复、发布门禁声明旧 release 不可直接用升级后 DB。
- [ ] pypdfium2 声明为直接依赖并同步 requirements.lock（一旦生产 import）。
- [ ] fixed-corpus 门禁全绿；Rust workspace 测试、pytest 全绿、doctor --strict 通过。
- [ ] 分类门禁 + 提取替换门禁独立存档，生产代码按各自门禁结果落地。

## Out of Scope

1. Rust CLI JSON wire 字段形状变更。
2. 报告 schema、模板、LLM prompt 与 provider 行为变更。
3. office backend 或 office fallback policy 变更。
4. scanner 权限边界、Python 侧 `report_runner` 接口变更。
5. **OCR**：no-text PDF 内容提取不在本 spec——记为分类结果，不引入 tesseract/云 OCR 依赖。
6. **USN Journal / 持久目录监视器**：快照前可信变化检测不在本 spec（warm ≤150ms 才需要）。
7. 跨载体报告事务、Web/GUI/daemon/queue。
8. 非本 spec 提及的 parser backend 替换。

## Implementation Decisions

- **ID-1 确定性准入计划**：included/parsed 集合仅依赖元数据+分类+profile，与 cache 无关；golden output 冻结。
- **ID-2 预算用类型 excerpt 上限预测**：确定性、可提前计算；允许预测保守的少量未准入。
- **ID-3 PDF 分类独立于 Context action**：`text_in_parse_window | no_text_in_parse_window | unknown`；类型化分类缓存。
- **ID-4 not_parsed 重定义**：error_count 只算 Error；not_parsed 派生；NotParsed 可无 Diagnostic；计数等式为 contract。
- **ID-5 缓存与呈现解耦**：昂贵必缓存，cheap 有界淘汰，GC/物理压缩独立。
- **ID-6 快照完整 provenance**：键含全 profile/report_mode/worker/classifier/discovery；引用原 context_run_id + 重算耗时。
- **ID-7 流式 worker session**：NDJSON 逐请求独立 deadline，超时杀会话重启，v1 fallback。
- **ID-8 deadline 进 profile 与指纹**：总耗时/数量/页数上限进归一化 profile 与快照键。
- **ID-9 schema foundation 一次性**：不逐步改 user_version；备份+回滚策略明确。
- **ID-10 深模块 BudgetedContextScheduler**：状态机单一所有权，run.rs/Compressor/Store 各守边界。

## 建议实施顺序

1. 把本版不变量写入代码与测试：缓存无关确定性、NotParsed 计数、deadline、分类状态。
2. 一次性 schema/worker contract foundation + 生产 DB 备份与回滚策略。
3. 声明 pypdfium2 直接依赖并同步 requirements.lock；实现单文件 PDF 分类器 + 独立质量门禁。
4. 实现 BudgetedContextScheduler：空/部分/全缓存一致性 + 总时间预算测试。
5. 成本感知缓存 + GC/物理压缩。
6. 快照：完整 provenance + 现实目标。
7. 长驻流式 worker；门禁存档后条件性替换 PDF 提取器。
8. 批量 SQL 与 discovery 微调（最后，按 profile 再做；SQL 分块/临时表，保持 cache miss reason 优先级）。

## 主要风险与缓解

| 风险 | 缓解 |
|---|---|
| 准入预测保守导致少装文件 | 用类型 excerpt 上限预测；确定性优先，接受少量未准入 |
| cache 状态影响 context | 准入计划 cache 无关 + 三态一致性门禁 + golden output |
| no-text 分类误伤稀疏文本 PDF | 独立分类门禁覆盖稀疏/混合/旋转/OCR 隐藏层等 |
| not_parsed 语义改动破坏现有消费方 | 计数等式进 spec；报告模板/消费方不把 error_file_count 当含 not_parsed |
| 快照复用旧 parser 结果/旧耗时 | 完整 provenance 键 + 保留 handshake + 当前 run 重算耗时 |
| 流式 worker 会话泄漏/崩溃 | 独立 deadline + 杀会话重启 + v1 fallback |
| 90d ≤40s 不可保证 | 总耗时/数量/页数 deadline 作为硬契约进 profile |
| schema v2 破坏旧 release | 升级前备份 + 回滚恢复 + 发布门禁声明 |
| pypdfium2 非直接依赖漂移 | 生产 import 即 `uv add` + 同步 requirements.lock |

## 决策记录

- 2026-08-08 用户授权：重开 scanner DB schema / cache identity 冻结边界，做 v2 迁移。
- 2026-08-08 用户确认：未解析文件 input_chars 用 size 近似。
- 2026-08-08 用户确认：no-text PDF（扫描件）暂不做 OCR，记为分类结果。
- 2026-08-08 设计审查后：六条阻断点与高优先级项本版闭合；状态 Needs revision，待复评。
