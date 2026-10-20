# 项目可再生内容清理结果

2026-10-18（Asia/Shanghai）。用户授权“删除本项目不必要的内容”；按运行用途、恢复依赖、Git 归属和独立盘点确定精确目录后实际删除，未按目录名称批量丢弃历史材料。

## 实测变化

| 口径 | 清理前 | 清理后 | 减少 |
| --- | ---: | ---: | ---: |
| 项目文件数据，硬链接去重 | 65,068,328,889 B | 51,518,727,116 B | **13,549,601,773 B（约 13.55 GB）** |
| Rust 目录文件数据 | 14,109,111,770 B | 780,275,681 B | 13,328,836,089 B |
| 快照文件路径数 | 156,020 | 137,443 | 18,577 |

GB 为十进制。沿用 GetCompressedFileSizeW 与文件身份去重口径，不含全部文件系统元数据。前后全量盘点错误均为 0，均跳过同一处已有重解析点；快照后的最终凭据与本文会增加少量文件数据。D: 整卷空闲从 407,920,914,432 到 421,485,404,160 B，包含其他活动，另列而不替代项目收益。

## 实际删除

- 6 处 Rust 构建缓存：debug 的 deps、examples、build、.fingerprint，以及 target 的 flycheck0、空 tmp。顶部程序、旧匹配 Worker、固定 PDFium 与旧 crate-local 构建目录保留。
- 13 处源码/测试目录的 Python 字节码与 pytest 缓存。
- uv 下载缓存 `.uv/cache`；已有 `.venv`、基准记录和所需包文件保留。硬链接共享数据按保留文件扣除，没有把逻辑路径字节当作实际释放量。
- 已合并的 historical-artifact-compression 工作区及本地分支；无强制删除。另一 report-coverage 工作区保持。仅移除本轮失效的本地 exclude 条目及空父目录。

缓存合计 20 个目录、17,793 个文件路径。逐项目录在删除前核验绝对路径、无跟踪文件、无重解析、无编译进程；全部成功，无重试或属性/ACL 改写。

## 恢复入口与验证

原工作区 114 个忽略文件均有核对字节哈希的保留副本，其中 113 项开发审查材料已归档。冻结工具和测试从已合并提交 `dd5ddf3` 保持原字节保存，说明文件另保持原换行。工具 SHA 仍为 `F6A76ECC1779DE9052675983EE14DFEE210333A2E6766D60974F893C7481FFC8`；迁移后的 78 项隔离测试全部通过。当前[历史产物回退入口](../../.artifacts/rust-migration/historical-artifact-compression/20261018-173835/rollback-current.md)已替代已移除的 CodeRoot；原始命令和全部验收记录保持，真实 rollback 未执行。

清理后 21:15 的只读复检：

- 正式 incremental-06 包 445 文件校验、doctor --strict、数据库、11 模板及工作进程握手通过，model.calls=0、checked=false。
- 三项任务定义、入口、参数及工作目录未改，均 Ready；20:00 采集自然触发结果 0，下一次 22:00，日报下一次仍为 10 月 19 日 09:00。
- 已发布报告哈希、5 个保留 Worker 哈希不变；另独立重算 100 个保留构建/运行文件，大小、mtime 和 SHA 均与前值一致。
- 旧 Python 仍为 3.13.13，源码原生 DLL 在清理前后均可成功导入。health 仍只有原 3 条历史失败提醒，pending=0，无新增错误。
- data、.tmp、.releases、.venv、logs、config、config-rust 和 outputs 的文件数及逻辑字节保持；业务源码、配置、任务和解析覆盖保持，OCR 未改。

旧包、恢复环境、数据库、响应账本、未完成运行、失败材料、原件、历史测量及工作簿继续按用途保留。现有程序继续使用已验收包；后续 Rust 开发构建和包安装会按需重建或下载缓存。

## 本机清理证据

[完整证据目录](../../.artifacts/rust-migration/project-cleanup-20261018-2053/)包括删除白名单、独立盘点、逐文件清单、原生退出码和操作流水、前后占用快照、运行复检、恢复迁移与分支安全检查。结论集中在[最终核验](../../.artifacts/rust-migration/project-cleanup-20261018-2053/final-verification.json)、[缓存删除报告](../../.artifacts/rust-migration/project-cleanup-20261018-2053/cache-delete-report.md)和[工作区清理结果](../../.artifacts/rust-migration/project-cleanup-20261018-2053/worktree-delete-result.json)。这些本机材料包含恢复与审计用途，不随源码自动上传。
