# Light Text Parser Design

Status: REVIEW_READY
Mode: Brainstorming

## 目标

在现有 scanner cache / direct lane 工作基础上，新增一个 Python 内置的轻量 text-like 解析器，优先降低 `.md`、`.txt`、`.json`、`.log`、`.csv` 文件的解析耗时。

本设计只解决扫描摘要场景。它不是高保真文档转换器，也不是 Markdown AST 解析器。目标是用有上限的读取和轻结构提取，快速生成日报、周报扫描链路可消费的 `FileContext`，同时保留现有缓存、审计和错误模型。

第一版不引入 Rust，不接入 MarkItDown，不改 PDF / Office / 图片 / 音频等复杂格式解析。MarkItDown 后续可作为 rich-document backend 的参考或可选适配层，但不进入本轮实现范围。

## 输入

- scanner 配置：
  - `worker_lane_mode`
  - `allowed_extensions`
  - `ignored_patterns`
  - `excluded_dirs`
  - `text_max_chars`
  - `summary_text_max_chars`
  - 新增或调整的轻量解析读取预算
- `FileScanner._extract_uncached_content()` 传入的：
  - `file_path`
  - `file_type`
  - full / summary 模式下的 `limits`
- `ScanPlanner.build_parser_profile()` 生成的 parser profile。
- 现有 `FileContext` 输出模型和 scan run / extension metrics 持久化链路。

## 输出

- `.md`、`.txt`、`.json`、`.log`、`.csv` 文件优先走轻量解析器，不再因为文件总大小超过旧的 `direct_text_max_bytes` 阈值而回退到 subprocess。
- 大 text-like 文件只读取配置上限内的内容，输出中保留截断标记或截断说明。
- `.json` 和 `.csv` 在可解析时输出轻结构摘要；不可解析时回退为 plain text excerpt，并记录 warning 或 error 信息。
- 非 text-like 文件继续沿用现有 timeout / subprocess 解析路径，避免扩大复杂格式解析风险。
- parser profile 纳入轻量解析器版本和读取预算，避免参数变化后错误复用旧缓存。
- benchmark 能区分 direct light parser 与 subprocess parser 的数量和耗时，便于验证收益。

## 事实基础

当前 scanner 已经有 text-like direct lane：

- text-like 扩展名为 `.txt`、`.md`、`.csv`、`.json`、`.log`。
- direct lane 由 `worker_lane_mode = "direct"` 控制。
- 当前默认 `direct_text_max_bytes = 64 * 1024`。
- 超过阈值的 text-like 文件会回退到 subprocess timeout lane。

最近两周范围排查显示，扫描样本中存在大量 `RAG_knowledge\data\maodocs` 下的 Markdown / JSON / TXT 文件。warm cache 后仍有一批大 text-like 文件进入 subprocess 或 error cache 路径。这个现象说明优化点不是单纯把 parser 语言换成 Rust，而是要把 text-like 文件从“按文件总大小决定是否 direct”改为“按读取预算 bounded direct parse”。

MarkItDown 的定位是 Python 工具，用于把多种文件和 Office 文档转换为 Markdown，面向 LLM 和文本分析流水线。它适合后续作为复杂格式转换 backend 的参考，但对本轮已经是文本类文件的扫描摘要，不应作为第一依赖。

## 设计原则

1. Correctness 优先。不能为了速度跳过 source version、parser profile 或 error cache 的正确性判断。
2. 读取必须有硬上限。轻量解析器允许处理大文件，但只能读取配置预算内的内容。
3. 输出必须可审计。解析失败、编码失败、JSON / CSV fallback 都要体现在 `FileContext` 或 scan detail 中。
4. 不扩复杂格式范围。PDF、Office、图片、音频等格式继续走现有隔离路径。
5. 不提前 Rust 化。只有在完成范围剪枝、bounded direct parse 和 benchmark 后仍证明 Python 文本处理 CPU-bound，才考虑 Rust backend。
6. 模块边界清晰。`FileScanner` 只做路由，轻量解析细节放入独立服务模块。

## 架构设计

### 1. 新增轻量解析模块

新增模块：

```text
src/services/light_text_parser.py
```

建议公开函数或类：

```python
def parse_text_like_file(
    file_path: Path,
    file_type: str,
    limits: dict[str, Any],
    options: LightTextParserOptions,
) -> FileContext:
    ...
```

也可以用 `LightTextParser` 类承载 options。第一版保持轻量，不需要抽象成复杂插件系统。

`LightTextParserOptions` 最少包含：

- `read_head_bytes`
- `read_tail_bytes`
- `max_output_chars`
- `encoding`
- `parser_backend_version`

### 2. FileScanner 路由调整

`FileScanner._extract_uncached_content()` 当前在 `_should_parse_direct()` 返回 true 时走 direct parse，否则走 subprocess。

新设计改为：

- text-like 文件始终优先尝试 light parser，只要 `worker_lane_mode = "direct"`。
- light parser 不再以文件总大小作为 direct 资格判断。
- 文件总大小只作为 metadata 和 `truncated` 判断依据。
- 非 text-like 文件仍走 `_extract_content_with_timeout()`。

旧的 `direct_text_max_bytes` 语义容易误导，因为它像是“最大文件大小”。本轮建议新增配置 `direct_text_read_bytes`，表示“最多读取多少字节”。为了兼容旧配置，可以短期支持：

1. 优先读取 `direct_text_read_bytes`。
2. 如果不存在，则回退读取旧的 `direct_text_max_bytes`。
3. parser profile 中记录最终生效值。

### 3. 按扩展名的轻结构策略

`.md`：

- 读取文件头部。
- 提取第一个 Markdown 标题。
- 保留前若干非空段落。
- 输出截断说明。

`.txt`：

- 读取文件头部。
- 规整空行和过长行。
- 输出 plain text excerpt。

`.log`：

- 优先读取文件尾部，保留最近上下文。
- 如果文件小于 tail budget，可以读取全部预算内内容。
- 输出中标记这是 tail excerpt，避免误解为完整日志。

`.json`：

- 读取文件头部。
- 如果预算内内容可以完整解析为 JSON：
  - object 输出顶层 keys 和简短字段预览。
  - list 输出长度和前几项类型 / keys。
- 如果无法完整解析：
  - 回退为 plain text excerpt。
  - 添加 `JSON_PREVIEW_FALLBACK` warning。

`.csv`：

- 使用标准库 `csv` 读取预算内文本。
- 输出表头和前几行。
- 不使用 pandas 全量加载。
- 解析失败时回退 plain text excerpt，并添加 `CSV_PREVIEW_FALLBACK` warning。

### 4. 缓存与 parser profile

轻量解析器会改变输出内容，因此必须进入 parser profile。建议增加：

- `text_parser_backend = "light_text_v1"`
- `direct_text_read_bytes`
- `log_tail_read_bytes`
- `text_excerpt_max_chars`
- `json_preview_enabled`
- `csv_preview_enabled`

这样当读取预算、输出上限或结构策略变化时，cache 会自然触发重新解析。不能让旧 cache 在参数变化后继续命中。

### 5. 配置建议

建议新增配置：

```toml
[scanner]
worker_lane_mode = "direct"
direct_text_read_bytes = 262144
log_tail_read_bytes = 262144
text_excerpt_max_chars = 12000
```

默认值保持保守：

- `direct_text_read_bytes`: 256 KiB
- `log_tail_read_bytes`: 256 KiB
- `text_excerpt_max_chars`: 沿用或低于现有 `text_max_chars`

如果配置非法，回退默认值并记录 warning，不能让 scanner 因配置类型问题静默偏离。

### 6. Benchmark 可观测性

本轮建议给 benchmark 最小补充以下字段：

- `parser_backend`：例如 `light_text_v1` / `subprocess`
- `direct_count`
- `subprocess_count`
- `truncated_count`
- 按扩展名聚合的 direct / subprocess 数量

这些指标用于验证方案 A 是否真正减少 subprocess 解析，而不是只看总耗时。

## 错误处理

轻量解析器不得抛出未包装异常到扫描主流程。它应把错误转换为可审计的 `FileContext`。

建议错误码：

- `FILE_READ_FAILED`
- `TEXT_DECODE_FAILED`
- `JSON_PREVIEW_FALLBACK`
- `CSV_PREVIEW_FALLBACK`
- `LIGHT_TEXT_PARSE_FAILED`

其中 JSON / CSV fallback 更像 warning。如果现有 `FileContext` 没有 warning 字段，可以先把说明写入 content metadata 或 parse detail，避免第一版改动模型过大。

## 测试计划

新增或更新测试：

1. 小 `.md` 文件走 light parser，不调用 subprocess。
2. 大 `.md` 文件也走 light parser，只读取预算内内容，输出截断标记。
3. `.txt` 文件按 head budget 输出 plain text excerpt。
4. `.log` 文件按 tail budget 输出最近日志片段。
5. 合法 `.json` 输出顶层结构摘要。
6. 非法或截断 `.json` 回退 text excerpt，并产生可审计 fallback 信息。
7. `.csv` 输出表头和前几行，不调用 pandas 全量加载。
8. 非 text-like 文件继续走 subprocess timeout lane。
9. parser profile 参数变化会导致 cache miss / reparse。
10. benchmark 输出包含 direct / subprocess / truncated 聚合指标。

## 验收标准

- 两周范围 benchmark 中，text-like 文件的 subprocess 数量明显下降。
- 大 `.md` / `.json` / `.log` 文件不再仅因总大小超过 64 KiB 进入 subprocess。
- `ScanResult`、daily、weekly、monthly 的上层业务契约不变。
- 解析失败仍进入 error cache 或可审计结果，不静默吞掉。
- pytest 中 scanner、planner、benchmark 相关测试通过。
- benchmark JSON / Markdown 能解释 direct light parser 的使用数量和截断数量。

## 非目标

- 不做 Rust parser。
- 不接入 MarkItDown。
- 不改变 PDF / DOCX / PPTX / XLSX / 图片 / 音频解析策略。
- 不引入常驻 worker pool。
- 不引入 Markdown AST 或完整语义解析。
- 不改变 scan discovery 的目录剪枝逻辑。目录剪枝是另一个独立优化项，应单独实现和验证。

## 后续扩展

如果方案 A benchmark 通过，后续可以考虑：

- 把 MarkItDown 作为 `rich_document_backend` 接入复杂格式。
- 为极大日志文件增加更细的 tail sampling 策略。
- 为 JSONL 增加专门 preview。
- 如果 Python 轻量解析仍被证明 CPU-bound，再评估 Rust backend：
  - PyO3 / maturin 嵌入式模块；
  - 或 Rust CLI worker；
  - 但必须先证明收益超过 Windows / conda / CI 分发复杂度。

## 伪代码草案

```python
# 目标：为 text-like 文件提供有上限、可审计、低开销的扫描解析
# 输入：
# - file_path: 本地文件路径
# - file_type: 扩展名，例如 .md/.json/.log/.csv
# - limits: full/summary 模式下的内容长度限制
# - scanner_cfg: direct_text_read_bytes、log_tail_read_bytes 等配置
# 输出：
# - FileContext: 成功时包含摘要内容、metadata、truncated 标记
# - FileContext(error=...): 失败时包含错误原因，进入现有审计链路

def extract_uncached_content(file_path, file_type, limits, scanner_cfg):
    # 为什么先判断 text-like：这类文件扫描只需要摘要，不需要启动子进程做完整转换。
    if is_text_like(file_type) and scanner_cfg.worker_lane_mode == "direct":
        options = build_light_text_options(scanner_cfg, limits)
        return parse_text_like_file(file_path, file_type, limits, options)

    # 非文本格式仍保留 timeout/subprocess 隔离，避免复杂解析阻塞主进程。
    return extract_content_with_timeout(file_path, limits)


def parse_text_like_file(file_path, file_type, limits, options):
    try:
        stat_result = file_path.stat()

        if file_type == ".log":
            # 日志最有价值的信息通常在尾部，所以读取最近片段。
            raw_text, truncated = read_tail_text(
                file_path,
                max_bytes=options.read_tail_bytes,
                encoding=options.encoding,
            )
        else:
            # 普通文本和结构化文本先读头部，控制 I/O 上限。
            raw_text, truncated = read_head_text(
                file_path,
                max_bytes=options.read_head_bytes,
                encoding=options.encoding,
            )

        # byte 上限控制 I/O，char 上限控制进入报告链路的 token 规模。
        excerpt = clamp_chars(raw_text, options.max_output_chars)

        if file_type == ".json":
            return parse_json_preview_or_text_fallback(
                file_path=file_path,
                text=excerpt,
                truncated=truncated,
                stat_result=stat_result,
                options=options,
            )

        if file_type == ".csv":
            return parse_csv_preview_or_text_fallback(
                file_path=file_path,
                text=excerpt,
                truncated=truncated,
                stat_result=stat_result,
                options=options,
            )

        return build_text_context(
            file_path=file_path,
            content=summarize_plain_text(excerpt, file_type, truncated),
            parser_backend=options.parser_backend_version,
            truncated=truncated,
            stat_result=stat_result,
        )

    except UnicodeDecodeError as exc:
        # 编码失败必须可审计，否则 benchmark success 会虚高。
        return build_error_context(
            file_path=file_path,
            error_code="TEXT_DECODE_FAILED",
            message=str(exc),
            retryable=False,
        )

    except OSError as exc:
        # 文件读取失败可能来自临时文件被删除或权限变化，保留 retryable 语义。
        return build_error_context(
            file_path=file_path,
            error_code="FILE_READ_FAILED",
            message=str(exc),
            retryable=True,
        )

    except Exception as exc:
        # 未知异常兜底进入审计结果，不能穿透到 scanner 主流程。
        return build_error_context(
            file_path=file_path,
            error_code="LIGHT_TEXT_PARSE_FAILED",
            message=str(exc),
            retryable=False,
        )
```

## 参考

- Microsoft MarkItDown: https://github.com/microsoft/markitdown
