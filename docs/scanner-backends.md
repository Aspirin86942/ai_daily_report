# 扫描与解析后端

## v2 调用关系

主程序负责文件顺序、材料预算、报告库与发布。文本和 Office 直接返回带原文位置的 Rust 结构。PDF 子程序在同一文档句柄里检查选定页并提取正文。v4 缓存保存可重建的解析结果，发布依据独立保存在报告记录中。

~~~text
ai-daily-report.exe
  └─ daily → daily_report::scan
       ├─ daily_parsers::discovery → 文件及来源身份
       ├─ daily_parsers::text / office → 原文及来源范围
       └─ ai-daily-pdf-worker.exe + pdfium.dll → 页文本及页级观察
~~~

旧 Python 的 NativeScanner 与 PyO3 调用链已随匹配版本归档，恢复规程见[恢复说明](../.artifacts/kernel-once-20261121/legacy/RECOVERY.md)和[历史扫描说明](archive/python-scanner-backends-2026-10-17.md)。

## 格式与执行通道

parser_backend 表示实际生成内容的解析器；worker_lane 表示执行位置。性能证据同时记录两者。

| 真实格式 | parser_backend | worker_lane |
| --- | --- | --- |
| 文本、Markdown、CSV、JSON、日志 | light_text_v2 | rust_core |
| XLSX | rust_xlsx_bounded_v2 | rust_office_in_process |
| XLS | rust_xls_calamine_v3 | rust_office_in_process |
| DOCX、PPTX | rust_office_oxide_v2 | rust_office_in_process |
| 显式启用的 DOC、PPT | rust_legacy_office_v3 | rust_office_in_process |
| PDF 文字层 | rust_pdfium_v3 | rust_pdf_process_v3 |
| 逐页确认的纯图 PDF，显式启用识字 | rust_pdfium_ocr_v1 | rust_pdf_process_v3 |

纯图 PDF 可在同一解析进程内用 RustO 识字。每段识别文字保留原页页码，标记为待核实。文件与页数配额在一次扫描内共享。

按真实格式选择一个 Rust 解析路径。DOC/PPT 默认关闭，需同时启用 legacy_office_enabled 并允许对应扩展名。解析错误和内容限制保留为覆盖缺口；已退休的 Python 回退配置会被拒绝。

日扫为完整证据块保留原文位置。CSV 记录按逻辑记录号定位，Excel 保留表、行和列，Word 关联表格题注，PowerPoint 标注图表数值和备注所在页，PDF 在选定页间分配可用字数。范围外的行、页和没有可提取文字的文件会进入覆盖说明。

### 上游实现与本地验证

| 用途 | 本项目如何复用 | 上游项目 |
| --- | --- | --- |
| Excel 工作簿与 Office 文件内的 XML | 用 calamine 读取旧式 XLS。用 quick-xml 读取 XLSX 单元格、演示文稿图表及其关系，保留来源位置和读取预算。 | [calamine](https://github.com/tafia/calamine)、[quick-xml](https://github.com/tafia/quick-xml) |
| PDF 文字与版面 | 通过 pdfium-render 调用随包固定的 PDFium，读取字符和坐标，用于还原行、栏和页码。 | [pdfium-render](https://github.com/ajrcarey/pdfium-render) |
| CSV 记录边界 | 在开发测试中用 rust-csv 独立读取同一份输入，逐条比对带引号和跨行的记录。 | [rust-csv](https://github.com/BurntSushi/rust-csv) |
| 演示文稿提取质量 | 在验收时用 MarkItDown 独立转换同一份 PPTX，核对图表类别、系列、数值、备注和组合形状。 | [Microsoft MarkItDown](https://github.com/microsoft/markitdown) |

## 工作进程与缓存

主程序与 PDF 工作程序同批构建。启动时核对解析器身份与固定 PDFium 的 SHA-256。协议 v1 为每帧添加四字节长度前缀、版本和请求编号。一次业务请求完成页窗检查和正文提取。达到单文件截止时间或协议出错时回收子程序，保留已经确认的页级观察。

文本和 Office 受实际读取、解压、结构深度、单元格大小和截止时间约束。默认并发最多四路。PDF 同时受进程名额和内存预算限制，显式单路配置按顺序执行。

新扫描库 `scan_cache_v4.sqlite3` 只保存解析缓存、PDF 负向结果和快照三张表。缓存键包含来源身份、影响输出的设置、解析器构建身份和实际页窗。命中后仍复核记录哈希及快照引用。正常冷扫合并为一次写事务，同日全命中零写，跨日访问日合并更新。旧三期包继续使用原配的协议 v4 工作程序和独立 v3 扫描库。

扫描结果一次返回上下文、来源、诊断及审计证据。冷扫描表示目标缓存中没有可用解析结果；热扫描复用兼容结果。比较时保持输入和内容签名一致，并区分进程内工作量、并行耗时之和与实际完成耗时。

## 验证与阅读

v2 使用本机验收目录的 `bench/v2_semantics.py`，由同一份候选清单和构建驱动核对解析、冻结材料、实际请求、正文和依据。固定用例覆盖 8 项正文事实、9 项附件定位、39 处原文范围及 3 份冻结答复。[验收入口](plans/2026-11-22-kernel-v2-three-phase-plan.md)

使用隔离目录、固定样本和本地模拟模型验证每日扫描、冻结恢复与重复发布。测试入口为 `rust/daily_report/tests/personal_scan.rs` 和 `bundle_acceptance.rs`。旧归档性能测量保留原始数据和口径，不能作为当前日扫性能结论；旧采集基准脚本已退役。计时期间不并行运行构建或测试。

- [历史归档查询](change-archive.md)：全文、差异、引用核验和存储维护。
- [历史性能测量](archive/history/archive-performance-20261017.md)：全量样本、覆盖缺口和原始测量口径。
- [当前状态](current-state.md)：正式任务及验收状态。
- [术语表](../CONTEXT.md)：程序中各层职责。
