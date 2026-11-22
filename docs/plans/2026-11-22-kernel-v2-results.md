# 个人日报内核 v2 本机验收

**已通过 Windows 离线验收。** 按本次交付口径核验 36 项，全部通过。Windows 包 `offline-v2-04` 的发布清单 SHA-256 为 `bf27689c17127d01edb5b933aaf1e6be568c6cc6320fea755785922ce2a3f296`。测试与计时均绑定该清单及对应源码快照。11 月 22 日 12:46，同一份包已部署到本机正式环境，实际任务结果另行记录。[正式运行记录](../current-state.md) · [逐项验收](../../.artifacts/kernel-v2-20261122/candidate-offline-v2-04/acceptance.json) · [候选清单](../../.artifacts/kernel-v2-20261122/candidate-offline-v2-04/package/manifest.json)

Windows x64 离线压缩包共 9.84 MiB，342 个条目均已从 ZIP 逐项读回核验。包内含两个 EXE、固定版本 PDFium、3 份活动模板和运维脚本。[下载离线包](../../.artifacts/kernel-v2-20261122/candidate-offline-v2-04/ai-daily-report-offline-v2-04-windows-x64.zip) · [压缩包 SHA-256](../../.artifacts/kernel-v2-20261122/candidate-offline-v2-04/delivery.json)

## 同机指标

| 项目 | 实测 | 验收上限 |
| --- | ---: | ---: |
| 48 文件冷扫描中位数 / P95 | 202.78 / 232.63 ms | 400 / 480 ms |
| 主程序和 PDF 子程序峰值工作集 | 50.76 MiB | 52 MiB |
| 13 文件四批暖扫合计中位数 | 13.78 ms | 18 ms |
| 各批暖扫中位数 | 4.07 / 3.63 / 3.65 / 2.39 ms | 每批 4.5 ms |
| 2,064 个文件的目录发现 | 13.00 ms | 20 ms |
| 225 条历史报告首次建索引 | 2.000 s | 2.8 s |
| 独立进程热查询 / 重读原报告 | 12.552 ms / 0 行 | 14 ms / 0 行 |
| 全文索引 / 前五结果命中 | 41.92 MiB / 20/20 | 45 MiB / 20/20 |
| Rust 工作区成员 / 成员源码物理行数 | 3 个 / 35,637 行 | 3 个 / 行数按实记录（原目标 18,000 行） |

扫描和检索均与旧内核三期交替运行。冷扫按每种并发 20 对采样，暖扫 10 对，发现 12 对。检索包含 6 对冷建和 54 次独立进程热查，每轮比较结果及排序。[扫描逐轮数据](../../.artifacts/kernel-v2-20261122/bench/scans/offline-v2-04/scans.json) · [检索逐轮数据](../../.artifacts/kernel-v2-20261122/bench/search/offline-v2-04/search.json)

## 日报、模型调用和恢复

新正文最多 1,500 个 Unicode 字符，含标点、换行和正文引用。附件保存本次采纳的全部依据和位置。6 份语义文件通过 8 项正文事实、9 项附件定位与 39 处原文范围校验，3 份历史冻结答复逐字节回放。[语义和回放](../../.artifacts/kernel-v2-20261122/bench/semantics/offline-v2-04/verification.json)

隔离副本保有 223 份日报、1 份周报、1 份月报和 1,668 条模型调用明细。从历史发布基线重建 283 个文件，其中 265 个可在业务记录中找到原始字节，18 个从封存原件取回。候选程序实际按日期恢复 221 份有当前运行记录的日报及 26 份附件。只保有原件的 18 个文件继续使用独立离线工具。[离线恢复用法](../daily-report-operations.md#故障恢复与历史查询) · [283 个发布文件](../../.artifacts/kernel-v2-20261122/all-publications-6ggmux7x/receipt.json) · [历史 CLI 恢复](../../.artifacts/kernel-v2-20261122/candidate-offline-v2-04/history/receipt.json) · [本轮离线恢复 18 个原件](../../.artifacts/kernel-v2-20261122/candidate-offline-v2-04/historic-originals.log)

新旧程序在同一隔离库中完成发布、回读和恢复，核对已入库待发布、结果未知和版本专属未完成计划。回环模型替身记录实际发送次数，远端请求和正式任务写入均为 0。业务验收基线单独封存。11 月 22 日上午的自然增长由旧正式包产生，当日只读快照为 224 份日报和 285 个发布文件。[跨版本交接](../../.artifacts/kernel-v2-20261122/candidate-offline-v2-04/compat/receipt.json) · [当日业务快照](../../.artifacts/kernel-v2-20261122/daily-observation-20261122-103010-833392/receipt.json)

## 源码和测试迁移

旧版同口径为 58,527 行。候选的 `src/**/*.rs` 含 31,870 行非测试源码与 3,767 行测试专用源码，均含空行和注释。另有独立 Rust 测试 14,353 行、示例 623 行和构建脚本 146 行。[逐文件源码计数](../../.artifacts/kernel-v2-20261122/candidate-offline-v2-04/size.json)

三个包分别为`daily_parsers` 11,988 行、`pdf_worker` 1,630 行、`daily_report` 22,019 行。实际规模比原目标 18,000 行多 **17,637 行**。2026 年 11 月 22 日，用户确认保留全部功能，以实测行数完成离线交付。

| 旧版测试覆盖 | v2 验收入口 |
| --- | --- |
| `text_parser`、`office_parser`、`discovery` | `daily_parsers/tests/{text,office,discovery}` 及包内单元测试复核原文、范围、格式上限与 Windows 文件身份 |
| `worker_contract`、`pdf_parser` | `daily_parsers::pdf_wire` 和 `pdf_worker/tests/pdf_stdio.rs` 验证帧、进程异常、原生打开次数、页级观察与可选识字 |
| `scanner_contract`、`scanner_core` | 历史输入样本在 `daily_parsers/tests/settings_fixtures`，新调度、缓存写前日志和跨进程互斥在 `daily_report/src/scan` 及集成用例中验证 |
| `report_app`、构建身份和 Windows 包 | `daily_report/tests`、`tests/test_parser_content_identity.py`、包内独立用例及 Windows 任务替身覆盖生成、历史恢复、身份和交接 |
| 退休的归档查询、旧协议和扫描库 | 保留的旧三期包及其原有验证夹具，操作说明见[历史归档](../change-archive.md) |

锁定工作区测试通过 358 项，失败 0 项，默认跳过 8 项。默认跳过的 8 项已在配套环境逐项通过。[专项测试逐项回执](../../.artifacts/kernel-v2-20261122/candidate-offline-v2-04/special-cases.json)。真实 PDF 准入、旧答复回放和归档回读另行执行。独立 Windows 包核对 341 个清单文件。[锁定工作区检查](../../.artifacts/kernel-v2-20261122/verified-workspaces/offline-v2-04/receipt.json)

冷扫提交 1 次，同日暖扫 0 次，跨日首次访问 1 次，再次同日命中 0 次。[缓存提交标记](../../.artifacts/kernel-v2-20261122/bench/wal/offline-v2-04/verification.json)。可选 OCR 配对构建通过独立用例。[OCR 验收](../../.artifacts/kernel-v2-20261122/candidate-offline-v2-04/ocr/receipt.json)
