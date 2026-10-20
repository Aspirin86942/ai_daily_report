# 历史产物透明压缩实施结果

验收时间：2026-10-18 18:39（Asia/Shanghai）。[实施计划](2026-10-18-historical-artifact-compression.md)的第一阶段已完成：工具实现、78 项隔离测试、逐任务审查、最终整体审查及定向复审、真实试批、压缩、全量核验和正式运行对比均通过。

## 实际空间变化

| 口径 | 之前 | 之后 | 变化 |
| --- | ---: | ---: | ---: |
| 两处目标文件数据占用 | 14,279,826,110 B | 5,401,958,732 B | **释放 8,877,867,378 B（8.88 GB，62.1707%）** |
| 项目全部文件数据，硬链接去重 | 73,921,613,875 B | 65,066,161,384 B | **净减 8,855,452,491 B（73.92 → 65.07 GB）** |
| D: 整卷空闲 | 399,044,878,336 B | 407,923,941,376 B | 增加 8,879,063,040 B |

GB 为十进制。文件数据用 GetCompressedFileSizeW / 原生压缩信息测量，不含全部文件系统元数据；整卷空闲包含其他活动，单列口径。新增清单、日志和审计证据使项目逻辑文件数据增加 22,414,887 B，目标节省减去该增量，恰好等于项目净减量。前后完整盘点均无错误，均跳过同一个已有重解析点。

体检早期、缓存清理前的同口径项目文件数据为 80,780,969,544 B；先清理已授权的两个构建缓存，再完成本阶段，现为 65,066,161,384 B，累计减少 **15,714,808,160 B（约 15.71 GB）**。此前缓存清理与本阶段透明压缩分别保留证据，不以整卷空闲混算项目占用。

## 范围与完整性

仅操作以下历史 extractions 中的 body.md、units.jsonl、units.idx；其他目录不自动进入后续阶段。

| 允许根 | 文件 | 逻辑字节（保持） | 压缩后文件数据 | 释放 |
| --- | ---: | ---: | ---: | ---: |
| [archive-benchmark-final-20261017-04](D:/python_program/ai_daily_report/.artifacts/rust-migration/archive-benchmark-final-20261017-04/incremental/data/archive/extractions) | 9,129 | 7,328,993,694 | 2,769,158,785 | 4,559,834,909 |
| [archive-benchmark-final-20261017](D:/python_program/ai_daily_report/.artifacts/rust-migration/archive-benchmark-final-20261017/incremental/data/archive/extractions) | 7,512 | 6,950,832,416 | 2,632,799,947 | 4,318,032,469 |
| 合计 | **16,641** | **14,279,826,110** | **5,401,958,732** | **8,877,867,378** |

- 冻结清单 eligible 16,641、skipped 0，原先均未压缩且 nlink=1；每种文件名各 5,547 个。清单与规划基线完全一致。
- 真实试批 679 项、275,060,886 B，核验后占用 103,366,993 B，随后按同一清单继续。
- apply 于 18:01:15–18:21:17 完成，16,641 项全部 changed/verified；逐项操作前后完整 SHA-256、随机块、身份、长度及 mtime 核验通过。
- 独立 verify 于 18:21:35–18:37:59 对全部 16,641 项复读通过，files_changed=0、compressed=16,641、uncompressed=0。
- 独立元数据盘点确认目标集合与清单相等，missing/extra/read errors/reparse 均为 0，nlink、身份、大小、mtime 和压缩属性匹配。
- 路径、内容、逻辑长度及索引偏移保持，原件、数据库、未完成运行、响应账本、失败样本、旧包、配置、Python 恢复环境和匹配 Worker 均保留。实际 rollback 未执行。

## 正式运行与工具交付

18:00 Collect 真实日历触发自然完成、结果 0；操作前后三个任务均 Ready，任务定义 SHA-256 不变。下一次 Collect 为 20:00，Health 为 23:00；日报下一次日历触发仍为 2026-10-19 09:00，尚未发生，不以本轮离线测试替代。

正式 incremental-06 包 445 文件校验通过，doctor --strict 退出 0，数据库、11 模板、Worker 握手正常，model.calls=0、checked=false。已发布 10 月 17 日报告 SHA-256 与 5 个保留 Worker 哈希前后一致。health 仍仅有原 3 条历史失败提醒，pending=0，其余检查通过，没有本轮新增错误。

仓库入口为[维护脚本](../../../scripts/compress_historical_artifacts.py)、[78 项测试](../../../tests/test_historical_artifact_compression.py)与[维护说明](../../../scripts/README.md)。18:39 验收阶段在 historical-artifact-compression 隔离工作区完成，当时未提交或发布；20:32 用户追加提交、合并及远端发布授权。工具仅使用已有 Python 标准库及 Windows API，不增加 Rust 产品 Python 依赖；没有重建 Rust、重新解析、生产模型调用或任务改写，正式 incremental-06 程序包保持。

工具 SHA-256：`F6A76ECC1779DE9052675983EE14DFEE210333A2E6766D60974F893C7481FFC8`。失败后空间汇总的 T2-M1 限制已明确接受并写入说明；本次收益采用成功运行及独立前后测量，未使用失败汇总。

旧性能数字继续代表原测量时状态；后续复测须标明这两处材料已经透明压缩。

## 冻结证据与回退入口

证据根：[20261018-173835](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835)。

- [不可变清单](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835/manifest.json)及其 sidecar：原始 9,582,336 B，SHA-256 `35587a66daf5bf0a895c3789d27f36f678d12d4f5cafa811069e3a859c0c9e25`；plan_id `af0fc8998f0a42c4883d96092f7b9535`。
- [apply 汇总](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835/runs/fb670d5e25d54068adc2bd955392d2c7/summary.json)及同目录 events/task-windows；run_id `fb670d5e25d54068adc2bd955392d2c7`。
- [全量 verify 汇总](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835/runs/1088f51d0194465aae917241a86f97c8/summary.json)；run_id `1088f51d0194465aae917241a86f97c8`。
- [实际操作结果](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835/phaseb-audit/task-4-phase-b-results.json)、[空间变化](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835/phaseb-audit/space-change.json)及[正式运行前后对比](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835/production-checks/comparison.json)。
- [当前回退命令](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835/rollback-current.md)：21:14 清理已合并工作区后，使用 recovery-source 中保持原 SHA 的冻结源码；迁移后 78 项测试通过。[验收时原始命令](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835/phaseb-audit/rollback-command.md)继续保留。需要恢复时，以同一清单执行 rollback 后再 verify，并保留解压增量外 1 GiB；本次没有实际回退。
- [最终修复复审](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835/development-audit/final-fix-1-rereview.md)及[执行裁定/范围边界](D:/python_program/ai_daily_report/.artifacts/rust-migration/historical-artifact-compression/20261018-173835/development-audit/final-review-dispositions.md)；逐任务报告、RED/GREEN、全部失败与中间证据归档至同一 development-audit 目录；这些本机证据和业务内容不随源码推送。
