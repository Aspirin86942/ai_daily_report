# 扫描与解析后端

## 当前调用关系

Rust CLI 的 report_app 负责报告、模型调用和恢复。个人日报在一次运行中依次扫描、解析、总结并发布。scanner_core 复用未变文件的解析缓存；archive 只处理历史证据查询、核验和存储维护。

~~~text
ai-daily-report.exe
  ├─ archive → 历史原件、版本、差异与证据查询
  ├─ weekly/monthly --source db → 已发布日报汇总
  └─ daily/weekly/monthly --source scan → scanner_core
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

## 工作进程与缓存

Office/PDF 工作进程使用协议 v4，以逐行 JSON 完成启动握手、能力与构建身份检查、请求及响应。归档解析支持分块输出正文和位置。请求数、空闲时间、内存、运行时间或协议异常达到限制时回收进程。

三个 EXE 一起构建，两个工作进程必须具有匹配的构建身份。旧包继续使用各自匹配的工作进程。PDFium 的版本和 SHA-256 固定，发布校验会检查 DLL。

扫描缓存身份包含规范化配置、解析器、通道、构建身份、预算、超时及原件指纹。影响解析结果的设置变化必须改变缓存身份。源文件在读取期间变化时保留诊断和既有重试边界。

扫描结果一次返回上下文、来源、诊断及审计证据。冷扫描表示目标缓存中没有可用解析结果；热扫描复用兼容结果。比较时保持输入和内容签名一致，并区分进程内工作量、并行耗时之和与实际完成耗时。

## 验证与阅读

使用隔离目录、固定样本和本地模拟模型验证每日扫描、冻结恢复与重复发布。测试入口为 `rust/report_app/tests/personal_scan.rs` 和 `bundle_acceptance.rs`。旧归档性能测量保留原始数据和口径，不能作为当前日扫性能结论；旧采集基准脚本已退役。计时期间不并行运行构建或测试。

- [历史归档查询](change-archive.md)：全文、差异、引用核验和存储维护。
- [历史性能测量](archive/history/archive-performance-20261017.md)：全量样本、覆盖缺口和原始测量口径。
- [当前状态](current-state.md)：正式任务及验收状态。
- [术语表](../CONTEXT.md)：程序中各层职责。
