# 设计：最大化保留文件原文的上下文压缩改进

- 日期：2026-08-10
- 状态：已确认（预算 500k / 双端兜底 / 契约零改动）
- 关联：`docs/scanner-backends.md`、`rust/scanner_core/src/compressor.rs`、`rust/scanner_core/src/config.rs`、`rust/scanner_contract/src/lib.rs`

## 1. 背景与问题

当前「文件内容 → LLM 上下文」的压缩链路（Rust core `build_context`）在三个环节丢失原文：

1. **解析阶段预截断**：周/月报走 `summary_*` 降档（`summary_text_max_chars=2000`、`summary_pdf_max_pages=2`、`summary_excel_max_rows=10`）；light text 解析只读文件头部 256KB（`.log` 只读尾部 256KB）——内容在进压缩器之前就没了。
2. **逐文件硬截断**（`compressor.rs` `render_file_section`）：正文超过单文件预算时只留头部 N 字符（`.log` 只留尾部），中段与结尾整体丢失，且不感知行/段落边界。
3. **全局准入省略**：超出候选配额/全局预算的文件只留一行省略摘要。配额（96/192/384）已足够宽，不是当前主要矛盾。

用户确认的目标约束：

- **逐字保留，绝不让 LLM 改写**——改进只做「选哪段、怎么切、怎么分预算」，无 LLM 参与压缩。
- 预算可以大幅放量：模型为 `deepseek-v4-flash`（上下文 1,000,000 token、最大输出 8,192），输入侧字符预算天花板约 70 万字符。
- 少而长（少数超大文件）与多而杂（大量文件）两种工作量都要稳健。
- 主要痛点：长文件砍头丢中尾、周/月报解析阶段预截断。

## 2. 方案总览

方案一：**大预算 + 结构感知双端兜底**。

- 全局/单文件预算大幅提额，使绝大多数文件全文逐字进入上下文；
- 仅对仍超预算的巨文件（>100k 字符）做结构感知「头 + 尾」切割兜底，切点回退到行边界，逐字保留；
- 解析阶段各类型上限同步放宽，取消周/月 summary 降档；
- 契约与预算模型不变量零改动，压缩策略版本号 bump 使缓存失效。

明确不做（YAGNI）：

- 不做 LLM 摘要压缩（违反逐字约束）；
- 不做「全局预算填满 pass」（预算放大后收益锐减，且需动预算模型/scheduler 契约）。

## 3. 预算与解析上限（新默认值）

默认值定义在 `rust/scanner_core/src/config.rs` 的 profile 归一化中；所有值均可被 scanner 配置覆盖（wire 叶子键名不变）。

| 项 | 现值 | 新默认 | 说明 |
|---|---|---|---|
| `global_max_chars` 日/周/月 | 50k / 50k / 60k | **500k 统一** | 1M token 窗口内，约 35~40 万 token |
| `per_file_max_chars` 日/周/月 | 8k / 5k / 4k | **100k 统一** | 10 万字以内全文逐字进上下文 |
| summary 模式文本上限（周/月） | 2k | **400k** | 与日报统一，取消降档 |
| `document_excerpt_max_chars`（Office/PDF） | 6k | **400k** | 摘录上限 > 单文件预算，压缩器才能看到超预算内容并做双端兜底 |
| PDF 页数（日 / summary） | 5 / 2 | **100 页统一** | 摘录 100k 字符上限兜底 |
| Excel 行数（日 / summary） | 50 / 10 | **20k** | 摘录上限兜底 |
| Excel sheets / docx 段落 / pptx 页 | — | **100 / 50k / 500** | 摘录上限兜底 |
| light text 读头部 `direct_text_read_bytes` | 256KB | **2MB** | 100k 中文字符 ≈ 400KB，2MB 富余 |
| `.log` 读尾部 `log_tail_read_bytes` | 256KB | **2MB** | |
| 聚合上限 `total_max_chars`（`aggregate_max_chars`） | 50k | **500k** | 实现时验证其执行点（当前仅在 config.rs 赋值，未见强制点） |
| 候选文件配额 `max_candidate_files` | 96 / 192 / 384 | **不变** | 已足够宽 |
| `COMPRESSION_POLICY_VERSION` | `markdown_context_v2` | **`markdown_context_v3`** | 缓存 profile 判定不一致 → 强制重建 |

配置文件同步更新：

- `config/settings.example.yaml`：示例默认值改为新值；
- `config/settings.windows.yaml`：用户本地生效配置改为新值（含 `.pdf` 超时放宽，见 §5）。

## 4. 结构感知双端兜底切割

唯一保留下来的切割路径，位于 `compressor.rs` 的 `render_file_section`，仅当 `input_chars > per_file_max_chars`（100k）时触发。`action=Compress`、`truncated=true` 语义与现有契约完全一致。

普通文本（`.md`/`.txt`/Office/PDF 摘录）：

1. 先扣除标记预留 `marker_reserve`（≈64 字符，覆盖最坏情况的省略标记行），剩余按比例分配：头预算 = 40% × (limit − marker_reserve)，尾预算 = 60% × (limit − marker_reserve)（比例可配，默认 40/60）；
2. 头部取前 `head_budget` 字符后**回退到最近的换行边界**（不截断半行）；尾部取最后 `tail_budget` 字符后**前进到最近的换行边界**；
3. 中缝插入一行标记：`…（已省略中部约 N 字符）…`（N 为实际省略的字符数，`count_chars` 计算）；
4. **不变量保证**：边界回退/前进只会缩短头/尾（`head_end ≤ head_budget`、
   `tail_start ≥ total − tail_budget`），marker 计入 `OMITTED_MARKER_RESERVE=64`
   预留——`count_chars(body) ≤ limit` 结构性成立，无需补偿逻辑；测试显式断言
   边界放置与预算不变量。

`.log`（时间序，新信息在尾部）：

1. 解析摘录层取读窗（2MB）的**最后** `excerpt_max_chars`（400k）字符——metadata 行保留在开头（`finalize_tail_content` / `limit_tail_chars`）；
2. 压缩器再取最后 `limit − 64` 字符（逐字后缀），头部加省略标记：`…（已省略头部约 N 字符）…`。

边界与安全（补充）：

- 解析摘录上限（400k）**大于**压缩器单文件预算（100k）——这是双端兜底可达的前提；预算模型预留取 `min(per_file_max_chars, route.max_excerpt_chars)`，预留不变，多文件准入算术不受影响；
- 超过 400k 字符的文件：摘录层保留前 400k（`excerpt_max_chars` 前缀截断），压缩器对前 400k 做头+尾兜底——比纯砍头多覆盖尾部，但文件 400k 之后的内容仍不可达（读窗 2MB 内的覆盖上限）。

边界与安全：

- 全部按 Unicode scalar（`count_chars`/`chars()`）切割，保持现有习惯，不破坏多字节字符；
- 内容 ≤ limit 的文件原样透传（verbatim），零改动路径；
- 空内容、无换行单行巨文本（罕见）退化为纯字符截断 + 标记，仍逐字；
- 省略标记计入预算，避免 `BUDGET_MODEL_MISMATCH`。

## 5. 风险与缓解

1. **PDF 解析超时**：页数 5 → 100 后，单文件解析耗时可能超过 `file_timeout_by_extension` 中 `.pdf: 45s`；超时文件进 error 列表（有省略摘要行，不丢元信息）。缓解：本地配置 `.pdf` 超时放宽至 **120s**；页数虽为 100，摘录 100k 字符上限使实际读取页数受字符约束。
2. **每次运行 token 成本**：500k 字符 ≈ 35~40 万输入 token。用户已确认可接受；所有值可覆盖回退（成本敏感时调低全局/单文件预算即可）。
3. **Office 解析耗时**：docx 段落 / pptx 页数上限放大后单文件耗时上升；不改变超时语义（超时 → error 列表 + 省略摘要），不做隐式 fallback。
4. **缓存放大**：缓存条目按文件粒度存储解析结果，预算放大不增加缓存条目数，只增单条体积；`markdown_context_v3` 版本 bump 后旧缓存整体失效一次，之后正常。
5. **分类超时预算与窗口匹配**：分类窗口从 5 页/PDF 放大到 100 页后，`pdf_classification_timeout_ms` 默认由 2s 提到 **10s**（与本地配置 `settings.windows.yaml` 及 corpus gate 钉值一致）。分类器指纹含 `pdf_classification_timeout_ms`，默认变化使分类缓存身份失效一次——与 `markdown_context_v3` bump 同级，预期且可接受。

## 6. 测试策略

Rust 单测（`rust/scanner_core`，`compressor.rs` 新增用例）：

- 双端切割：中文 Unicode 边界、行边界回退（不截半行）、marker 与实际省略字符数一致、`output_chars` 含 marker、body ≤ limit；
- 边界规则：头部结束于换行边界（`'\n'` 之后）、尾部起始于行首；无换行区域退化为字符截断；body ≤ limit 与 marker 字符数断言；
- `.log` 尾优先 + 头部省略标记；
- 预算内文件原样透传（verbatim，无标记）；
- 巨文件（>100k）触发兜底路径；
- 无换行单行巨文本退化为字符截断。

Rust 集成（`contract_v2` / `scheduler_core` 用例）：

- 大预算 profile 下 `rendered ≤ reserved` 不变量成立；
- `BUDGET_MODEL_MISMATCH` / `ContextFixedSectionsOverBudget` 语义不变；
- 压缩策略版本 bump 后缓存判定不一致（cache profile 重建）。

回归：

- `scripts/corpus_gate.py`：冻结 profile 显式设值，不受新默认影响，跑通一遍确认；
- Python：`uv run pytest` 全量；
- 基准脚本（`scripts/benchmark_scanner.py` 等）profile 覆盖，确认大预算下不回归。

## 7. 文档更新

- `docs/scanner-backends.md`：预算表与压缩策略描述更新；
- `CLAUDE.md`：「扫描策略：summary_mode + total_max_chars」表述更新为「大预算 + 双端兜底」；
- 本设计文档归档到 `docs/archive/specs/` 的惯例按项目现有流程处理（实施完成后）。

## 8. 实施范围（涉及文件）

- `rust/scanner_core/src/config.rs`：新默认值（全局 500k、单文件 100k、解析上限、读头/读尾字节）；
- `rust/scanner_core/src/compressor.rs`：双端兜底切割（`take_head_and_tail` 等）替代 `take_prefix_chars`/`take_suffix_chars`；
- `rust/scanner_contract/src/lib.rs`：`COMPRESSION_POLICY_VERSION` bump；
- `config/settings.example.yaml`、`config/settings.windows.yaml`：新默认值 + `.pdf` 超时 120s；
- 测试：`rust/scanner_core` 单测/集成新增用例；必要时 `tests/` 下 Python 断言随新默认值调整；
- 文档：`docs/scanner-backends.md`、`CLAUDE.md`。

契约 wire 形状、`ContextDecision`、预算模型 `BudgetError` 语义均零改动。
