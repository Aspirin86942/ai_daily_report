# 扫描与解析后端

## 当前调用关系

Rust CLI 的 report_app 负责报告、模型调用和恢复。个人日报在一次运行中依次扫描、解析、总结并发布。scanner_core 复用未变文件的解析缓存；archive 只处理历史证据查询、核验和存储维护。

~~~text
ai-daily-report.exe
  ├─ archive → 历史原件、版本、差异与证据查询
  └─ daily --source scan → scanner_core
       ├─ Rust 内部轻文本解析
       ├─ ai-daily-office-parser.exe
       └─ ai-daily-pdf-parser.exe + pdfium.dll
~~~

旧 Python 的 NativeScanner 与 PyO3 调用链继续保留用于旧版恢复，其规则见[历史扫描说明](archive/python-scanner-backends-2026-10-17.md)。

## 格式与执行通道

parser_backend 表示实际生成内容的解析器；worker_lane 表示执行位置。性能证据同时记录两者。

| 真实格式 | parser_backend | worker_lane |
| --- | --- | --- |
| 文本、Markdown、CSV、JSON、日志 | light_text_v2 | rust_core |
| XLSX | rust_xlsx_bounded_v2 | rust_office_process_v3 |
| XLS | rust_xls_calamine_v3 | rust_office_process_v3 |
| DOCX、PPTX | rust_office_oxide_v2 | rust_office_process_v3 |
| 显式启用的 DOC、PPT | rust_legacy_office_v3 | rust_office_process_v3 |
| PDF | rust_pdfium_v3 | rust_pdf_process_v3 |

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

Office/PDF 工作进程使用协议 v4，以逐行 JSON 完成启动握手、能力与构建身份检查、请求及响应。归档解析支持分块输出正文和位置。请求数、空闲时间、内存、运行时间或协议异常达到限制时回收进程。

DOCX、PPTX 和旧式 XLS 的上游库在选取摘录前就会展开文档部件或解码工作簿。默认 512 MiB 是单个请求结束后判断是否回收工作进程的阈值。大型文档首次读取的峰值可能更高，超时会作为覆盖缺口记录。

三个 EXE 一起构建，两个工作进程必须具有匹配的构建身份。旧包继续使用各自匹配的工作进程。PDFium 的版本和 SHA-256 固定，发布校验会检查 DLL。

扫描缓存身份包含规范化配置、解析器、通道、构建身份、预算、超时及原件指纹。影响解析结果的设置变化必须改变缓存身份。源文件在读取期间变化时保留诊断和既有重试边界。

扫描结果一次返回上下文、来源、诊断及审计证据。冷扫描表示目标缓存中没有可用解析结果；热扫描复用兼容结果。比较时保持输入和内容签名一致，并区分进程内工作量、并行耗时之和与实际完成耗时。

## 验证与阅读

`scripts/verify_rich_facts.py` 用同一组合成文档核对解析、冻结材料、实际请求、正文和依据五个环节。创建样本使用 Python 开发环境中的 python-docx、python-pptx、openpyxl、pypdf。传入 `--bundle`、`--corpus` 和新的 `--output` 目录，候选包加 `--require`。可用 `--markitdown-python` 指向独立安装 MarkItDown 的测试环境，核对同一份演示文稿的系列、类别、数值、备注和组合形状。日常生成使用 Rust 解析程序。

使用隔离目录、固定样本和本地模拟模型验证每日扫描、冻结恢复与重复发布。测试入口为 `rust/report_app/tests/personal_scan.rs` 和 `bundle_acceptance.rs`。旧归档性能测量保留原始数据和口径，不能作为当前日扫性能结论；旧采集基准脚本已退役。计时期间不并行运行构建或测试。

- [历史归档查询](change-archive.md)：全文、差异、引用核验和存储维护。
- [历史性能测量](archive/history/archive-performance-20261017.md)：全量样本、覆盖缺口和原始测量口径。
- [当前状态](current-state.md)：正式任务及验收状态。
- [术语表](../CONTEXT.md)：程序中各层职责。
