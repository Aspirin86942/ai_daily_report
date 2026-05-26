# Bounded XLSX Preview Design

Status: IMPLEMENTED
Mode: Brainstorming

## 目标

为 scanner 的 `.xlsx` 解析增加 Rust 有界预览路径，避免把大 workbook 完整解析成 Markdown 后再截断。日报/周报的 LLM 上下文只能消费有限内容，因此 `.xlsx` parser 也应按同一预算只读取前几个 sheet、前 N 行、前 N 列，并在达到字符预算时早停。

本设计只改变 `.xlsx` 主路径：

- `.xlsx`: 新增 `rust_xlsx_bounded_v1`
- `.docx`、`.pptx`、`.doc`、`.xls`、`.ppt`: 继续走 `rust_office_oxide_v1`
- `.pdf` 与 text-like 文件不在本设计范围内

## 输入

- `OfficeParseRequest.file_path`: 待解析 `.xlsx` 路径。
- `OfficeParseRequest.file_type`: 必须标准化为 `.xlsx`。
- `OfficeParseRequest.limits`:
  - `excel_max_sheets`: 最多读取的 sheet 数，summary 默认 2。
  - `excel_max_rows`: 每个 sheet 最多保留的非空行数，summary 默认 10。
  - `excel_max_columns`: 每行最多保留的列数，summary 默认 12。
  - `document_excerpt_max_chars`: 输出文本最大字符数，summary 默认跟随 text budget。
- Python `FileScanner` 仍负责 timeout、parse cache、metrics、aggregation 和 benchmark 输出。

## 输出

成功时继续返回 `FileContextOut`：

- `file_path`: 原始文件路径。
- `file_type`: `.xlsx`。
- `content`: Markdown-like preview。
- `error`: `None`。
- `parser_backend`: `rust_xlsx_bounded_v1`。
- `truncated`: 只要触发 sheet、row、column 或 char 预算即为 `true`。

失败时返回可审计错误，不抛出到 scanner 主流程：

- ZIP 结构错误：`RUST_XLSX_BOUNDED_PARSE_FAILED: ...`
- workbook / rels / worksheet XML 解析错误：`RUST_XLSX_BOUNDED_PARSE_FAILED: ...`
- 不支持或无内容：返回空 preview 或结构化错误，由 scanner 保持现有 error-count 语义。

## 设计

### Rust fast path

`rust/office_parser/src/lib.rs::parse_office_file()` 增加 `.xlsx` 分支：

```text
if file_type == ".xlsx":
    return parse_bounded_xlsx(request)
else:
    return parse_office_oxide(request)
```

`parse_bounded_xlsx()` 不再调用 `office_oxide::Document::open().to_markdown()`，而是直接读取 XLSX zip 包的必要 XML：

- `xl/workbook.xml`: 读取 sheet 名称与 `r:id`。
- `xl/_rels/workbook.xml.rels`: 把 `r:id` 映射到 worksheet zip entry。
- `xl/worksheets/sheetN.xml`: 只流式读取选中 sheet 的前 N 行 N 列。
- `xl/sharedStrings.xml`: 只解析预览区域实际引用到的 shared string index。

### 早停规则

- Sheet 早停：只处理前 `excel_max_sheets` 个可解析 sheet。
- Row 早停：每个 sheet 只保留前 `excel_max_rows` 个非空行。
- Column 早停：只保留列号小于等于 `excel_max_columns` 的 cell。
- Char 早停：最终追加 Markdown 时使用字符预算，达到 `document_excerpt_max_chars` 后停止。
- 只要因预算跳过了可见内容，`truncated=true`。

### Python 接线

Python 侧需要接受新的 Rust backend：

- `src/services/office_parser.py` 新增常量 `RUST_XLSX_BOUNDED_BACKEND = "rust_xlsx_bounded_v1"`。
- `_validate_rust_payload_context()` 对 `.xlsx` 允许 `rust_xlsx_bounded_v1` 和 `rust_office_oxide_v1`。
- `parse_office_with_fallback()` 遇到 `RUST_XLSX_BOUNDED_PARSE_FAILED: ZIP error` 这类确定性坏 xlsx 时不再跑 Python fallback，避免 warm scan 反复花 1s+ 解析坏文件。

### Cache 与 benchmark

`parser_backend` 会进入 scan metrics 和 parse cache，warm run 应能清晰显示：

- `.xlsx` backend 从 `rust_office_oxide_v1` 变为 `rust_xlsx_bounded_v1`。
- 大 `.xlsx` cold parse 长尾明显下降。
- 确定性坏 `.xlsx` warm run 不再触发 Python fallback 长尾。

## 伪代码草案

```python
# [伪代码草案]
# 目标：对 XLSX 做确定性 bounded preview，避免完整 workbook -> markdown 后才截断
# 输入：
# - request.file_path: XLSX 文件路径
# - request.limits: sheet/row/column/char 预算
# 输出：
# - FileContextOut: 成功预览或可审计错误

def parse_office_file(request):
    file_type = normalize_file_type(request.file_type)
    if file_type == ".xlsx":
        return parse_bounded_xlsx(request)
    return parse_with_office_oxide(request)


def parse_bounded_xlsx(request):
    budgets = normalize_limits(request.limits)

    try:
        archive = open_xlsx_zip(request.file_path)

        # 先读 workbook 和 rels，只为了知道前几个 sheet 的名字与 XML 路径。
        sheets = parse_workbook_sheets(archive, limit=budgets.max_sheets)
        rels = parse_workbook_relationships(archive)

        preview_sheets = []
        used_shared_string_indexes = set()
        truncated = False

        for sheet in sheets:
            path = resolve_sheet_path(sheet, rels)
            # 这里必须流式读 worksheet XML。读到足够行列后就停止，避免大 sheet 全量解压解析。
            sheet_preview = parse_sheet_rows_bounded(
                archive=archive,
                path=path,
                max_rows=budgets.max_rows,
                max_columns=budgets.max_columns,
            )
            used_shared_string_indexes.update(sheet_preview.shared_indexes)
            truncated = truncated or sheet_preview.truncated
            preview_sheets.append(sheet_preview)

        shared_strings = parse_needed_shared_strings(
            archive=archive,
            indexes=used_shared_string_indexes,
        )

        content, char_truncated = render_markdown_with_budget(
            preview_sheets,
            shared_strings,
            max_chars=budgets.max_chars,
        )

        return success_context(
            content=content or "No worksheet text extracted",
            backend="rust_xlsx_bounded_v1",
            truncated=truncated or char_truncated,
        )

    except Exception as exc:
        return error_context(
            error_code="RUST_XLSX_BOUNDED_PARSE_FAILED",
            message=str(exc),
            backend="rust_xlsx_bounded_v1",
        )
```

## 风险点 / 边界条件

- Shared strings 可能很大。实现应只解析预览区域实际引用的 index，并在超过最大 index 后停止。
- XLSX 可能包含 inline string、shared string、number、bool、formula cached value。第一阶段只需要稳定文本预览，不做 Excel 格式化还原。
- 列号应从 cell ref 解析，例如 `A1 -> 1`、`AA1 -> 27`，无 cell ref 时按当前行已见 cell 顺序估算。
- 坏 zip / 假 xlsx 应快速失败，并返回结构化错误。
- 新 backend 会改变 cache profile，旧缓存不应被复用。

## 验收条件

- Rust 单元测试证明 `.xlsx` 返回 `rust_xlsx_bounded_v1`。
- Rust 单元测试证明只输出前 N 行 N 列，并设置 `truncated=true`。
- Python 单元测试证明 runner 接受 `rust_xlsx_bounded_v1`。
- Python 单元测试证明确定性坏 `.xlsx` 不触发 Python fallback。
- `cargo test`、目标 Python pytest 通过。
- 至少跑一次 `scripts/run_scanner_benchmark_ab.ps1 -SkipBuild` 或等价 benchmark，记录 cold/warm 对比。
