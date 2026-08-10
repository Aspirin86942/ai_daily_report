# Office / PDF Parser Backend Design

Status: REVIEW_READY
Mode: Brainstorming

## 目标

在不破坏现有 `light_text_v1` 行为的前提下，为 scanner 加入可观测、可缓存、可 benchmark 的 Office / PDF 解析 backend。

第一阶段目标不是做高保真文档转换器，而是为日报、周报和后续 LLM 阅读提供 bounded Markdown-like / text preview。所有新增解析路径必须继续遵守 scanner 的 `max_file_size_mb`、timeout、parse cache、summary mode 和 benchmark 约束。

本设计优先覆盖：

- Word：`.docx`
- Excel：`.xlsx`
- PowerPoint：`.pptx`
- PDF：有 text layer 的 PDF

第一阶段不覆盖：

- OCR
- 扫描版 PDF 图像文字识别
- `.doc` / `.ppt`
- 复杂图片、音视频、嵌入对象解析
- Azure Document Intelligence / Azure Content Understanding
- 大模型视觉能力
- Rust backend
- discovery pruning

## 当前现状

当前 scanner 已经有 Office / PDF 解析函数，不能按“尚无支持”来设计。

`src/services/file_scanner.py` 现状如下：

- `_parse_excel()` 已存在。
  - `.xlsx` / `.xls` 都会进入这个分支。
  - 使用 `pandas.ExcelFile()` 和 `pd.read_excel()`。
  - 当前会读取所有 sheet 名称，每个 sheet 用 `nrows=max_rows` 限制行数。
  - 没有 sheet 数限制、列数限制，也没有按输出字符预算提前停止。
  - `.xls` 目前是名义支持；`requirements.txt` 没有 `xlrd`，所以真实 `.xls` 能否成功取决于环境是否额外安装依赖。
- `_parse_pdf()` 已存在。
  - 使用 `pdfplumber`。
  - 使用 `pdf.pages[:max_pages]` 限制页数。
  - 只提取 `page.extract_text()`，没有 OCR。
  - 如果前 N 页没有 text layer，当前会返回空字符串，不会明确记录 `PDF_NO_TEXT_LAYER` 一类可审计原因。
- `_parse_pptx()` 已存在。
  - 使用 `python-pptx` 的 `Presentation`。
  - 提取 shape 上的 `text`。
  - 没有 slide 数限制，没有 notes 提取，也没有明确处理表格、图表、图片 alt text。
- `_parse_docx()` 已存在。
  - 使用 `python-docx`。
  - 只提取段落文本。
  - 不提取表格，没有段落数、表格数、行列数、字符数的内部预算。

当前执行路径：

- 默认 `worker_lane_mode = "direct"` 只让 `.txt/.md/.csv/.json/.log` 走 `light_text_v1` direct lane。
- Office / PDF 在 direct 模式下仍会走 `_extract_content_with_timeout()`，也就是 Windows spawn subprocess timeout lane。
- `file_timeout_by_extension` 当前只为 `.pdf/.xlsx/.xls` 配了覆盖值；`.docx/.pptx` 使用默认 `file_timeout_seconds`。
- `max_file_size_mb` 在 `_extract_content()` 中会生效，但 Office / PDF 当前是在子进程内做 size gate。下一阶段应把 size gate 前移到父进程规划/分流前，避免超大 Office/PDF 文件被误计为 subprocess 解析。
- 现有 Office / PDF 成功返回的 `FileContext` 没有设置 `parser_backend`，也没有设置真实 `truncated`。
- `FileScanner._record_reparse_detail()` 对空 backend 会回退成 `"subprocess"`，导致 benchmark 只能看到 subprocess，不能区分实际是 Excel、PDF、PPTX 还是 DOCX parser。
- parse cache 已经支持 `parser_backend` 和 `truncated` 字段，cache hit 后也会恢复这两个字段。
- `ScanPlanner.build_parser_profile()` 已经把 `text_parser_backend = "light_text_v1"`、读取预算和 `parser_profile_version` 纳入 cache key；Office/PDF 的 backend 版本和新增预算还没有纳入 profile。

结论：本轮不是新增“能不能读 Office/PDF”，而是把已有解析升级成 bounded、可审计、可缓存、可 benchmark 的正式 parser backend。

## MarkItDown 参考点

本设计参考了 Microsoft MarkItDown，但不建议第一阶段把它变成硬依赖。

事实基础：

- MarkItDown README 定位为轻量 Python 工具，把多种文件转换为 Markdown，用于 LLM 和文本分析流水线；它强调尽量保留文档结构，例如 headings、lists、tables、links 等。
- MarkItDown 支持 PDF、PowerPoint、Word、Excel，也支持图片 OCR、音频转写、HTML、ZIP、EPub 等更多格式。
- MarkItDown 的可选依赖按 feature group 安装，例如 `[pdf]`、`[docx]`、`[pptx]`、`[xlsx]`、`[xls]`。
- MarkItDown 的主入口会注册多个 converter，并基于扩展名、MIME、Magika 等 stream info 选择 converter。
- MarkItDown 的 DOCX converter 使用 `mammoth` 把 docx 转 HTML，再复用 HTML converter 转 Markdown，表格和部分样式保留更好。
- MarkItDown 的 XLSX converter 使用 `pandas.read_excel(sheet_name=None)` 读取所有 sheet，再转 HTML / Markdown。
- MarkItDown 的 PPTX converter 使用 `python-pptx`，可提取标题、文本、表格、图表、图片 alt text 和 notes；若传入 LLM client/model，还能为图片生成描述。
- MarkItDown 的 PDF converter 使用 `pdfplumber` / `pdfminer.six`，包含表格/表单启发式处理，并在遍历 page 后 close page 以降低缓存占用。
- MarkItDown README 也明确提示它会以当前进程权限执行 I/O，输入不可信时需要做安全收窄。

对本项目有价值的参考：

- converter registry / adapter 思路：把“识别格式”和“执行转换”解耦。
- 输出 Markdown-like preview 的方向是对的，适合 LLM 消费。
- PowerPoint 可以在不接 OCR 的情况下提取 notes、表格、图表摘要和图片 alt text。
- PDF 应优先使用 text layer，不做 OCR；对无 text layer 的文件要明确返回可审计状态。
- 每种格式的 parser backend 版本应进入 parser profile，避免缓存错误复用。

不应直接照搬的点：

- MarkItDown 的 XLSX converter 会读取所有 sheets，不符合本项目“sheet 数、行数、列数必须有上限”的 scanner 约束。
- MarkItDown 的 PDF converter 面向完整转换，第一阶段不需要复杂表格/表单启发式，也不能让它绕过本项目 `pdf_max_pages`。
- `markitdown[all]` 依赖范围过大，会引入音频、Azure、OCR 等本轮不需要的能力。
- 即便只安装 `[pdf,docx,pptx,xlsx]`，也会引入本项目当前没有的 `mammoth`、`lxml`、`magika`、`markdownify`、`pdfminer.six` 版本约束等，需要额外验证 Windows 稳定性和依赖冲突。
- MarkItDown 的 plugin / OCR 能力必须默认关闭，否则会偏离“不做 OCR、不接视觉/云服务”的边界。

参考链接：

- MarkItDown repository / README: https://github.com/microsoft/markitdown
- MarkItDown converters: https://github.com/microsoft/markitdown/tree/main/packages/markitdown/src/markitdown/converters
- MarkItDown package dependencies: https://github.com/microsoft/markitdown/blob/main/packages/markitdown/pyproject.toml
- DOCX converter: https://github.com/microsoft/markitdown/blob/main/packages/markitdown/src/markitdown/converters/_docx_converter.py
- XLSX converter: https://github.com/microsoft/markitdown/blob/main/packages/markitdown/src/markitdown/converters/_xlsx_converter.py
- PPTX converter: https://github.com/microsoft/markitdown/blob/main/packages/markitdown/src/markitdown/converters/_pptx_converter.py
- PDF converter: https://github.com/microsoft/markitdown/blob/main/packages/markitdown/src/markitdown/converters/_pdf_converter.py

## 推荐方案

### 方案 A：沿用当前 Python 依赖，封装本项目自己的 Office/PDF backend

做法：

- 新增本项目自己的 document parser 模块，例如 `src/services/document_parser.py`。
- 复用现有依赖：
  - `.docx`: `python-docx`
  - `.xlsx`: `openpyxl` read-only mode
  - `.pptx`: `python-pptx`
  - `.pdf`: `pdfplumber`
- 输出 `FileContext`，明确设置 `parser_backend`、`truncated`、`error`。
- `FileScanner` 只负责路由、size gate、timeout lane 和 cache 写回；格式细节不继续堆在 `file_scanner.py`。

依赖体积和安装风险：

- 低。依赖基本已经在 `requirements.txt` 中。
- 不新增 Rust，不新增 OCR，不新增 Azure。
- `.xls` 若继续真实支持，需要新增 `xlrd`，但第一阶段不建议把 `.xls` 做成重点范围。

Windows 本地稳定性：

- 较高。当前项目已经在 Windows + conda 环境运行这些依赖。
- Office/PDF 解析仍建议通过 subprocess supervisor 执行，以保留 hard timeout 和失败隔离。

解析质量：

- 中等。足够做 scanner preview，但不追求 MarkItDown 那种更完整的 Markdown 转换。
- DOCX 表格、XLSX 表格、PPTX slide text / notes、PDF text layer 可以覆盖主要日报/周报素材。

性能风险：

- 可控。每类文件都有 size gate、sheet/page/slide/row/column/char 上限。
- 由于 Office/PDF 解析依赖库本身可能加载整个 zip/pdf 结构，仍需要 subprocess timeout 兜底。

cache key / parser profile：

- 在 profile 中加入：
  - `office_parser_backend = "office_v1"`
  - `pdf_parser_backend = "pdf_text_v1"`
  - Office/PDF 预算参数
  - `parser_profile_version`
- 任一预算或 backend 版本变化都会触发重新解析。

benchmark 证明方式：

- `parser_backend_summary.by_extension` 出现 `.docx/.xlsx/.pptx -> office_v1` 和 `.pdf -> pdf_text_v1`。
- `truncated_count` 能反映 sheet/page/slide/char 预算截断。
- 对 `max_file_size_mb` 超限文件显示 `not_parsed`，不能显示成 subprocess。

评价：

- 推荐作为第一阶段落地基础。
- 缺点是转换质量不如 MarkItDown，尤其是 DOCX 样式、复杂 PDF 表格和 PPTX 图片描述。

### 方案 B：直接或间接集成 MarkItDown 作为可选依赖

做法：

- 增加可选依赖，例如 `markitdown[pdf,docx,pptx,xlsx]`。
- 在 scanner 中新增 `markitdown_v1` adapter。
- 默认关闭 OCR、plugins、Azure、LLM caption。
- 通过 adapter 把 MarkItDown output 包成 `FileContext`。

依赖体积和安装风险：

- 中到高。
- 即便不装 `[all]`，也会引入本项目目前没有的基础依赖和格式依赖，例如 `mammoth`、`lxml`、`magika`、`markdownify`、`pdfminer.six` 等。
- MarkItDown 本身还支持远程 URL、ZIP、图片、音频等能力；如果直接暴露，需要额外安全收窄。

Windows 本地稳定性：

- 需要专项验证。
- Python 3.10+ 是兼容前提，但 README 示例推荐独立 Python 3.12 环境；本项目默认按 3.10+ 和 `conda run -n test` 处理。

解析质量：

- 高于方案 A，尤其是 DOCX 样式、PPTX 表格/notes/alt text、PDF fallback。
- 但 scanner 只需要 preview，不一定值得第一阶段承担依赖和边界复杂度。

性能风险：

- 中到高。
- MarkItDown 的 XLSX 路径读取所有 sheets，PDF 路径也偏完整转换；如果 adapter 外层只截断输出，仍可能先消耗大量内存或时间。
- 要满足本项目 bounded extraction，需要在 adapter 外层额外做 size gate、timeout、页数/sheet 限制，甚至可能需要 fork 或自定义 converter。

cache key / parser profile：

- 需要把 `markitdown` package version、adapter version、启用 features、禁用 OCR/LLM/Azure 的状态写入 parser profile。
- 如果 MarkItDown 升级导致输出变化，也应触发重新解析。

benchmark 证明方式：

- 新 backend 可命名为 `markitdown_v1` 或 `markitdown_office_pdf_v1`。
- benchmark 需要展示 MarkItDown backend 的成功、失败、耗时、截断和 timeout。

评价：

- 不推荐作为第一阶段硬依赖。
- 可以作为后续 optional adapter，但必须先有本项目自己的 parser boundary 和 budget contract。

### 方案 C：混合方案：先实现本项目 bounded backend，预留 MarkItDown adapter seam

做法：

- 第一阶段落地方案 A 的核心能力。
- 模块边界按 adapter 形态设计：
  - `DocumentParserOptions`
  - `DocumentParseResult` 或直接 `FileContext`
  - `parse_document_file()`
  - per-extension parser function
- `FileScanner` 不关心 parser 内部用 `python-docx`、`openpyxl`、`python-pptx`、`pdfplumber` 还是 MarkItDown。
- 后续如果需要更高质量转换，再新增可选 `markitdown_adapter.py`，但默认关闭。

依赖体积和安装风险：

- 第一阶段低。
- 后续 adapter 可通过 optional dependency 单独验证，不影响默认用户。

Windows 本地稳定性：

- 第一阶段沿用当前依赖，稳定性最好。
- 后续 MarkItDown adapter 可以单独 benchmark 和回滚。

解析质量：

- 第一阶段中等但足够 scanner preview。
- 后续可在 DOCX/PPTX/PDF 复杂样本上按证据切换部分格式到 MarkItDown。

性能风险：

- 第一阶段可控。
- adapter seam 让后续 MarkItDown 不会直接侵入 `FileScanner` 主流程。

cache key / parser profile：

- 第一阶段使用 `office_v1` / `pdf_text_v1`。
- 后续 adapter 使用 `markitdown_v1`，并把 adapter 版本、MarkItDown 版本和启用 feature 写入 profile。

benchmark 证明方式：

- 第一阶段证明 `office_v1` / `pdf_text_v1` 的 backend 使用情况和 bounded 行为。
- 后续 adapter 可以在同一 benchmark 结构下横向比较。

评价：

- 推荐。
- 第一阶段实现内容基本等同方案 A，但设计边界按方案 C 保留 adapter seam。

## 不做范围

- 不改 `light_text_v1` 的读取策略、输出格式、backend 名称或测试契约。
- 不做 discovery pruning。
- 不把 MarkItDown 作为硬依赖。
- 不安装 `markitdown[all]`。
- 不接 OCR、Azure、LLM Vision。
- 不承诺扫描版 PDF 可读；扫描版 PDF 应返回明确的 no text layer 审计结果。
- 不在第一阶段重写 scan discovery、inventory、aggregation。
- 不为了 notes、图片、图表引入复杂依赖。
- 不把 `.doc` / `.ppt` 纳入 v1。
- 不把 `.xls` 作为 v1 重点支持；保留现有 allowed extension 和 legacy 路径，但真实解析质量取决于是否安装 `xlrd`。如后续要正式支持 `.xls`，应单独设计 `xls_legacy_v1` 或引入 `xlrd` optional dependency。

## Parser backend 设计

推荐 backend 命名：

- Office: `office_v1`
- PDF: `pdf_text_v1`
- 保持现有 text-like backend: `light_text_v1`
- 保持未解析 backend: `not_parsed`
- 保持 legacy subprocess fallback: `subprocess`

不推荐第一阶段使用单一 `office_pdf_v1`，原因是 Office 和 PDF 的依赖、预算、失败模式完全不同。把它们拆成 `office_v1` 和 `pdf_text_v1` 可以让 benchmark 和 cache profile 更清楚。

也不建议一开始拆成 `docx_v1`、`xlsx_v1`、`pptx_v1` 三个 backend。当前 benchmark 已经按 extension 分组，`office_v1 + extension` 足够定位格式；如果后续某一类格式频繁独立演进，再拆细 backend。

建议新增模块：

```text
src/services/document_parser.py
```

建议职责：

- 接收 `file_path`、`file_type`、`limits`、`DocumentParserOptions`。
- 先做 per-format bounded extraction。
- 返回 `FileContext`，而不是裸字符串。
- 对所有失败写入稳定错误码前缀。
- 不负责 discovery、cache、benchmark、thread pool。

`FileScanner` 建议职责：

- 保持 orchestration。
- 在父进程先执行 `max_file_size_mb` gate，所有格式一致。
- 根据 `worker_lane_mode` 和扩展名选择路径。
- 对 Office/PDF 使用 timeout supervisor，避免第三方解析库卡住时拖死主进程。
- 写回 parse cache 和 reparse detail。

需要拆清两个概念：

- `parser_backend`：内容由哪个 parser 逻辑生成，例如 `office_v1`、`pdf_text_v1`、`light_text_v1`。
- `execution_lane` 或 `worker_lane`：解析在哪条执行通道运行，例如 `direct`、`subprocess`、`not_parsed`。

当前代码把空 backend 回退为 `"subprocess"`，这会把“执行通道”和“解析后端”混在一起。升级 Office/PDF 后，建议在 `ReparseDetail` 增加可选 `worker_lane` 字段，benchmark 的 `subprocess_count` 统计执行通道，backend 表统计真实 parser backend。为了兼容旧数据，如果没有 `worker_lane`，可以继续按 `parser_backend == "subprocess"` 推断。

这样可以同时满足两件事：

- benchmark 能看到 `.docx/.xlsx/.pptx -> office_v1`、`.pdf -> pdf_text_v1`。
- timeout/subprocess 计数仍然真实，不会因为 backend 改名而误报 subprocess 为 0。

## 各文件类型解析策略

### `.docx`

依赖：

- 第一阶段使用现有 `python-docx`。
- 暂不引入 MarkItDown 的 `mammoth`。

bounded extraction：

- 先过 `max_file_size_mb`。
- 限制段落数，例如 `docx_max_paragraphs`。
- 限制表格数，例如 `docx_max_tables`。
- 限制每个表格行数，例如 `docx_table_max_rows`。
- 限制每个表格列数，例如 `docx_table_max_cols`。
- 限制总输出字符数，达到预算后停止继续遍历。

输出格式：

```markdown
# DOCX preview

## Paragraphs

第一段文本...

第二段文本...

## Table 1

| A | B |
|---|---|
| 1 | 2 |
```

truncated 规则：

- 段落数超过预算。
- 表格数超过预算。
- 表格行/列超过预算。
- 输出字符超过预算。

错误处理：

- 解析异常返回 `DOCX_PARSE_FAILED: ...`。
- 空文档可返回成功但 content 中注明 `No paragraph/table text extracted`；如果后续希望纳入 error_count，可单独调整。

### `.xlsx`

依赖：

- 第一阶段优先使用 `openpyxl.load_workbook(read_only=True, data_only=True)`。
- 不再用 `pd.read_excel(sheet_name=None)` 读取整本工作簿。
- `.xls` 不纳入 `office_v1` 正式目标。

bounded extraction：

- 先过 `max_file_size_mb`。
- 限制 sheet 数，例如 `excel_max_sheets`。
- 复用现有 `excel_max_rows` / `summary_excel_max_rows`。
- 新增列数限制，例如 `excel_max_columns`。
- 跳过全空行。
- 达到总输出字符预算后停止继续解析。

输出格式：

```markdown
# XLSX preview

## Sheet: Sheet1

| 客户 | 金额 | 日期 |
|---|---:|---|
| A 公司 | 100.00 | 2026-05-24 |

_Truncated: sheet/row/column limit reached._
```

truncated 规则：

- sheet 数超过预算。
- 任一 sheet 行数超过预算。
- 任一行列数超过预算。
- 输出字符超过预算。

金额和小数：

- scanner preview 不做金额计算，只做展示。
- 若后续增加金额运算或对账逻辑，必须使用 `decimal.Decimal`，禁止用 `float` 做精确计算。

错误处理：

- `.xlsx` 解析异常返回 `XLSX_PARSE_FAILED: ...`。
- 加密或损坏工作簿返回 error，不静默吞掉。
- `.xls` 若走 legacy 路径且缺少依赖，返回可审计错误，例如 `XLS_LEGACY_UNSUPPORTED: xlrd is not installed`。

### `.pptx`

依赖：

- 使用现有 `python-pptx`。

bounded extraction：

- 先过 `max_file_size_mb`。
- 新增 slide 数限制，例如 `pptx_max_slides` / `summary_pptx_max_slides`。
- 提取 slide title、shape text。
- 简单提取 table 文本，转 Markdown-like table。
- 如果 `slide.has_notes_slide` 且 notes frame 可得，提取 speaker notes。
- 不为 notes 引入新依赖。
- 不解析图片内容，不生成图片 caption。

输出格式：

```markdown
# PPTX preview

## Slide 1

标题

正文文本...

### Notes

讲者备注...
```

truncated 规则：

- slide 数超过预算。
- 单页 shape/table/notes 达到输出字符预算。
- 总输出字符超过预算。

错误处理：

- 解析异常返回 `PPTX_PARSE_FAILED: ...`。
- 无文本 deck 可返回成功但 content 中注明 `No slide text extracted`。

### `.pdf`

依赖：

- 使用现有 `pdfplumber`。
- 第一阶段不直接使用 MarkItDown 的 `pdfminer.six` fallback 和表格/表单启发式。

bounded extraction：

- 先过 `max_file_size_mb`。
- 复用现有 `pdf_max_pages` / `summary_pdf_max_pages`。
- 只遍历前 N 页。
- 每页调用 `page.extract_text()`。
- 页面解析后尽量释放 page 缓存，降低长 PDF 的内存占用。
- 达到输出字符预算后停止继续解析。

输出格式：

```markdown
# PDF text preview

## Page 1

文本内容...

## Page 2

文本内容...
```

truncated 规则：

- PDF 总页数超过 `pdf_max_pages`。
- 输出字符超过预算。

错误处理：

- 无 text layer 返回 `PDF_NO_TEXT_LAYER: no extractable text in first N pages`，不做 OCR。
- 解析异常返回 `PDF_PARSE_FAILED: ...`。
- 加密 PDF 返回可审计 error。

## 配置项 / parser profile

现有配置继续保留：

- `excel_max_rows`
- `summary_excel_max_rows`
- `pdf_max_pages`
- `summary_pdf_max_pages`
- `text_max_chars`
- `summary_text_max_chars`
- `total_max_chars`
- `max_file_size_mb`
- `file_timeout_seconds`
- `file_timeout_by_extension`
- `worker_lane_mode`
- `parser_profile_version`

建议新增配置，默认值可保守：

```toml
[scanner]
office_parser_backend = "office_v1"
pdf_parser_backend = "pdf_text_v1"

excel_max_sheets = 5
summary_excel_max_sheets = 2
excel_max_columns = 20
summary_excel_max_columns = 12

docx_max_paragraphs = 200
summary_docx_max_paragraphs = 80
docx_max_tables = 20
summary_docx_max_tables = 8
docx_table_max_rows = 50
summary_docx_table_max_rows = 20
docx_table_max_cols = 12
summary_docx_table_max_cols = 8

pptx_max_slides = 50
summary_pptx_max_slides = 15
pptx_include_notes = true

document_excerpt_max_chars = 6000
summary_document_excerpt_max_chars = 2000
```

`ScanPlanner.build_parser_profile()` 应把这些值纳入稳定 JSON：

```json
{
  "parser_profile_version": "v1",
  "text_parser_backend": "light_text_v1",
  "office_parser_backend": "office_v1",
  "pdf_parser_backend": "pdf_text_v1",
  "excel_max_rows": 50,
  "excel_max_sheets": 5,
  "excel_max_columns": 20,
  "pdf_max_pages": 5,
  "docx_max_paragraphs": 200,
  "docx_max_tables": 20,
  "docx_table_max_rows": 50,
  "docx_table_max_cols": 12,
  "pptx_max_slides": 50,
  "pptx_include_notes": true,
  "document_excerpt_max_chars": 6000,
  "summary_mode": false,
  "total_max_chars": 50000
}
```

设计理由：

- profile 是 parse cache key 的一部分，预算变化必须导致重新解析。
- backend 版本进入 profile，parser 输出格式变化时可以通过版本 bump 避免旧缓存污染。
- summary mode 必须继续产生不同 profile，不能和 full scan 混用缓存。

## Cache / metrics / benchmark 影响

parse cache：

- 继续使用 `file_identity + parser_profile + source_version` 作为主键。
- 成功缓存保存 `content_excerpt`、`parser_backend`、`truncated`。
- 错误缓存继续保存 `parse_status="error"` 和 `parse_error`，但不作为 fresh cache 复用。
- cache hit 恢复 `parser_backend` 和 `truncated`，保持现有行为。

metrics：

- `ExtensionMetrics` 继续按扩展名记录耗时、成功、错误、timeout。
- `ReparseDetail` 建议新增 `worker_lane`：
  - `direct`
  - `subprocess`
  - `not_parsed`
- 对旧 detail 兼容：
  - `parser_backend == "subprocess"` 推断为 `worker_lane="subprocess"`。
  - `parser_backend == "not_parsed"` 推断为 `worker_lane="not_parsed"`。
  - 其他 backend 推断为 `worker_lane="direct"`，但 Office/PDF 新路径应显式填入真实 lane。

benchmark：

- JSON 保留 `parser_backend_summary`。
- `parser_backend_summary` 的 by-extension backend 表要能展示新 backend。
- `subprocess_count` 应统计 execution lane，而不是 parser backend 字符串。
- Markdown 报告中建议把表头从：

```text
| extension | backend | backend_count | subprocess_count | extension_truncated_count |
```

保留不破坏，也可增加一列：

```text
| extension | backend | backend_count | subprocess_count | extension_truncated_count |
```

其中：

- `backend_count` 是该 extension 下该 parser backend 的数量。
- `subprocess_count` 是该 extension 下实际走 subprocess lane 的数量。
- `extension_truncated_count` 是该 extension 下截断数量。

这样 benchmark 可以证明：

- `light_text_v1` 没被破坏。
- `.docx/.xlsx/.pptx` 开始出现 `office_v1`。
- `.pdf` 开始出现 `pdf_text_v1`。
- 超大文件进入 `not_parsed`，不是 subprocess。
- summary mode / full mode 使用不同 profile。
- parser profile 改变后发生 reparse，而不是 cache hit。

## 错误处理与审计

所有 parser 都必须返回 `FileContext`，并设置：

- `parser_backend`
- `truncated`
- `error`

错误码建议：

- `FILE_TOO_LARGE`
- `DOCX_PARSE_FAILED`
- `XLSX_PARSE_FAILED`
- `XLS_LEGACY_UNSUPPORTED`
- `PPTX_PARSE_FAILED`
- `PDF_PARSE_FAILED`
- `PDF_NO_TEXT_LAYER`
- `DOCUMENT_UNSUPPORTED_EXTENSION`
- `DOCUMENT_OUTPUT_TRUNCATED` 不作为 error，使用 `truncated=True` 表示

规则：

- 禁止 `try/except: pass`。
- 第三方库异常必须进入 `FileContext.error`，再进入 parse cache / reparse detail。
- 无 text layer PDF 不做 OCR，必须明确说明。
- 超过 `max_file_size_mb` 的文件在父进程直接返回 `not_parsed`，避免被误记为 subprocess timeout 或 parser 失败。
- timeout 仍由 `ParserSupervisor` 生成稳定错误，例如 `timeout: file parse exceeded 45s`。
- parser 内部的“达到预算停止”不是错误，使用 `truncated=True`。

## 测试策略

采用 TDD。先写失败测试，再写实现。

建议新增或扩展测试：

- `tests/test_document_parser.py`
  - `.docx` 正常段落文本提取。
  - `.docx` 表格提取为 Markdown-like table。
  - `.xlsx` 多 sheet 限制，超过 `excel_max_sheets` 时 `truncated=True`。
  - `.xlsx` 行数限制，超过 `excel_max_rows` 时 `truncated=True`。
  - `.xlsx` 列数限制，超过 `excel_max_columns` 时 `truncated=True`。
  - `.pptx` slide text 提取。
  - `.pptx` notes 简单提取。
  - `.pdf` text layer 提取。
  - 空白 PDF 或扫描件 fixture 返回 `PDF_NO_TEXT_LAYER`，不做 OCR。
- `tests/test_file_scanner.py`
  - 超过 `max_file_size_mb` 时 backend 是 `not_parsed`，`worker_lane` 不是 `subprocess`。
  - `worker_lane_mode="subprocess"` 时保持旧 subprocess 行为。
  - `worker_lane_mode="direct"` 下 text-like 文件继续走 `light_text_v1`。
  - Office/PDF 返回的 `FileContext` 写入并恢复 parser metadata。
  - cache hit 后仍恢复 `parser_backend` 和 `truncated`。
  - parser profile 改变后触发重新解析，cache miss reason 是 `parser_profile_changed`。
- `tests/test_scan_planner.py`
  - parser profile 包含 `office_parser_backend`、`pdf_parser_backend` 和所有新增预算。
  - summary mode 使用 summary Office/PDF 预算。
  - profile 序列化继续稳定排序。
- `tests/test_benchmark_scanner.py`
  - backend summary 能显示 `office_v1` / `pdf_text_v1`。
  - `subprocess_count` 与 parser backend 解耦。
  - `truncated_count` 覆盖 Office/PDF 截断。
- `tests/test_scan_index_store.py`
  - 如新增 `worker_lane` 需要覆盖 schema migration 和 load/cache 行为。

测试文件生成建议：

- DOCX：使用 `python-docx` 在 tmp_path 动态生成。
- XLSX：使用 `openpyxl` 在 tmp_path 动态生成。
- PPTX：使用 `python-pptx` 在 tmp_path 动态生成。
- PDF text layer：优先使用稳定小 fixture，避免为测试额外引入重依赖；如果维护成本过高，再考虑 test-only `reportlab`。
- 空白/扫描 PDF：使用稳定小 fixture 或最小空白 PDF bytes，验证 no text layer 分支。

验证命令：

```powershell
conda run -n test python -m pytest tests/test_document_parser.py -v
conda run -n test python -m pytest tests/test_file_scanner.py tests/test_scan_planner.py tests/test_benchmark_scanner.py -v
conda run -n test python -m pytest tests -q
conda run -n test python -m compileall main.py src tests
```

如果 `test` conda 环境不存在，再按项目文档或当前可用环境调整；不能静默混用系统 Python。

## 风险点 / 边界条件

- `python-docx`、`python-pptx`、`openpyxl` 都可能先加载 zip 包结构，不能把预算误解为完全流式解析；因此仍需要 `max_file_size_mb` 和 subprocess timeout。
- `.xlsx` 公式值在 `data_only=True` 下依赖工作簿是否保存过计算结果；如果没有 cached value，可能显示为空或公式结果缺失。preview 中应接受这一限制。
- `.xls` 当前 allowed，但没有明确依赖。第一阶段不应承诺 `.xls` 质量。
- PDF text layer 提取质量取决于 PDF 编码和布局，不能保证阅读顺序完全正确。
- 扫描版 PDF 应明确失败或空结果说明，不做 OCR。
- PPTX notes 有时不存在或结构异常，不能为了 notes 抛弃 slide text。
- 输出 Markdown-like table 不是审计计算表，不做金额校验。
- 新增 profile 字段会让既有 Office/PDF 缓存失效，这是正确行为；text-like profile 不应无故变化。
- 如果 `ReparseDetail` 新增 `worker_lane`，需要迁移测试覆盖，避免 benchmark 兼容性回归。
- 不要把 document parser 重新塞回 `FileScanner`，否则会破坏上一轮 scanner 边界收敛。

## 伪代码草案

### 输入

- `file_path: Path`：待解析文件路径。
- `file_type: str`：小写扩展名，例如 `.docx`、`.xlsx`、`.pptx`、`.pdf`。
- `scanner_cfg: dict`：scanner 配置，包含 size、timeout、backend、budget。
- `parser_profile: dict`：由 `ScanPlanner` 归一化后的 parser profile。
- `limits: dict`：当前 full / summary mode 下的预算。
- `cache_probe: CacheProbe`：当前文件的 cache 状态，用于 reparse detail。

### 输出

- `FileContext`：
  - `content`: Markdown-like / text preview。
  - `error`: 成功为 `None`，失败为稳定错误码前缀。
  - `parser_backend`: `office_v1`、`pdf_text_v1`、`light_text_v1`、`not_parsed` 或 `subprocess`。
  - `truncated`: 是否因 sheet/page/slide/row/column/char 预算截断。
- parse cache row：
  - success cache 保存 content/backend/truncated。
  - error cache 保存 parse_error，但不作为 fresh cache 复用。
- benchmark summary：
  - backend summary 展示真实 parser backend。
  - lane summary 或 `subprocess_count` 展示真实 subprocess 使用情况。

```python
# [伪代码草案]
# 目标：在 scanner 现有 discovery/cache/aggregation 边界内，新增 Office/PDF 的 bounded parser backend。
# 输入：
# - item: InventoryItem，包含 path、extension、file_identity、source_version
# - parser_profile: ScanPlanner 生成的稳定 profile，决定 cache key 和解析预算
# - scanner_cfg: scanner 配置，提供 size gate、timeout、worker lane 等硬约束
# - dependencies: scan_index_store、parser_supervisor、document_parser、metrics
# 输出：
# - FileContext: 文件级解析结果，必须包含 backend、truncated、error
# - parse_cache: 成功或失败都要落库，便于审计；只有 success cache 可 fresh reuse
# - reparse_detail: benchmark 使用的单文件明细，包含 backend 和 execution lane

def scan_uncached_item(item, parser_profile, scanner_cfg, dependencies):
    file_path = Path(item.path)
    file_type = item.extension.lower()

    # 1. size gate 必须在父进程先执行。
    # 为什么这样做：超大文件没有必要启动子进程或加载第三方库，否则 benchmark 会把
    # "明确不解析" 误看成 subprocess/timeout 问题。
    too_large_context = build_file_too_large_context(
        file_path=file_path,
        file_type=file_type,
        max_file_size_mb=scanner_cfg.get("max_file_size_mb"),
    )
    if too_large_context is not None:
        record_reparse_detail(
            item=item,
            context=too_large_context,
            worker_lane="not_parsed",
            cache_miss_reason="new_file_or_changed",
        )
        write_parse_cache(
            item=item,
            parser_profile=serialize_profile(parser_profile),
            context=too_large_context,
        )
        return too_large_context

    # 2. text-like 文件保持 light_text_v1 原行为。
    # 为什么这样做：这条路径刚完成性能优化，本轮只扩 Office/PDF，不能让文档 parser
    # 影响 .md/.txt/.log/.csv/.json 的读取预算和 backend 名称。
    if file_type in {".txt", ".md", ".csv", ".json", ".log"}:
        if scanner_cfg.get("worker_lane_mode", "direct") == "direct":
            context = parse_text_like_file(
                file_path=file_path,
                file_type=file_type,
                limits=limits_from_profile(parser_profile),
                options=build_light_text_options(parser_profile),
            )
            worker_lane = "direct"
        else:
            context = run_legacy_subprocess_parser(
                file_path=file_path,
                limits=limits_from_profile(parser_profile),
                timeout=resolve_file_timeout(file_type, scanner_cfg),
            )
            worker_lane = "subprocess"

        persist_parse_outcome(item, parser_profile, context, worker_lane, dependencies)
        return context

    # 3. Office/PDF 在 direct 模式下使用新的 parser backend，但仍受 timeout supervisor 管理。
    # 为什么这样做：第三方 Office/PDF 库可能在坏文件或复杂文件上卡住，单靠 Python
    # 函数内部预算不能提供 hard timeout。
    if file_type in {".docx", ".xlsx", ".pptx", ".pdf"}:
        if scanner_cfg.get("worker_lane_mode", "direct") == "subprocess":
            # 保留旧 subprocess 行为，作为兼容和回滚路径。
            context = run_legacy_subprocess_parser(
                file_path=file_path,
                limits=limits_from_profile(parser_profile),
                timeout=resolve_file_timeout(file_type, scanner_cfg),
            )
            worker_lane = "subprocess"
        else:
            context = run_document_parser_with_timeout(
                file_path=file_path,
                file_type=file_type,
                limits=limits_from_profile(parser_profile),
                options=build_document_parser_options(parser_profile),
                timeout=resolve_file_timeout(file_type, scanner_cfg),
            )
            # 这里的 lane 可能仍是 subprocess-supervised；backend 则是 office_v1/pdf_text_v1。
            # 为什么这样做：backend 说明“谁解析了内容”，lane 说明“在哪里执行”，两者不能混用。
            worker_lane = "subprocess"

        persist_parse_outcome(item, parser_profile, context, worker_lane, dependencies)
        return context

    # 4. 未支持格式返回可审计 not_parsed。
    context = FileContext(
        file_path=str(file_path),
        file_type=file_type,
        content="",
        error=f"DOCUMENT_UNSUPPORTED_EXTENSION: {file_type}",
        parser_backend="not_parsed",
        truncated=False,
    )
    persist_parse_outcome(item, parser_profile, context, "not_parsed", dependencies)
    return context


def parse_document_file(file_path, file_type, limits, options):
    # 5. 按扩展名分支，每个分支只负责格式内 extraction。
    # 为什么这样做：不同格式的预算维度不同，放在一个大函数里会让边界和测试都变弱。
    try:
        if file_type == ".docx":
            return parse_docx_bounded(file_path, limits, options)

        if file_type == ".xlsx":
            return parse_xlsx_bounded(file_path, limits, options)

        if file_type == ".pptx":
            return parse_pptx_bounded(file_path, limits, options)

        if file_type == ".pdf":
            return parse_pdf_text_layer_bounded(file_path, limits, options)

        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error=f"DOCUMENT_UNSUPPORTED_EXTENSION: {file_type}",
            parser_backend="not_parsed",
            truncated=False,
        )

    except KnownDocumentParserError as exc:
        # 6. 业务可预期异常保留稳定错误码。
        # 为什么这样做：benchmark 和 error cache 可以按错误类型归因，而不是只看到一段库异常文本。
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error=f"{exc.code}: {exc.message}",
            parser_backend=backend_for(file_type),
            truncated=False,
        )

    except Exception as exc:
        # 7. 未知异常也必须进入审计载体，不能静默失败。
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="",
            error=f"{generic_error_code_for(file_type)}: {exc}",
            parser_backend=backend_for(file_type),
            truncated=False,
        )


def parse_xlsx_bounded(file_path, limits, options):
    workbook = openpyxl.load_workbook(
        filename=file_path,
        read_only=True,
        data_only=True,
    )
    output = MarkdownPreviewBuilder(max_chars=limits.document_excerpt_max_chars)
    truncated = False

    try:
        for sheet_index, sheet_name in enumerate(workbook.sheetnames):
            if sheet_index >= limits.excel_max_sheets:
                truncated = True
                break

            sheet = workbook[sheet_name]
            output.add_heading(f"Sheet: {sheet_name}", level=2)

            # 为什么这样做：Excel 可能是大表，scanner 只需要 preview，
            # 所以按 sheet/row/column 三层预算截断，避免整本表全量转文本。
            rows_written = 0
            for row in sheet.iter_rows(
                max_row=limits.excel_max_rows + 1,
                max_col=limits.excel_max_columns + 1,
                values_only=True,
            ):
                if row_is_empty(row):
                    continue
                if rows_written >= limits.excel_max_rows:
                    truncated = True
                    break
                output.add_table_row(row[: limits.excel_max_columns])
                rows_written += 1

                if output.is_full():
                    truncated = True
                    break

            if output.is_full():
                break
    finally:
        workbook.close()

    return FileContext(
        file_path=str(file_path),
        file_type=".xlsx",
        content=output.to_markdown(),
        error=None,
        parser_backend="office_v1",
        truncated=truncated or output.was_truncated,
    )


def parse_pdf_text_layer_bounded(file_path, limits, options):
    output = MarkdownPreviewBuilder(max_chars=limits.document_excerpt_max_chars)
    extracted_text_count = 0
    truncated = False

    with pdfplumber.open(file_path) as pdf:
        for page_index, page in enumerate(pdf.pages):
            if page_index >= limits.pdf_max_pages:
                truncated = True
                break

            text = page.extract_text() or ""
            if text.strip():
                extracted_text_count += len(text.strip())
                output.add_heading(f"Page {page_index + 1}", level=2)
                output.add_text(text.strip())

            # 为什么这样做：pdfplumber page 可能缓存 layout 信息，长 PDF 中及时释放更稳。
            close_page_if_supported(page)

            if output.is_full():
                truncated = True
                break

        if len(pdf.pages) > limits.pdf_max_pages:
            truncated = True

    if extracted_text_count == 0:
        return FileContext(
            file_path=str(file_path),
            file_type=".pdf",
            content="",
            error=(
                "PDF_NO_TEXT_LAYER: no extractable text "
                f"in first {limits.pdf_max_pages} pages"
            ),
            parser_backend="pdf_text_v1",
            truncated=False,
        )

    return FileContext(
        file_path=str(file_path),
        file_type=".pdf",
        content=output.to_markdown(),
        error=None,
        parser_backend="pdf_text_v1",
        truncated=truncated or output.was_truncated,
    )


def persist_parse_outcome(item, parser_profile, context, worker_lane, dependencies):
    parser_profile_key = serialize_profile(parser_profile)

    # 8. cache metadata 必须和内容一起写回。
    # 为什么这样做：cache hit 后 benchmark 和审计仍需要知道内容来自哪个 backend、是否截断。
    dependencies.scan_index_store.upsert_parse_cache(
        file_identity=item.file_identity,
        parser_profile=parser_profile_key,
        source_version=item.source_version,
        content_excerpt=context.content if context.error is None else "",
        parse_status="success" if context.error is None else "error",
        parse_error=context.error or "",
        parser_backend=context.parser_backend or "",
        truncated=context.truncated,
    )

    dependencies.metrics.record_reparse_detail(
        path=item.path,
        extension=item.extension,
        file_identity=item.file_identity,
        source_version=item.source_version,
        parser_backend=context.parser_backend or "subprocess",
        worker_lane=worker_lane,
        truncated=context.truncated,
        parse_status="error" if context.error else "success",
        parse_error=context.error or "",
    )


def build_parser_backend_summary(reparse_details):
    summary = new_empty_summary()

    for detail in reparse_details:
        backend = detail.parser_backend or "not_parsed"
        lane = infer_worker_lane(detail)
        extension = detail.extension

        # 9. backend summary 统计“谁解析了内容”。
        # 为什么这样做：Office/PDF parser 即使在 subprocess-supervised lane 中运行，
        # 也应该能在 benchmark 里看到 office_v1 / pdf_text_v1。
        summary.by_extension[extension].backend_counts[backend] += 1

        # 10. subprocess_count 统计“在哪里执行”。
        # 为什么这样做：timeout 隔离是运行策略，不应该被 backend 名称覆盖。
        if lane == "subprocess":
            summary.subprocess_count += 1
            summary.by_extension[extension].subprocess_count += 1
        elif lane == "not_parsed":
            summary.not_parsed_count += 1
        else:
            summary.direct_count += 1

        if detail.truncated:
            summary.truncated_count += 1
            summary.by_extension[extension].truncated_count += 1

    return summary.to_dict()
```
