# Scanner 预算感知解析 + 缓存瘦身 + PDF 分类与激进性能 设计规格

> 状态：Ready for implementation
> 日期：2026-08-08
> 决策范围：Rust scanner core 的解析预算、缓存、快照、PDF 分类与性能调优
> 首要目标：以真实目录证据驱动，消除「解析大量文件却只进少量上下文」的浪费，并把 image-only PDF（扫描件）从昂贵的 pdfplumber 提取路径中剔除
> 不涉及：Rust CLI JSON wire contract、报告 schema/模板、LLM 行为、office backend 迁移、OCR

## Problem Statement

### 真实语料与实测证据（2026-08-08，`D:\01- 工作`）

对真实工作目录 `D:\01- 工作`（3293 文件 / 13.4GB）做清点、PDF 分类与受控实测：

**目录规模与窗口负载**

| 窗口 | 可解析文件 | office/PDF 类 | 解读 |
|---|---:|---:|---|
| 日报 (2d) | 33 | 32 | 冷扫秒级，缓存价值低 |
| 周报 (7d) | 136 | 95 | 冷扫 3.7s，已有感知 |
| 月报 (30d) | 322 | 267 | 冷扫约 1 分钟量级 |
| 90d | 729 | 516 | 冷扫卡死 >5min，PDF 273 |
| 全年 | 1667 | 1565 | 冷扫预计 10min+ |

**PDF 类型分布（pypdfium2 快速分类，1184 个 PDF）**

| 窗口 | PDF 数 | 有文字 | **纯图片（扫描件）** | image 占比 |
|---|---:|---:|---:|---:|
| 7d | 4 | 4 | 0 | 0% |
| 30d | 74 | 16 | **58** | **78%** |
| 90d | 273 | 68 | **205** | **75%** |
| 全年 | 1184 | 403 | **779** | 66% |

月报/季度窗口里 3/4 的 PDF 是扫描件（投标文件、审计报告、设计评审、技术协议等），pdfplumber 提取不到任何文字却照样花 ~2.5s/个白解析。

**7 天窗口冷/温实测**（`scripts/benchmark_scanner.py`，一次性 scan DB）

| 指标 | 冷扫 | 温扫 |
|---|---:|---:|
| 总耗时 | 3735ms | 347ms |
| discovery | 273ms | 272ms（占 warm 78%） |
| parse | 3387ms（91%） | 0ms（全复用） |
| 吞吐 | 36 files/s | 392 files/s |
| 解析/复用 | 解析 136 | 复用 136 |
| 进上下文 | **14 / 136（10%）** | — |

单文件 parse 耗时（实测）：PDF **~2543ms/个**（Python worker + pdfplumber）、xlsx ~14ms、docx ~12ms、pptx ~18ms。

### 当前问题

1. **解析浪费（核心）**：`run.rs` 先对**所有 cache miss 文件**并行解析（office/PDF 每个起独立子进程），`compressor.rs` 再按全局预算 `total_max_chars` 把放不下的文件 `Omit`。实测 136 个解析中只有 14 个进上下文，**90% 的解析和缓存是纯浪费**。大窗口（90d/全年）会为「塞进 ~14 个文件」解析 273~823 个慢 PDF。
2. **image-only PDF 是纯浪费**：真实月报/季度窗口里 75~78% 的 PDF 是扫描件，pdfplumber 提取空文本却每个花 ~2.5s，是 90d 冷扫卡死 >5min 的直接原因。
3. **缓存臃肿**：`parse_cache` 对每个成功解析文件写内容缓存，实测 136 行中仅 ~14 行（进上下文的）有复用价值，其余 122 行是存储与写放大浪费。
4. **PDF 是唯一大瓶颈**：office 已吃满 Rust 红利（15ms 级），PDF 走 Python worker 平均 2.5s（慢 100 倍）；PDF→Rust 迁移已在 2026-08-07 被门禁否决，PDF 短期只能留在 Python。
5. **温扫 discovery 成本**：温扫 347ms 中 272ms 是 `WalkDir` 全量遍历 + 逐文件 `canonicalize`+stat，未利用已存储的 `context_runs.final_context`。
6. **并行度与查询**：`max_workers=4` 固定；`attach_cache_evidence` 逐文件单条 SQLite 查询（N 次往返）。

### 目标与成功标准

1. 90d 窗口冷扫从「卡死 >5min」→ 目标 **≤40s**（早停 + image PDF 分类核心收益）。
2. 月报 30d 冷扫约 1min → 目标 **≤20s**。
3. image-only PDF 不再进入 pdfplumber 提取路径：实测 30d/90d 窗口 image PDF 全 `metadata_only`，单文件成本从 ~2.5s 降到 ~0.3s（分类）。
4. 昂贵档（text PDF）解析不超预算：被预算省略的文件全部 `not_parsed`（不启动提取）；便宜档（office/text）全量解析成本有界（90d 约 1.5s）。
5. `parse_cache` 按「重解析成本」精简：PDF 结果（text 内容 / image 分类）必缓存，便宜文件仅 included 缓存，DB 体积下降。
6. 温扫无变化命中快照 → 目标 **≤150ms**（从 347ms）。
7. 现有 fixed-corpus 门禁保持全绿；新增「真实目录 90d 冷扫有界完成」回归（≤40s，无 timeout/error）。
8. 上下文质量不降：included 文件集合与内容保持确定性；未解析/扫描件有明确审计标记。

## Solution

### Part 1（P0）：预算感知早停解析

把「解析全部 → 再丢弃 90%」改成「按上下文呈现顺序边解析边记账，预算耗尽即停止启动新解析」。

**解析顺序**：与上下文呈现顺序一致（planner 现有 `files.sort_by(path)` 保持），保证同输入同输出、报告语义不变。

**两档解析（预算记账）**：
- 便宜档（text-like + xlsx/docx/pptx，单文件 <20ms）：总是并行解析（现有 rayon 通道），成本可忽略。
- 昂贵档（PDF，~2.5s/个）：按呈现顺序**分块**处理——每次并行解析下一个 `max_workers` 个昂贵文件，逐块渲染并入预算；**剩余预算装不下下一块时停止**，其余昂贵文件标 `not_parsed`。

**预算记账**：复用 `compressor.rs` 的增量渲染与 `can_append_with_footer` 逻辑，边渲染边累计实际输出字符；`global_max_chars` 用尽即判定后续文件 `omit`。为保持确定性，渲染顺序 = 呈现顺序。

**未解析文件审计**：
- `parse_status='not_parsed'`（schema 已预留该枚举值）。
- decision `action=omit`、`reason=global_budget_exceeded`、`output_chars=0`。
- 省略文件摘要的 `input_chars` 用 **size_bytes 近似**（已确认），渲染时标注 `~` 近似记号。
- `source_file_count`、`omitted_file_count` 语义不变（包含未解析文件）。

**边界防护**：对昂贵文件用保守门控 `剩余预算 ≥ 该文件 excerpt 上限`（按类型已知）预判，避免对必然装不下的文件启动进程。

**与并行通道的关系**：便宜档仍走现有 `ParserScheduler.parse_planned_files`（rayon 全量）；昂贵档改为分块有界并行，块间渲染记账。允许最多一块（≤`max_workers` 个文件）的边际浪费，换取有界并行与早停。

### Part 2（P1）：缓存按「重解析成本」精简

- **规则**：`parse_cache` 覆盖——included 文件（任意类型）+ **所有 PDF 的解析/分类结果**（text 内容或 image 分类）；便宜文件（office/text）仅当 included 才写缓存。
- **理由**：重解析成本决定缓存价值。PDF 2.5s 必须缓存（否则温扫每次重新提取/分类）；office/text 15ms 可随时重解析，非 included 不缓存以消除臃肿（实测 136 行 → 约 18 行）。
- 预算漂移导致以前 omitted 文件变 included → cache miss → 重解析，语义正确（便宜文件成本可忽略）。
- v2 迁移时按新规则清理旧库 `parse_cache` 行。

### Part 3（P2）：无变化快照快速路径

- `context_runs.final_context` 已存完整上下文全文。
- v2 新增「目录状态指纹」存储：`(discovered 文件 (file_identity, source_version) 排序哈希, context_profile_hash)`，随成功 run 落库。
- 下一轮 discovery 完成后，若指纹与最近成功 run 一致且 profile hash 一致 → **直接返回缓存的 final_context 与 summary**，跳过 worker handshake、cache 查询、解析、上下文重渲染。
- 收益：温扫 347ms → ~150ms（discovery 272ms 是检测变化的必需成本，不省）。
- 快照命中仍写入一条新的 audit run（scan_runs/context_runs 引用已缓存上下文），保持审计连续。

### Part 4（P0）：PDF 快速分类（image vs text）

把 image-only PDF（扫描件）从昂贵的 pdfplumber 提取路径中剔除。

- **分类器**：pypdfium2 文本层检测（已在依赖，`pypdfium2-5.12.1`，无需新增依赖）。判定：某页文字 ≥ N 字符（默认 ~20，可配置）即视为有文本层。
- **流程**：PDF cache miss 时先分类——
  - 有文本层 → 走 pdfplumber 提取正文（text PDF）；
  - 无文本层（image-only）→ 返回 `metadata_only`：记录 page_count、扫描件标记、size，**不跑 pdfplumber、不产生空 body**。
- **缓存分类结果**：image-only 写 `metadata_only` 缓存行（backend `pdf_image_meta_v1`，空内容 + 元数据），温扫直接复用分类结果，不重复扫描 205 个扫描件。
- **成本**：image PDF 单文件从 ~2.5s（pdfplumber 空提取）降到 ~0.3s（worker 进程启动 + pypdfium2 分类）；配合批量 worker（Part 5a）进一步摊薄进程启动。
- **边界**：文本阈值太严会误伤稀疏文本 PDF → 阈值可配置 + 分类阈值回归测试。
- **契约影响**：分类是 Python worker 内部 PDF 处理的一步，不改 wire contract，不升 worker contract（批量 worker 才升 v2）。

### Part 5（P1，激进 PDF）：批量 worker + pypdfium2 门禁评估

#### 5a 批量 PDF worker
- 一次性进程处理一批 PDF（列表输入、列表输出），摊薄 Python 解释器启动 + pdfplumber import（占单文件 2.5s 中约 0.5~1s），并与 Part 4 分类合并（一个 worker 进程内「分类 → 路由 → 提取」多文件）。
- 分块大小与 `max_workers` 对齐；批内单文件超时仍逐文件判定。
- **worker contract 升 `WORKER_CONTRACT_VERSION` 到 v2**，新契约独立测试。
- 崩溃隔离降级：批失败时按块回退到逐文件路径，保留现有隔离。

#### 5b pypdfium2 门禁评估（重开 2026-08-07「PDF 维持 pdfplumber」边界）
- 复用 pdf-extract 门禁方法：`tests/fixtures/pdf_benchmark/` 同语料（6 份中英 PDF，ground truth 已知），门槛 = 平均 gt_ratio ≥ 0.950、printable_ratio ≥ 0.980、P50 耗时 ≤ pdfplumber × 50%（至少 2x 快）。
- **两项都通过才替换生产 `document_parser.py` 的 PDF 分支**（此时 pypdfium2 同时承担 Part 4 分类与正文提取）；任一不通过则维持 pdfplumber，pypdfium2 仍作为 Part 4 快分类器（不依赖门禁）。
- 若候选为 pypdfium2，需将其声明为直接依赖（`uv add`）——本 spec 已含该授权，实施时执行。

### Part 6（P2，低风险调优）

- `max_workers` 改为可配置上限 + 按 CPU 数量自动取值（`min(配置, cpu_count)`），默认仍受显式配置约束。
- `attach_cache_evidence` 由逐文件查询改为**批量 SQL 查询**（一次取回所有候选键）。
- discovery 去除冗余：扩展名/忽略模式在 `canonicalize` 之前判断（现状已基本如此，复核排除目录重复 `canonicalize`）。

### Schema v2 迁移

沿用 `LATEST_USER_VERSION` / `COMMITTED_USER_VERSIONS` 机制，新增事务化迁移：

1. 新增目录状态指纹表 `scan_state_fingerprints`：记录 `(directory_fingerprint, context_profile_hash, scan_run_id, context_run_id, final_context, created_at_ms)`，以 `(directory_fingerprint, context_profile_hash)` 为查询键。
2. `parse_cache` 按 Part 2 规则精简：清理非 included 便宜文件行；image-only PDF 保留为 `pdf_image_meta_v1` metadata 行（可复用现有列，`content` 为空 + backend 标记）。
3. 迁移事务化、失败回滚；旧库（v1）先迁移到 v2 再运行。
4. 严格 doctor 校验迁移后 schema。

### 保持冻结（不修改）

- Rust CLI JSON wire contract（`BuildContextRequest/ContextEnvelope` 等）。
- 报告 schema、模板内容、LLM 调用次数与 provider 行为。
- office backend（`rust_office_oxide_v1` / `rust_xlsx_bounded_v1`）与 fallback policy。
- scanner 权限边界、`report_runner`/`context_scheduler`/`rust_context_client` 的 Python 侧接口。
- **OCR**：image-only PDF 记为 metadata_only，不提取扫描件文字。

## Error Handling 与确定性

- 早停、快照命中、缓存精简、PDF 分类都必须保持**确定性**：同输入（目录状态 + profile）同输出。
- 未解析文件不产生 parse 错误；病态 PDF 超时仍按现有 timeout 语义记录，不影响其他文件。
- 快照命中仅在指纹完全一致时启用；指纹不一致的任何情况回退到完整扫描。
- PDF 分类失败（无法打开/损坏）→ 视为 text 路径兜底或记录 error，不静默吞掉。
- 预算记账溢出、指纹哈希冲突等内部不变量破坏时，回退完整扫描并记录诊断。

## Testing

### Rust 单元/集成

- 早停：构造「预算装不下一部分昂贵文件」的语料，断言未解析文件 `not_parsed` + `omit`、included 集合确定、省略摘要 size 近似正确。
- 缓存精简：断言 `parse_cache` 只含 included 便宜文件 + PDF 结果（text/image）；预算漂移后 included 变化时温扫正确重解析。
- 快照：同目录状态两次 run → 第二次命中快照返回相同 `final_context`；任一文件变更 → 不命中。
- 批量 worker v2：批处理正确性、批内超时、批失败回退逐文件。
- schema v2 迁移：v1→v2 事务化、失败回滚、旧行清理。

### Python worker / PDF 分类

- image-only PDF → `metadata_only`，不产生空 body，缓存为 `pdf_image_meta_v1`。
- 稀疏文本 PDF 不被误伤（阈值回归）。
- pypdfium2 门禁：同 pdf-extract 方法（语料 + gt_ratio/printable_ratio/P50）。

### 性能回归门禁

- 现有 fixed-corpus cold/warm 门禁全绿（`tests/test_benchmark_scanner.py`）。
- 新增真实目录回归：`D:\01- 工作` 90d 冷扫 ≤40s、无 timeout/error、warm 复用全部。

### 验证命令（Windows）

```bash
uv run pytest
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked
uv run python main.py doctor --strict
git diff --check
```

PDF 门禁评估（独立于生产）：

```powershell
uv run python scripts\pdf_benchmark\generate_corpus.py
uv run python scripts\pdf_benchmark\run_python.py        # pdfplumber 基线
uv run python scripts\pdf_benchmark\run_candidate.py     # pypdfium2 候选
uv run python scripts\pdf_benchmark\summarize.py
```

## Acceptance Criteria

- [ ] 90d 真实目录冷扫 ≤40s（含「不卡死」回归）。
- [ ] 月报 30d 冷扫 ≤20s。
- [ ] 30d/90d 窗口 image-only PDF 全部 `metadata_only`，单文件成本 ~0.3s，不进 pdfplumber 提取路径（实测记录）。
- [ ] 昂贵档解析不超预算：被预算省略的昂贵文件全部 `not_parsed`（不启动提取），实测昂贵档预算外解析数 ≈ 0；便宜档全量解析成本有界（90d 约 1.5s）。
- [ ] `parse_cache` 按成本规则精简（included + PDF 结果），DB 体积下降（实测对比）。
- [ ] 温扫无变化命中快照，≤150ms；image/text PDF 分类结果温扫复用，不重复分类。
- [ ] 早停、快照、分类输出确定；未解析/扫描件审计标记正确。
- [ ] fixed-corpus 门禁全绿；Rust workspace 测试、pytest 全绿、doctor --strict 通过。
- [ ] pypdfium2 门禁结论存档（通过→替换 PDF 分支；不通过→pdfplumber 提取 + pypdfium2 分类），生产代码按门禁结果落地。
- [ ] schema v2 迁移事务化、可回退；旧库升级可用。

## Out of Scope

1. Rust CLI JSON wire contract 变更。
2. 报告 schema、模板、LLM prompt 与 provider 行为变更。
3. office backend 或 office fallback policy 变更。
4. scanner 权限边界、Python 侧 `report_runner` 接口变更。
5. **OCR**：image-only PDF（扫描件）内容提取不在本 spec——记为 metadata_only，不引入 tesseract/云 OCR 依赖。若未来需要，另立 spec。
6. 跨载体报告事务、Web/GUI/daemon/queue。
7. 非本 spec 提及的 parser backend 替换。

## Implementation Decisions

- **ID-1 解析顺序 = 呈现顺序**：保持报告语义，避免「解析内容与呈现不一致」。
- **ID-2 增量渲染记账**：复用 compressor 增量渲染，边解析边记实际输出预算。
- **ID-3 not_parsed 用 size 近似 input_chars**：审计标记明确、可读，避免额外读文件。
- **ID-4 缓存按重解析成本**：PDF 必缓存（text/image），便宜文件仅 included 缓存——消除臃肿且不破坏温扫。
- **ID-5 快照指纹**：目录状态指纹 + profile hash 双键，确定性高、实现小。
- **ID-6 分块有界并行**：昂贵档按 `max_workers` 分块，允许一块边际浪费换取早停与并行。
- **ID-7 image PDF 快分类**：pypdfium2 文本层检测，image-only 直接 metadata_only，剔除空提取浪费。
- **ID-8 批量 worker 带批失败回退**：保留崩溃隔离，收益在摊薄 Python 启动+import，与分类合并。
- **ID-9 PDF 门禁独立于生产**：候选不达标零生产影响，复用既有门禁方法；pypdfium2 分类器不依赖门禁通过。
- **ID-10 v2 迁移事务化**：沿用既有 user_version 机制，失败可回退。

## 建议实施顺序

1. Part 4 PDF 快分类（image vs text）—— 收益最大且独立，先做，含分类缓存与回归。
2. Part 1 预算早停 + 对应测试与真实目录回归。
3. Part 2 缓存精简 + v2 迁移（含旧行清理）。
4. Part 3 快照快速路径 + v2 指纹存储。
5. Part 6 低风险调优（max_workers、批量 cache 查询）。
6. Part 5a 批量 PDF worker（contract v2）。
7. Part 5b pypdfium2 门禁评估 → 按门禁结果决定是否替换生产 PDF 分支。

## 主要风险与缓解

| 风险 | 缓解 |
|---|---|
| 早停改变上下文呈现/审计口径 | 省略摘要标注 size 近似；呈现顺序不变 |
| PDF 门控保守导致少装文件 | 分块 + 实际渲染记账，仅≤一块边际浪费 |
| pypdfium2 候选翻车 | 门禁否决即维持 pdfplumber 提取，分类器独立可用 |
| 分类阈值误伤稀疏文本 PDF | 阈值可配置 + 分类回归测试 |
| 早停/快照/分类破坏确定性 | 同输入同输出测试；指纹不一致回退完整扫描 |
| 病态 PDF 卡死仍存在 | 分块批内逐文件超时；90d 回归门槛保证有界完成 |
| v2 迁移破坏旧库 | 事务化迁移 + 失败回滚 + 旧库升级验证 |

## 决策记录

- 2026-08-08 用户授权：重开 scanner DB schema / cache identity 冻结边界，做 v2 迁移。
- 2026-08-08 用户确认：未解析文件 input_chars 用 size 近似。
- 2026-08-08 用户确认：实施方向一（预算早停 + 缓存精简 + 快照）并加入激进 PDF 性能（批量 worker + pypdfium2 门禁评估）与低风险调优。
- 2026-08-08 用户确认：image-only PDF（扫描件）暂不做 OCR，统一记为 metadata_only。
