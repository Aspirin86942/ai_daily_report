# Context Scheduler 设计草案

## 目标

在现有 scanner / parser / cache 基础上，新增一个可复用的文件上下文调度与压缩层：`context_scheduler.py`。它服务于 `daily`、`weekly`、`monthly` 三类 CLI 报告入口，负责把一次手动 CLI 运行中的文件证据转换成稳定、可审计、受预算约束的 LLM 上下文。

本阶段目标不是做后台服务，而是让每次手动 CLI run 都具备“策略调度”能力：

- 根据报告模式、日期范围、文件类型、文件大小、parser metadata、cache 状态和上下文预算选择处理策略。
- 对小文件尽量保留正文，对中大型文件做确定性压缩，对超大或低优先级文件只保留 metadata / omission audit。
- 把每个文件为什么被保留、压缩、只保留 metadata 或省略写入 SQLite，保证后续 benchmark 和排障可复核。
- 保持现有 scanner/parser 语义不退化，不破坏 `light_text_v1`、`office_v1`、`pdf_text_v1`、parse cache 和 scanner benchmark。

## 当前现状

当前项目已经具备较清晰的 scanner 边界：

- `FileScanner.scan_files()` 负责编排 discovery -> inventory/cache -> parse -> aggregation。
- `ScanPlanner` 负责 parser profile、summary mode 预算和 cache planning。
- `ScanIndexStore` 已承载 inventory、parse cache、scan run metrics、extension metrics 等持久化状态。
- `ScanAggregator` 当前只按 `total_max_chars` 做全局字符预算截断，超过预算后后续文件正文直接替换为“已达全局字符上限，内容省略”。
- `main.py` 的 scan source 入口目前直接调用 `FileScanner().scan_files(..., summary_mode=True)`，再用 `build_file_context(scan_result)` 把所有 `FileContext` 简单拼接给 LLM。

parser 层当前状态：

- text-like 文件：`.md/.txt/.log/.csv/.json` 走 bounded `light_text_v1`。
- Office/PDF：`.docx/.xlsx/.pptx` 走 `office_v1`，`.pdf` 走 `pdf_text_v1`，并继续通过 subprocess lane 保留 hard timeout。
- `.xls` 仍保留旧路径，不纳入本次 context scheduler 的 parser 改造范围。
- `FileContext` 已包含 `parser_backend`、`truncated`、`error`，可以作为 context scheduler 的决策输入。

现有缺口：

- 只有全局截断，没有真正的上下文压缩策略。
- 文件进入 LLM 上下文的顺序和取舍缺少独立、可审计的策略层。
- scanner benchmark 能看 parser backend，但看不到“最终给 LLM 的上下文被压缩了多少、哪些文件被省略、为什么省略”。
- parser profile 和未来 context/compression profile 尚未分离。

## 推荐方案

采用方案 B：新增 `context_scheduler.py` 作为文件上下文调度层，新增 `context_compressor.py` 作为确定性压缩层，并扩展 `ScanIndexStore` 保存 context run 与文件级 decision。

推荐结构：

```text
CLI daily / weekly / monthly
  ↓
main.py 解析命令参数和日期范围
  ↓
ContextScheduler
  ↓
FileScanner
  ↓
ScanResult
  ↓
ContextCompressor
  ↓
CompressedContext
  ↓
LLMClient.generate_report / generate_weekly_report / generate_monthly_report
```

`ContextScheduler` 是一次 CLI run 内的短生命周期调度器，不是后台 daemon。它通过 SQLite/index/cache 获取长期记忆，通过确定性策略决定本次文件上下文如何构建。

## 不做范围

本阶段明确不做：

- 不做常驻后台服务。
- 不做定时扫描。
- 不做 GUI / Web 前后端。
- 不做系统托盘、桌面模拟或后台守护进程。
- 不引入任务队列。
- 不引入 `asyncio` orchestration。
- 不调用 LLM 做压缩。
- 不做 embedding / 向量索引。
- 不做 OCR。
- 不改变现有 parser 的 bounded extraction 行为。
- 不改变 `light_text_v1` 的文本解析语义。
- 不把 Office/PDF parser 改成 direct in-process；仍保留 subprocess timeout 隔离。

## 架构边界

### ContextScheduler

职责：

- 接收 `ContextScheduleRequest`，包含 report mode、日期范围、source、compression profile 等。
- 调用 `FileScanner.scan_files()` 获取 `ScanResult`。
- 根据文件类型、大小、parser metadata、error/truncated、报告模式和预算生成 `ContextDecision`。
- 调用 `ContextCompressor` 生成 `CompressedContext`。
- 把 `context_runs` 和 `context_decisions` 写入 `ScanIndexStore`。
- 返回 `ContextScheduleResult` 给 CLI/report pipeline。

非职责：

- 不实现 discovery。
- 不实现具体 parser。
- 不直接调用 LLM。
- 不渲染最终报告 Markdown。
- 不保存日报/周报/月报业务结果。

### ContextCompressor

职责：

- 接收 `ScanResult`、`ContextDecision`、`ContextProfile`。
- 生成固定结构的 Markdown-like LLM 上下文。
- 执行 per-file 和 global context budget。
- 生成 included / omitted / metadata_only / compressed / error / truncated 统计。
- 返回 `CompressedContext`。

非职责：

- 不读取文件系统重新解析正文。
- 不修改 parse cache。
- 不判断 parser profile。
- 不调用 LLM。

### ScanIndexStore

第一版继续扩展 `ScanIndexStore`，不急于新建并行 storage service。

原因：

- parse cache、inventory、scan run、context run 共享 `file_identity/source_version/parser_profile` 等语义。
- 过早拆分存储层容易出现两套 cache key 和审计口径。
- 后续如果表数量和职责明显膨胀，再拆出 `context_index_store.py`。

## Parser Backend 关系

context scheduler 不替代 parser backend，只消费 parser 输出。

- parser profile：决定文件如何读取、解析、截断。变化后需要重新 parse。
- context profile：决定已解析内容如何排序、压缩、进入 LLM 上下文。变化后通常只需要重新压缩，不应触发 Office/PDF 全量重解析。

必须保持两类 profile 分离：

```text
parser_profile_key
  -> parse_cache 维度

context_profile_key
  -> context_runs / future context_cache 维度
```

## 文件分类与策略规则

第一版文件规模分四档：

```text
small:
  小文本、小 Markdown、小 JSON、小 CSV
  策略：尽量保留内容。

medium:
  普通 Office/PDF、较长 Markdown、较长日志
  策略：bounded parse + 规则压缩。

large:
  大 Excel、大 PDF、大日志、大文档
  策略：summary profile / per-file compression / metadata supplement。

huge:
  超过 max_file_size_mb 或明显不适合本轮读取
  策略：不解析正文，只保留 metadata 和审计原因。
```

分类维度：

- 文件扩展名。
- 文件大小。
- parser backend。
- parse error。
- truncated。
- cache status。
- report mode：daily / weekly / monthly。
- 全局上下文预算压力。
- 路径优先级，例如 logs、benchmarks、cache 目录默认低优先级。

第一版 action：

```text
keep:
  小文件或高优先级文件，尽量保留 parser 输出。

compress:
  中大型文件，保留结构、前部/尾部/有限正文。

metadata_only:
  超大文件、错误文件、低优先级文件或预算不足文件，只保留路径、类型、大小、原因。

omit:
  不进入正文，但必须进入 context_decisions 和省略摘要。
```

建议策略表：

```text
fresh cache:
  action = reuse_cache + downstream keep/compress decision
  reason = cache_fresh

small text-like:
  action = keep
  reason = small_file_keep

medium text-like:
  action = compress
  reason = medium_text_compress

large log:
  action = compress
  reason = large_log_tail

small/medium docx:
  action = compress
  reason = document_preview

small/medium xlsx:
  action = compress
  reason = sheet_preview

small/medium pptx:
  action = compress
  reason = slide_preview

small/medium pdf:
  action = compress
  reason = pdf_text_preview

large office/pdf:
  action = compress 或 metadata_only
  reason = large_document_summary

huge / over max_file_size_mb:
  action = metadata_only
  reason = file_size_policy
```

排序必须稳定，不能依赖 parser 线程完成顺序。第一版排序规则：

```text
1. 非 error 文件优先，但 error 文件要保留审计说明。
2. 非 logs / 非 benchmark / 非 cache 目录优先。
3. Office/PDF 和 Markdown/text 优先于 logs/json benchmark 输出。
4. 同优先级按 modified time 降序。
5. 再按 path 升序，保证 benchmark 可复现。
```

## 数据模型

建议新增模型：

```python
@dataclass(slots=True)
class ContextScheduleRequest:
    report_mode: str
    source: str
    start_date: date
    end_date: date
    compression_profile: str
    user_input: str | None = None


@dataclass(slots=True)
class ContextDecision:
    file_path: str
    extension: str
    size_bytes: int | None
    parser_backend: str | None
    worker_lane: str | None
    cache_status: str
    action: str
    reason: str
    priority: int
    input_chars: int
    output_chars: int
    truncated: bool
    error: str | None


@dataclass(slots=True)
class CompressedContext:
    content: str
    source_file_count: int
    included_file_count: int
    omitted_file_count: int
    metadata_only_count: int
    compressed_file_count: int
    error_file_count: int
    truncated_file_count: int
    input_chars: int
    output_chars: int
    warnings: list[str]
    decisions: list[ContextDecision]


@dataclass(slots=True)
class ContextScheduleResult:
    file_context: str
    compressed_context: CompressedContext
    scan_result: ScanResult | None
    context_run_id: int | None
    decisions: list[ContextDecision]
    error: str | None = None
```

后续可根据项目风格改为 Pydantic model；第一版若只在 service 内部使用，`dataclass(slots=True)` 足够。

## 压缩输出格式

`CompressedContext.content` 输出固定 Markdown-like 结构：

````markdown
# 文件证据上下文

## 本轮摘要
- 日期范围：2026-05-10 ~ 2026-05-24
- 报告模式：weekly
- 扫描文件数：783
- 纳入上下文：42
- 仅保留元数据：12
- 因预算省略：729
- 解析错误：0
- 截断文件：11

## 重要提示
- 部分文件因全局上下文预算被省略。
- Office/PDF 仅提取文本层或结构化预览，不做 OCR。
- Excel 仅保留有限 sheet / row / column 预览。

## 文件证据

### 1. D:\...\行业学习-激光行业.xlsx
- 类型：.xlsx
- parser_backend：office_v1
- 策略：compress
- 原因：large_document_summary
- 截断：是

```text
## Sheet: ...
| 字段 | 值 |
...
```

## 省略文件摘要
- 因低优先级和预算限制省略：729 个
- 主要类型：.log 120, .json 80, .md 529

## 解析问题
- 无
````

输出要求：

- LLM 必须能看到上下文是否完整。
- 每个进入正文的文件必须带 parser backend、策略、原因和截断状态。
- 被省略文件不能静默消失，至少要出现在 omitted summary 和 `context_decisions`。
- Office/PDF 能力边界必须明确提示：PDF 只读 text layer，不做 OCR。

## 配置项 / Context Profile

第一版可以先内置默认值，后续再暴露到 `config/settings.toml`。建议 context profile 包含：

```json
{
  "version": "context_scheduler_v1",
  "report_mode": "weekly",
  "compression_profile": "weekly_balanced_v1",
  "global_context_max_chars": 50000,
  "per_file_max_chars": 5000,
  "small_file_max_bytes": 65536,
  "medium_file_max_bytes": 1048576,
  "large_file_max_bytes": 10485760,
  "priority_policy": "default_v1",
  "compression_policy": "markdown_context_v1"
}
```

建议默认预算：

```text
daily:
  global_context_max_chars = 50000
  per_file_max_chars = 8000

weekly:
  global_context_max_chars = 50000
  per_file_max_chars = 5000

monthly:
  global_context_max_chars = 60000
  per_file_max_chars = 4000
```

`monthly` 总预算可略大，但单文件预算更小，因为月报更需要覆盖面，而不是单个文件细节。

## Cache / Metrics / Benchmark 影响

### Cache

第一版暂不做强 `context_cache`。原因：

- parse cache 已经覆盖主要成本。
- compressor 第一版是纯 Python 确定性规则，运行成本较低。
- 先把 `context_profile_key`、`context_runs`、`context_decisions` 设计清楚，避免过早缓存错误结果。

后续如果引入 LLM 压缩、本地模型摘要或 embedding，可新增：

```text
context_cache:
  context_profile_key
  input_fingerprint
  content
  metadata_json
  created_at
```

### Metrics

新增 context summary：

```text
context_run_id
compression_profile
context_profile_key
source_file_count
included_file_count
omitted_file_count
metadata_only_count
compressed_file_count
error_file_count
truncated_file_count
input_chars
output_chars
compression_ratio
decision_action_summary
decision_reason_summary
parser_backend_summary
```

### Benchmark

建议新增脚本：

```powershell
conda run -n test python scripts\benchmark_context_scheduler.py --start-date 2026-05-09 --end-date 2026-05-24 --report-mode weekly --json-out data\benchmarks\context_scheduler_2026-05-24.json --markdown-out data\benchmarks\context_scheduler_2026-05-24.md
```

不建议第一版直接扩展 `benchmark_scanner.py`，避免 scanner parser 性能证据和 context 压缩证据混在一起。

benchmark JSON 示例：

```json
{
  "parameters": {
    "start_date": "2026-05-09",
    "end_date": "2026-05-24",
    "report_mode": "weekly",
    "compression_profile": "weekly_balanced_v1"
  },
  "scan_metrics": {
    "discovered_count": 783,
    "reused_count": 780,
    "reparsed_count": 3,
    "parse_duration_ms": 29
  },
  "context_scheduler_summary": {
    "source_file_count": 783,
    "included_file_count": 40,
    "omitted_file_count": 743,
    "metadata_only_count": 12,
    "compressed_file_count": 28,
    "error_file_count": 0,
    "truncated_file_count": 11,
    "input_chars": 180000,
    "output_chars": 48000,
    "compression_ratio": 0.266,
    "actions": {
      "keep": 8,
      "compress": 20,
      "metadata_only": 12,
      "omit": 743
    },
    "parser_backends": {
      "light_text_v1": 39,
      "office_v1": 1
    }
  }
}
```

## 存储层设计

新增 `context_runs`：

```text
id
created_at
report_mode
start_date
end_date
compression_profile
context_profile_key
scan_run_id
source_file_count
included_file_count
omitted_file_count
metadata_only_count
compressed_file_count
error_file_count
truncated_file_count
input_chars
output_chars
duration_ms
status
error
```

新增 `context_decisions`：

```text
id
context_run_id
file_identity
path
extension
size_bytes
parser_backend
worker_lane
cache_status
action
reason
priority
input_chars
output_chars
truncated
error
```

建议 `ScanIndexStore` 增加方法：

```text
save_context_run(...)
save_context_decisions(...)
latest_context_run()
list_context_decisions(context_run_id)
```

空扫描也必须写 `context_runs`，避免“到底有没有运行过”不可审计。

## 错误处理与审计

必须满足：

- compressor 出错不能静默失败。
- 单文件压缩失败时，保留该文件的 error decision。
- 整体压缩失败时，`ContextScheduler` 写入 status=`error` 的 `context_runs`。
- scan_result 为空时，仍返回“无文件证据”，并写入 source_file_count=0 的 context run。
- 全部文件都被省略时，`file_context` 不能为空，必须写明“无可用文件证据”或预算省略原因摘要。
- parser error 文件不进入正常正文，但必须进入“解析问题”和 `context_decisions`。
- context profile 变化不能误触发 parser 全量重解析。

## 测试策略

TDD 优先，测试重点放在 scheduler/compressor/store，不重复测试 parser 内部实现。

### ContextScheduler

- daily scan source 调用 `FileScanner` 并返回 `CompressedContext`。
- weekly / monthly 使用不同 compression profile。
- scanner 返回空结果时，仍输出“无文件证据”，并记录 context run。
- scanner 有错误文件时，错误进入 `ContextDecision`。
- cached 文件的 parser metadata 能进入 decision。
- Office/PDF parser backend 能进入 decision。
- truncated 文件进入 `truncated_file_count`。

### Workload / Decision

- small `.md` -> `keep`。
- large `.log` -> `compress` 或 `metadata_only`，取决于预算。
- `.xlsx` -> document compression decision。
- 超过 size policy 的文件 -> `metadata_only`。
- `logs/`、`data/benchmarks/`、`.pytest_cache/` 优先级低于普通业务文件。
- 同优先级排序稳定，不依赖线程完成顺序。

### ContextCompressor

- 小文件内容被保留。
- 单文件超过 `per_file_max_chars` 时被截断并标记 compressed。
- 总预算超过 `global_context_max_chars` 时，后续文件进入 omitted summary。
- 输出包含“本轮摘要 / 文件证据 / 省略文件摘要 / 解析问题”。
- `output_chars` 不超过预算加固定 header 容忍值。
- `metadata_only` 文件不把正文塞进 prompt。

### Store

- `save_context_run()` 写入总体统计。
- `save_context_decisions()` 写入文件级决策。
- `latest_context_run()` 能读回。
- `list_context_decisions()` 能按 `context_run_id` 读回。
- 空扫描也写 context run。
- 压缩失败时写入 error 状态。

### main.py integration

- daily scan path 使用 `ContextScheduler`。
- weekly `--source scan` 使用 `ContextScheduler`。
- monthly `--source scan` 使用 `ContextScheduler`。
- `--source db` 不受影响。
- 旧 `build_file_context()` 保留 fallback，不破坏现有测试。

## 验收标准

- 原 scanner benchmark 仍能跑，`parser_backend_summary` 不退化。
- 新 context benchmark 能输出 `context_scheduler_summary`。
- `CompressedContext.output_chars` 受预算控制。
- `included + omitted + metadata_only` 等口径可解释。
- Office/PDF 文件正确计入 parser backend 和 decision action。
- warm run 时 parse cache 仍复用，context scheduler 不触发不必要重解析。
- 所有 context decision 可以在 SQLite 中审计。
- 不引入后台常驻进程、GUI、Web 或 async queue。

## 风险点 / 边界条件

- 如果第一版压缩规则过于复杂，会增加不可解释性。应先坚持 deterministic rules。
- 如果 context profile 与 parser profile 混用，会导致压缩策略变化误触发 parser 全量重解析。
- 如果排序依赖 `ScanResult.contexts` 的线程完成顺序，benchmark 会漂移。
- 如果 omitted 文件不落 decision，后续报告无法解释“文件为什么没进入上下文”。
- 如果直接把 `ScanIndexStore` 扩得过大，后续需要拆出 `context_index_store.py`；但第一版不应过早拆分。
- 如果把调度器命名为 Agent 并引入 Agent 抽象，容易让边界变虚。代码中应优先使用 `Scheduler`、`Compressor`、`Decision`、`Store` 等确定性命名。

## 伪代码草案

### ContextScheduler

```python
# [伪代码草案]
# 目标：在一次 CLI 运行中，根据报告模式、日期范围、文件类型、大小和缓存状态，
# 生成稳定、可审计、可压缩的 LLM 文件上下文。
# 输入：
# - request: ContextScheduleRequest，包含 report_mode、start_date、end_date、source、用户补充等
# - scanner: FileScanner，负责现有 discovery/cache/parse 流程
# - store: ScanIndexStore，负责 scan/context run 和 decision 落库
# - compressor: ContextCompressor，负责把 ScanResult 压缩成 Markdown-like context
# 输出：
# - ContextScheduleResult，包含 file_context、CompressedContext、scan_result、context_run_id、decisions
# - 失败时返回带 error 状态的 context_run，并给调用方一个可读错误

def build_context(request, scanner, store, compressor):
    # 1. 输入校验：调度器是 CLI run 的编排层，必须先确认日期和模式有效，
    # 避免错误请求进入 scanner 后才以更难审计的形式失败。
    validate_context_request(request)

    started_at = now()
    context_profile = build_context_profile(request)
    context_profile_key = serialize_context_profile(context_profile)

    try:
        # 2. 扫描解析：scanner 继续负责 discovery/cache/parser，不把底层职责搬进调度器。
        # 为什么这样做：scanner 刚完成性能优化和 parser backend 接线，调度器只做策略层，避免破坏稳定路径。
        scan_result = scanner.scan_files(
            start_date=request.start_date,
            end_date=request.end_date,
            summary_mode=resolve_summary_mode(request.report_mode),
        )

        # 3. 构建文件级决策：基于 scan_result 和文件系统 metadata 生成 keep/compress/metadata_only/omit。
        # 为什么这样做：每个文件为什么进入或没进入 LLM 上下文，都要能在 SQLite 中审计。
        decisions = []
        for context in scan_result.contexts:
            file_meta = load_file_metadata(context.file_path)
            classification = classify_workload(
                file_type=context.file_type,
                size_bytes=file_meta.size_bytes,
                parser_backend=context.parser_backend,
                truncated=context.truncated,
                error=context.error,
                report_mode=request.report_mode,
            )
            decision = choose_context_action(
                context=context,
                file_meta=file_meta,
                classification=classification,
                context_profile=context_profile,
            )
            decisions.append(decision)

        # 4. 稳定排序：压缩顺序不能依赖线程完成顺序，否则 benchmark 和报告上下文会漂移。
        decisions = sort_decisions_by_priority(decisions)

        # 5. 压缩上下文：compressor 只做确定性规则压缩，不调用 LLM。
        # 为什么这样做：第一版要保持成本可控、行为可测，避免引入新的不稳定链路。
        compressed_context = compressor.compress(
            scan_result=scan_result,
            decisions=decisions,
            profile=context_profile,
        )

        # 6. 落库 context_run：即使没有文件，也要写 run 记录，避免“到底有没有运行过”不可审计。
        context_run_id = store.save_context_run(
            report_mode=request.report_mode,
            start_date=request.start_date,
            end_date=request.end_date,
            context_profile_key=context_profile_key,
            scan_run_id=resolve_latest_scan_run_id(store),
            summary=compressed_context.to_summary(),
            status="success",
            duration_ms=elapsed_ms(started_at),
            error="",
        )

        # 7. 落库 decisions：文件级策略必须持久化，方便 benchmark、排障和后续调参。
        store.save_context_decisions(
            context_run_id=context_run_id,
            decisions=compressed_context.decisions,
        )

        return ContextScheduleResult(
            file_context=compressed_context.content,
            compressed_context=compressed_context,
            scan_result=scan_result,
            context_run_id=context_run_id,
            decisions=compressed_context.decisions,
            error=None,
        )

    except Exception as exc:
        # 8. 兜底异常：调度层不能静默失败，要写入 error run，并返回可读错误上下文。
        # 为什么这样做：日报/周报生成失败时，需要知道是 scan、compress 还是 store 阶段出了问题。
        context_run_id = store.save_context_run(
            report_mode=request.report_mode,
            start_date=request.start_date,
            end_date=request.end_date,
            context_profile_key=context_profile_key,
            scan_run_id=None,
            summary=empty_context_summary(),
            status="error",
            duration_ms=elapsed_ms(started_at),
            error=str(exc),
        )

        fallback_context = build_error_file_context(exc)

        return ContextScheduleResult(
            file_context=fallback_context,
            compressed_context=CompressedContext.empty(error=str(exc)),
            scan_result=None,
            context_run_id=context_run_id,
            decisions=[],
            error=str(exc),
        )
```

### ContextCompressor

```python
# [伪代码草案]
# 目标：把 scanner 输出的 FileContext 列表压缩成固定结构的 Markdown-like LLM 上下文。
# 输入：
# - scan_result: scanner 输出，包含文件内容、error、parser_backend、truncated
# - decisions: 调度器生成的文件级动作和优先级
# - profile: 全局预算、单文件预算、报告模式、压缩策略版本
# 输出：
# - CompressedContext，包含 content 和完整统计

def compress(scan_result, decisions, profile):
    output_parts = []
    output_chars = 0
    included = []
    omitted = []
    warnings = []

    # 1. 先构建摘要头：LLM 需要知道上下文是否完整，避免把压缩结果误认为全集。
    header = build_context_header(scan_result, decisions, profile)
    output_parts.append(header)
    output_chars += len(header)

    for decision in decisions:
        context = find_context(scan_result, decision.file_path)

        if decision.action == "omit":
            omitted.append(decision)
            continue

        if decision.action == "metadata_only":
            block = render_metadata_only_block(decision, context)
        elif decision.action == "keep":
            block = render_keep_block(decision, context, profile.per_file_max_chars)
        elif decision.action == "compress":
            block = render_compressed_block(decision, context, profile.per_file_max_chars)
        else:
            block = render_error_block(decision, context)

        # 2. 全局预算门禁：预算满后不再追加正文，但保留 omitted decision。
        # 为什么这样做：保证 LLM prompt 可控，同时让被省略文件仍有审计记录。
        if output_chars + len(block) > profile.global_context_max_chars:
            decision.action = "omit"
            decision.reason = "global_budget_exceeded"
            omitted.append(decision)
            continue

        output_parts.append(block)
        output_chars += len(block)
        included.append(decision)

    # 3. 省略摘要：被压缩或省略的文件不能静默消失。
    omitted_summary = render_omitted_summary(omitted)
    if output_chars + len(omitted_summary) <= profile.global_context_max_chars:
        output_parts.append(omitted_summary)
        output_chars += len(omitted_summary)
    else:
        warnings.append("省略摘要因全局预算不足被缩短")

    content = "\n\n".join(output_parts).strip() or "无文件证据"

    return CompressedContext(
        content=content,
        source_file_count=scan_result.total_files,
        included_file_count=len(included),
        omitted_file_count=len(omitted),
        metadata_only_count=count_action(decisions, "metadata_only"),
        compressed_file_count=count_action(decisions, "compress"),
        error_file_count=count_errors(scan_result),
        truncated_file_count=count_truncated(scan_result),
        input_chars=sum(len(ctx.content) for ctx in scan_result.contexts),
        output_chars=len(content),
        warnings=warnings,
        decisions=decisions,
    )
```

## Implementation Notes

以下为 Task 1-5 已落地实现的对齐说明，只记录当前实现事实：

- context profile 使用 `ContextProfile` 表示，作为调度与压缩共享的 profile 对象。
- `ContextDecision` 定义在 `context_compressor.py`，因为 scheduler、compressor、store 复用同一个文件级决策模型。
- 第一版未实现 `context_cache`，持久化范围为 `context_runs` 和 `context_decisions`。
- 成功路径通过 `ScanIndexStore.save_context_run_with_decisions(...)` 原子写入 context run 与 decisions，避免 run / decision 审计不一致；`save_context_run()` 和 `save_context_decisions()` 保留给错误路径或测试使用。
- `ScanResult` 显式携带 `scan_run_id`，context run 绑定该 ID；避免多个 CLI run 共享 SQLite 时通过 latest scan run 串号。
- benchmark 绑定本次 `ContextScheduler` run：通过 `get_context_run(context_run_id)` 和 `get_scan_run_detail(scan_run_id)` 读取 payload，不以全局 latest 作为真相源。
- benchmark summary 包含 `action_counts` 与 `parser_backend_counts`，用于证明 scheduler 策略和 parser backend 分布。
- compressor 最终预算收口只截断尾部审计摘要，不把已进入正文预算的文件证据整体替换成“预算不足”。
- CLI scan 路径会消费 `ContextScheduleResult.error` 并打印 / 记录降级 warning，随后使用 fallback context 继续生成报告。

## 实施顺序建议

等用户 review 并确认后，再进入 implementation plan。建议顺序：

1. 写 `ContextScheduler` / `ContextCompressor` 模型和失败测试。
2. 扩展 `ScanIndexStore`：`context_runs` / `context_decisions`。
3. 实现 `ContextCompressor` 的确定性输出。
4. 实现 `ContextScheduler` 调用 scanner + compressor + store。
5. 接入 `main.py` 的 daily / weekly / monthly scan source。
6. 新增 `scripts/benchmark_context_scheduler.py`。
7. 跑单测、compileall、scanner benchmark、context scheduler benchmark。
