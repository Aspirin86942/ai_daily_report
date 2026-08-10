# Rust Office Oxide Parser Backend Design

Status: REVIEW_READY
Mode: Brainstorming

## 目标

用 `office_oxide` 作为激进主路径替换当前 Office 文件的 Python per-file subprocess parser，降低无缓存扫描时的 Office 解析固定成本，同时保留现有 scanner 的并行、timeout、cache、benchmark 和可审计错误契约。

本阶段覆盖 Office 文件，不覆盖 PDF：

- Rust 主路径：`.docx`、`.xlsx`、`.pptx`、`.doc`、`.xls`、`.ppt`
- Python fallback：Rust 失败时按扩展名回退到 Python 解析能力
- PDF：继续走现有 `pdf_text_v1` / `pdfplumber` 路径

第一阶段重点不是追求与旧 `office_v1` 输出逐字符一致，而是提供稳定、可截断、可缓存、可 benchmark 的 Markdown / text preview，供日报、周报和后续 LLM 上下文使用。

## 输入

- `FileScanner` 发现到的候选文件路径和扩展名。
- scanner parser profile 中的解析预算：
  - `document_excerpt_max_chars`
  - `excel_max_sheets`
  - `excel_max_rows`
  - `excel_max_columns`
  - `docx_max_paragraphs`
  - `docx_max_tables`
  - `docx_table_max_rows`
  - `docx_table_max_cols`
  - `pptx_max_slides`
  - `max_file_size_mb`
  - `file_timeout_seconds`
  - `file_timeout_by_extension`
- Rust parser CLI 路径，例如 `rust/office_parser/target/release/ai-daily-office-parser`。
- Python fallback 依赖：
  - 现有 `.docx/.xlsx/.pptx` 依赖：`python-docx`、`openpyxl`、`python-pptx`
  - 新增 legacy fallback 候选：`sharepoint-to-text`
  - 可选外部 fallback 候选：Apache Tika、LibreOffice headless

运行环境：

- Linux 本地开发环境。
- Python 3.10+，当前验证优先使用 `conda run -n test ...`。
- Rust stable toolchain。
- 不要求第一阶段支持 Windows 构建产物，但 JSON 契约不能写死 Linux-only 数据结构。

外部资料依据：

- `office_oxide` docs.rs 页面声明支持 Rust、Python、CLI、WASM 等入口，并支持 DOCX/XLSX/PPTX 与 legacy DOC/XLS/PPT。
- `office_oxide` FFI 页面提供 `office_document_open`、`office_document_plain_text`、`office_document_to_markdown` 等能力。
- `sharepoint-to-text` PyPI 页面声明 pure Python，支持现代和 legacy Microsoft Office，包括 `.doc/.xls/.ppt`。
- Apache Tika 官方格式页显示 Microsoft Office legacy / OOXML 由 Office parser 系列支持，但该路线需要 Java/Tika runtime。
- LibreOffice headless / `soffice --convert-to` 能作为外部转换兜底，但属于系统依赖，不作为默认 fallback。

## 输出

成功输出：

- `FileContext`，字段保持现有上游契约：
  - `file_path`
  - `file_type`
  - `content`
  - `error`
  - `parser_backend`
  - `truncated`

失败输出：

- Rust 和 Python fallback 都失败时，返回可审计错误，不抛出到 scanner 主流程。
- 错误必须包含至少：
  - Rust backend 错误摘要
  - Python fallback backend 错误摘要
  - 是否 timeout
  - 是否 fallback 不可用

副作用：

- Rust parser 不写 SQLite。
- Rust parser 不写 cache。
- Rust parser 不修改配置。
- Python scanner 继续负责 parse cache、metrics、aggregation 和 benchmark 输出。

## 当前现状

当前 scanner 已有明确边界：

- text-like 文件在 `worker_lane_mode = direct` 时走 `light_text_v1`，无缓存性能已经很好。
- Office/PDF 文件在 direct 模式下仍走 subprocess timeout lane。
- subprocess worker 最终会调用 `parse_document_file()`，并返回 `office_v1` / `pdf_text_v1`。
- 当前慢点不是 `parse_document_file()` 本身，而是每个 Office 文件都 spawn 一个新的 Python 子进程，并重复 import `openpyxl`、`python-docx`、`python-pptx` 等重依赖。

最近真实无缓存 benchmark：

- 发现文件数：428
- 重解析：428
- parse 阶段墙钟耗时：1204ms
- 真正走 Office subprocess 的只有 4 个文件
- `.xlsx` 两个文件累计 2283ms
- `.docx` 一个文件 1059ms
- `.pptx` 一个文件 969ms

同一 Python 进程内直接调用 `parse_document_file()` 的只读对照：

- `.xlsx`: 178ms / 76ms
- `.docx`: 22ms
- `.pptx`: 30ms

结论：优化目标应是替换 Python subprocess 内的重依赖解析路径，而不是取消分进程隔离。

## 推荐方案

### 方案 A：Rust `office_oxide` CLI 主路径 + Python fallback

这是本设计采用的方案。

新增一个 Rust CLI：

```text
rust/office_parser/
  Cargo.toml
  src/main.rs
  src/lib.rs
```

Python scanner 调用链：

```text
FileScanner
  -> _extract_document_content_with_timeout()
    -> RustOfficeOxideRunner
      -> subprocess.run(ai-daily-office-parser)
      -> stdout JSON
      -> validate FileContext payload
      -> success: return rust FileContext
      -> fail: run Python fallback
```

推荐原因：

- 保留 per-file hard timeout。
- 保留现有 `ThreadPoolExecutor(max_workers=4)` 并行模型。
- Rust CLI 不需要每个文件重新 import Python Office 依赖。
- 与现有 Rust discovery 的 stdin/stdout JSON 边界一致。
- Rust 失败时仍可用 Python fallback，避免一次性替换导致日报不可用。

### 方案 B：`office_oxide` Python binding 主路径

不推荐作为第一阶段主线。

优点：

- Python 接入可能更短。
- 不需要新增 Rust CLI 调用层。

缺点：

- native Python binding 会进入 conda 依赖链。
- hard timeout 隔离不如 CLI 清楚。
- 如果 binding 卡死或崩溃，主进程风险更大。
- benchmark 中难区分 Rust 解析和 Python 运行时开销。

### 方案 C：Rust batch parser / long-running worker

不推荐第一阶段直接做。

优点：

- 大量 Office 文件时启动成本最低。
- 可以在 Rust 内部并行。

缺点：

- per-file timeout 复杂。
- 单个坏文件卡住时可能需要 kill 整个 batch worker。
- 与当前 scanner 的 per-file cache/error/metrics 契约差距更大。

## Backend 命名与缓存

新增 backend 名称：

- `rust_office_oxide_v1`
- `python_office_v1`
- `python_sharepoint_text_v1`
- `python_tika_v1`
- `python_libreoffice_v1`
- `pdf_text_v1`
- `not_parsed`

`parser_backend` 不能继续统一写 `office_v1`，否则会误用旧 cache。

parser profile 至少新增或冻结：

```yaml
office_parser_backend: "rust_office_oxide_v1"
office_parser_fallback_enabled: true
office_parser_fallback_order:
  - "python_office_v1"
  - "python_sharepoint_text_v1"
office_legacy_extensions_enabled: false
rust_office_parser_bin: "rust/office_parser/target/release/ai-daily-office-parser"
rust_office_oxide_version: "from Cargo.lock"
```

只要主 backend、fallback 顺序、预算或版本变化，就应触发重新解析，避免跨 backend 复用旧内容。

## 扩展名策略

当前配置默认允许：

- `.docx`
- `.xlsx`
- `.pptx`
- `.xls`

本设计在代码层支持：

- `.docx`
- `.xlsx`
- `.pptx`
- `.doc`
- `.xls`
- `.ppt`

但第一阶段不默认把 `.doc/.ppt` 加进 `allowed_extensions`，除非本机业务扫描确认需要。原因是 legacy Office 文件可能数量多、质量参差不齐，突然扩大扫描范围会改变 benchmark 样本和日报材料来源。

建议新增配置：

```yaml
scanner:
  office_legacy_extensions_enabled: false
```

当该配置为 `true` 时，文档和示例中建议用户显式把 `.doc/.ppt` 加入 `allowed_extensions`，而不是代码自动扩大扫描范围。

## Python Fallback 策略

### 默认 fallback 表

| 扩展名 | 第一 fallback | 第二 fallback | 说明 |
|---|---|---|---|
| `.docx` | `python_office_v1` / `python-docx` | `sharepoint-to-text` | 现有路径稳定，sharepoint-to-text 作为统一补充 |
| `.xlsx` | `python_office_v1` / `openpyxl` | `sharepoint-to-text` | 现有路径稳定，`.xls` 可另评估 python-calamine |
| `.pptx` | `python_office_v1` / `python-pptx` | `sharepoint-to-text` | 现有路径稳定 |
| `.xls` | `sharepoint-to-text` | `python-calamine` 或 `xlrd` | 当前 requirements 没有稳定 `.xls` fallback |
| `.doc` | `sharepoint-to-text` | Apache Tika 或 LibreOffice headless | 不建议用 `python-docx`，它不读 legacy `.doc` |
| `.ppt` | `sharepoint-to-text` | Apache Tika 或 LibreOffice headless | 不建议用 `python-pptx`，它不读 legacy `.ppt` |
| `.pdf` | `pdf_text_v1` / `pdfplumber` | 无 | 不走 office_oxide |

### `sharepoint-to-text`

推荐作为 legacy Office 的第一 Python fallback 候选。

理由：

- PyPI 页面声明 pure Python，无 Java、无 LibreOffice、无系统级依赖。
- 支持现代与 legacy Microsoft Office，包括 `.doc/.xls/.ppt`。
- 提供统一接口，能按文档、sheet、slide 等结构迭代。

风险：

- 当前项目没有使用过，需要 spike 验证中文、表格、空内容、损坏文件、密码文件、性能和许可证。
- 支持声明来自包文档，必须用本机 fixture 和真实样本验证，不能只凭 README 上线。
- 依赖面可能大于现有最小依赖，第一阶段应作为 optional dependency 或明确纳入 requirements 后跑 `pip check`。

### Apache Tika

建议作为可选 fallback，不作为默认。

理由：

- 官方支持格式页显示 Microsoft Office legacy / OOXML 由 Tika parser 系列支持。
- 对 `.doc/.ppt` 这类 legacy 二进制格式通常比轻量 Python 库更成熟。

风险：

- 需要 Java runtime / Tika server 或 jar。
- 启动和资源占用重。
- 引入运维边界，不适合本项目默认轻量本地扫描。

### LibreOffice headless

建议作为可选 fallback，不作为默认。

理由：

- Linux 本地可通过 `soffice --headless --convert-to ...` 转换 legacy Office。
- 对 `.doc/.ppt` 的兼容面通常较好。

风险：

- 系统依赖重。
- 需要临时目录和输出文件清理。
- headless conversion 的错误模型不够结构化。
- 可能受用户本机 LibreOffice 版本影响。

### 不推荐作为默认 fallback 的候选

- `textract`：文档显示 `.doc` 依赖 antiword，能力覆盖和维护状态不适合做主线。
- `antiword` / `doc2txt`：只适合 `.doc`，不解决 `.ppt`，且外部命令依赖明显。
- `olefile` / `oletools`：适合 OLE 容器分析或安全检测，不适合作为通用文本抽取主 fallback。

## Rust CLI 契约

Rust parser 从 stdin 读取 JSON：

```json
{
  "file_path": "/home/george/work/report.xlsx",
  "file_type": ".xlsx",
  "limits": {
    "document_excerpt_max_chars": 6000,
    "excel_max_sheets": 5,
    "excel_max_rows": 50,
    "excel_max_columns": 20,
    "pptx_max_slides": 50
  },
  "parser_backend": "rust_office_oxide_v1"
}
```

stdout 返回 FileContext-compatible JSON：

```json
{
  "file_path": "/home/george/work/report.xlsx",
  "file_type": ".xlsx",
  "content": "# XLSX preview\n\n...",
  "error": null,
  "parser_backend": "rust_office_oxide_v1",
  "truncated": true
}
```

Rust 非零退出、stdout 非 JSON、字段缺失、类型不对，都视为 Rust backend 失败，并进入 Python fallback。

## 输出格式与预算

第一阶段允许 `rust_office_oxide_v1` 的 Markdown 与旧 `office_v1` 不逐字符兼容。

必须保持：

- 输出是 UTF-8 文本。
- `document_excerpt_max_chars` 必须最终生效。
- `truncated` 必须准确反映内容是否被预算截断。
- 空文档应返回可读的空内容说明，或可审计 error。
- Markdown 表格中的 pipe 必须清理或转义，避免破坏表格结构。

如果 `office_oxide` 第一阶段无法按 sheet/row/column/slide 提前停止，则可以先采用：

1. Rust 解析完整 Markdown / text。
2. Rust 或 Python wrapper 按 `document_excerpt_max_chars` 截断。
3. `truncated = true`。

这属于行为变化，必须在 benchmark 和 fixture 中记录。后续再基于 `DocumentIR` 做细粒度预算。

## 错误处理

Rust 失败但 Python fallback 成功：

- 返回 Python fallback 的 `FileContext`。
- `parser_backend` 写实际成功 backend，例如 `python_office_v1` 或 `python_sharepoint_text_v1`。
- reparse detail 记录：
  - `attempted_backend = rust_office_oxide_v1`
  - `fallback_backend = python_office_v1`
  - `fallback_reason = rust_failed:<summary>`

Rust 失败且 Python fallback 不可用：

- 返回 error `FileContext`。
- `parser_backend = rust_office_oxide_v1`
- `error` 示例：

```text
OFFICE_PARSE_FAILED: rust=RUST_OFFICE_PARSE_FAILED: ...; python=PYTHON_FALLBACK_UNAVAILABLE: .ppt
```

Rust timeout：

- 不再进入同一 Rust 进程重试。
- 是否进入 Python fallback 由配置控制：

```yaml
scanner:
  office_fallback_after_timeout: false
```

默认建议 `false`。原因是 timeout 通常意味着文件复杂或坏文件，继续用 Python fallback 可能拉长扫描时间。用户明确要尽量保材料时可打开。

## Benchmark 与可观测性

benchmark JSON / Markdown 需要新增或扩展字段：

- `attempted_backend`
- `parser_backend`
- `fallback_backend`
- `fallback_reason`
- `worker_lane`
- `parse_duration_ms`
- `rust_duration_ms`
- `fallback_duration_ms`

Markdown summary 至少能回答：

- Rust 成功解析了多少 Office 文件。
- Rust 失败后 fallback 了多少。
- fallback 后成功多少、失败多少。
- `.doc/.ppt` 是否真的出现。
- 各扩展名在 Rust 和 Python fallback 下分别耗时多少。

## 测试策略

### Rust 单元测试

- `office_oxide` CLI 能读取最小 JSON request。
- 不支持扩展名返回结构化错误。
- 输出 JSON 字段完整。
- `document_excerpt_max_chars` 能截断输出。
- 非 UTF-8 路径或特殊字符路径能稳定处理。

### Python 单元测试

- `Config.scanner_config` 暴露新增配置并保持可 pickle。
- Rust runner 成功时返回 `FileContext`。
- Rust runner 非零退出时进入 Python fallback。
- Rust stdout 非 JSON 时进入 Python fallback。
- Rust timeout 时按配置决定是否 fallback。
- `.doc/.ppt` fallback 不可用时返回可审计错误。
- `parser_backend` / fallback reason 写入 reparse detail。
- cache key 包含 Rust backend 和 fallback 配置。

### Fixture / contract 测试

最小 fixture：

- `.docx`：段落 + 表格 + 中文。
- `.xlsx`：多 sheet + 空行 + 超列 + 中文。
- `.pptx`：标题 + 文本框 + 表格 + notes。
- `.xls`：至少一个 sheet，验证 legacy fallback。
- `.doc`：至少一段中文和英文，验证 `sharepoint-to-text` 或可选 fallback。
- `.ppt`：至少一页标题和正文，验证 `sharepoint-to-text` 或可选 fallback。

输出不要求和旧 `office_v1` 逐字符一致，但必须包含关键文本、遵守截断、返回正确 backend。

### 性能测试

至少比较三组：

1. 当前 Python per-file subprocess `office_v1`
2. Rust `office_oxide` primary + Python fallback
3. Rust primary 在 fallback 关闭时的纯 Rust 成功/失败表现

benchmark 场景：

- 少量 Office 文件：复用当前 4 个真实 Office 文件样本。
- 大量 Office 文件：构造 50 或 100 个 `.docx/.xlsx/.pptx` 混合样本。
- Legacy 文件：如果本机有 `.doc/.ppt/.xls`，单独跑 legacy benchmark，不混进默认日报样本。

## 非目标

- 不把 PDF 改成 Rust。
- 不引入 OCR。
- 不调用 LLM 做图片、图表或扫描件解释。
- 不让 Rust 写 SQLite 或 parse cache。
- 不默认打开 `.doc/.ppt` 扫描范围。
- 不保证 `rust_office_oxide_v1` 与旧 `office_v1` 输出逐字符一致。
- 不在第一阶段实现 long-running Rust worker pool。

## 风险点 / 边界条件

- `office_oxide` 当前版本仍较新，必须通过真实样本和 fixture 验证，不能只凭 README 声明上线。
- Rust 输出与旧 Python preview 可能差异较大，cache key 必须隔离。
- `sharepoint-to-text` 虽是很有吸引力的 Python fallback，但也需要本机验证，尤其是中文、表格和 legacy `.ppt`。
- Apache Tika / LibreOffice fallback 会引入 Java 或系统 Office runtime，不适合默认启用。
- legacy `.doc/.ppt` 可能包含宏、嵌入对象、损坏 OLE 结构或密码保护，必须以可审计错误结束。
- 如果 fallback_after_timeout 打开，坏文件可能放大扫描耗时。

## 验收方式

1. `cd rust/office_parser && cargo test` 通过。
2. `cd rust/office_parser && cargo build --release` 通过。
3. `conda run -n test python -m pytest tests -q` 通过。
4. `conda run -n test python -m compileall main.py src tests` 通过。
5. Rust parser contract test 覆盖 `.docx/.xlsx/.pptx`，并在有 fixture 时覆盖 `.doc/.xls/.ppt`。
6. 无缓存 benchmark 中 `.docx/.xlsx/.pptx` 能显示 `rust_office_oxide_v1` 或明确 fallback reason。
7. fallback 成功时 benchmark 能显示 fallback backend。
8. cache hit 时不会把旧 `office_v1` 内容误认为 `rust_office_oxide_v1`。
9. 默认配置下 `.doc/.ppt` 不会突然进入扫描范围；启用 legacy 配置后才进入。

## 伪代码草案

```python
# [伪代码草案]
# 目标：Office 文件优先走 Rust office_oxide；失败时按扩展名进入 Python fallback；
#       所有路径都返回 FileContext，保证 scanner 主流程不被单个坏文件打断。

OFFICE_RUST_TYPES = {".docx", ".xlsx", ".pptx", ".doc", ".xls", ".ppt"}


def extract_document_with_timeout(file_path: Path, limits: dict) -> FileContext:
    file_type = file_path.suffix.lower()

    # PDF 不属于 office_oxide 范围，继续走现有 text layer parser。
    if file_type == ".pdf":
        return run_python_pdf_parser_with_timeout(file_path, limits)

    if file_type not in OFFICE_RUST_TYPES:
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error=f"DOCUMENT_UNSUPPORTED_EXTENSION: {file_type}",
            parser_backend="not_parsed",
            truncated=False,
        )

    # 文件大小门禁必须在进入 Rust/Python parser 前执行，避免坏样本拖垮进程。
    too_large = build_file_too_large_context(file_path, file_type)
    if too_large is not None:
        return too_large

    rust_result = run_rust_office_oxide(file_path, file_type, limits)
    if rust_result.ok:
        return rust_result.context

    # 为什么 fallback 要可配置：timeout 文件继续 fallback 可能让扫描耗时失控。
    if rust_result.timed_out and not scanner_cfg["office_fallback_after_timeout"]:
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error=f"RUST_OFFICE_TIMEOUT: {rust_result.error}",
            parser_backend="rust_office_oxide_v1",
            truncated=False,
        )

    fallback_result = run_python_office_fallback(
        file_path=file_path,
        file_type=file_type,
        limits=limits,
        rust_error=rust_result.error,
    )
    if fallback_result.ok:
        return fallback_result.context

    return FileContext(
        file_path=str(file_path),
        file_type=file_type,
        content="",
        error=(
            "OFFICE_PARSE_FAILED: "
            f"rust={rust_result.error}; python={fallback_result.error}"
        ),
        parser_backend="rust_office_oxide_v1",
        truncated=False,
    )


def run_python_office_fallback(
    file_path: Path,
    file_type: str,
    limits: dict,
    rust_error: str,
) -> FallbackResult:
    # 现代 OOXML 先复用项目现有稳定路径，降低引入新依赖的影响面。
    if file_type in {".docx", ".xlsx", ".pptx"}:
        context = parse_document_file(
            file_path=file_path,
            file_type=file_type,
            limits=limits,
            options=DocumentParserOptions(
                office_parser_backend="python_office_v1",
            ),
        )
        if context.error is None:
            return FallbackResult.ok(context, fallback_reason=rust_error)

    # legacy Office 优先用 sharepoint-to-text；它是候选依赖，必须通过 fixture 验证后启用。
    if file_type in {".doc", ".xls", ".ppt"} and has_sharepoint_to_text():
        context = parse_with_sharepoint_to_text(file_path, file_type, limits)
        if context.error is None:
            return FallbackResult.ok(context, fallback_reason=rust_error)

    # Tika / LibreOffice 是可选外部兜底，默认不启用。
    if scanner_cfg["office_external_fallback"] == "tika":
        return parse_with_tika(file_path, file_type, limits)
    if scanner_cfg["office_external_fallback"] == "libreoffice":
        return parse_with_libreoffice_headless(file_path, file_type, limits)

    return FallbackResult.error(
        f"PYTHON_FALLBACK_UNAVAILABLE: {file_type}; rust={rust_error}"
    )
```

## 用户确认点

已确认：

- 采用激进 Rust 主路径。
- 优先使用 `office_oxide`。
- Rust 失败时加入 Python fallback。
- `.doc/.ppt` 也要寻找 Python 相关依赖并纳入 fallback 设计。

实现默认值：

- `sharepoint-to-text` 纳入实现范围，但必须先通过安装、`pip check`、最小 fixture smoke test；只有验证通过才写入 `requirements.txt` 和 fallback 链。
- `.doc/.ppt` 不立即加入默认 `allowed_extensions`；代码支持与文档说明先落地，真实扫描范围由用户显式配置开启。
- Rust timeout 后默认不进入 Python fallback，即 `office_fallback_after_timeout = false`；用户明确要求尽量保材料时再开启。
