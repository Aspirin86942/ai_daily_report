# 当前运行与验收状态

## 日报正文字数硬上限提升至 1,500 字并恢复 10 月 28 日日报（2026-10-29，Asia/Shanghai）

正式程序于 **11:35:36** 更新正式配置、**11:36:49** 完成两项任务注册，切换至 `.releases/rust-20261029-summary-1500-01`，活动配置仍为 `config-rust`。两个计划任务的执行动作与工作目录、配置中的 Office/PDF 解析程序路径同步指向新包；触发器、权限、启用状态和超时逐项保持。

新生成个人日报仍以 420～560 字为目标、最多 5 项，正文字符硬上限由 600 提升至 **1,500**。初稿不超过 1,500 字时直接发布，不再进入精简步骤、不记内容缺口、不触发历史恢复告警；只有超过 1,500 字才进入独立精简步骤，精简失败继续按优先级缩减整项并保留下一步计划。本轮识别出旧规则的结构性问题：10 月 29 日 09:00 的初稿为 5 项 913 字，精简两次均照抄原稿，旧校验判为无效后按 600 字硬减量，丢弃 2 项非计划事项，日报仅剩 2 项 346 字，并产生 `MODEL_RESPONSE_INVALID` 缺口与历史恢复告警。新规则不再把「初稿未精简」当作缺口。历史 v4/v5 计划、历史提示词字节及其响应校验保持原样，仍按 5 项/600 字浓缩。

| 任务 | 本次实际执行 | 下次计划执行 |
| --- | --- | --- |
| `AI Daily Report` | 11:38:05 启动，退出 0；复用 10 月 28 日已发布日报，未调用模型 | 2026-10-30 09:00，目标日期 10 月 29 日 |
| `AI Daily Report Health` | 11:38:26 启动，退出 1（预期 warning）；**0 项错误、1 项预警** | 2026-10-29 23:00 |

唯一预警 `REPORT_CONTENT_PARTIAL` 仅来自既有材料覆盖边界（`EXCERPT_BOUNDED` 34 条、`PARSE_BOUNDED` 31 条，选择阶段 omissions 为空）；`HISTORICAL_REPORT_ATTENTION` 已回到 ok「没有未恢复的个人日报异常」。健康程序实际退出码与预期退出码均为 1。

10 月 28 日日报已按新口径重新生成：`daily --target-date 2026-10-28 --regenerate --reason "..."` 发布 run `6a73339e467e14e7e69da5ba036491bd`，5 项、正文 990 字、单次模型调用、无任何 `MODEL_` 缺口；`quality=partial` 仅因材料覆盖边界。`data/reports/2026-10/2026-10-28.md` 由 1,957 字节增至 3,705 字节，新附件 32,389 字节，原附件字节保留。

切换后业务表：200 条日报、1 份周报、1 份月报、270 条运行记录、1,666 条模型调用记录。10 月 29 日 09:00 的 3 次调用及精简失败响应仍作为恢复前证据保留在运行、调用账本与快照中。处理缓存 19,849,741 字节。

新包通过 `scripts/verify_windows_release.ps1` 完整文件清单核验（471 个文件），`doctor --strict` 通过（20 份模板、同版解析程序握手、报告库与扫描库），`doctor --check-model` 记录 1 次成功调用（run `128eefd68cfb6f8a5e0829c20ead3cb9`）。源码侧通过格式检查、严格 Clippy 与锁定工作区测试，并新增 1,500 字边界、913 字初稿直接发布、精简失败保留完整初稿的回归覆盖。

本次证据：[部署最终核验](../.artifacts/personal-summary-extension-20261029/deployment-01/final-verification.json)、[恢复运行与报告核对](../.artifacts/personal-summary-extension-20261029/deployment-01/regenerate-2026-10-28.json)、[任务安装回读](../.artifacts/personal-summary-extension-20261029/deployment-01/check-daily.json)。配置、任务 XML、报告库及扫描库一致备份位于同一部署目录的 `before`，哈希见 [backup-manifest.json](../.artifacts/personal-summary-extension-20261029/deployment-01/backup-manifest.json)。

上一版本 `.releases/rust-20261028-summary-compact-01` 完整保留。回退仅恢复程序路径、任务动作和配套配置，继续保留当前业务数据库及新增数据，详见[备份与回退](windows-deployment.md#备份与回退)。

## 前一版本记录：日报精简版本已切换并实跑验证（2026-10-28，Asia/Shanghai）

以下保留 10 月 28 日当时的验收结果；当前规则、正式版本和任务状态以本页上方的 10 月 29 日记录为准。

正式程序于 **14:31:33** 切换至 `.releases/rust-20261028-summary-compact-01`，活动配置为 `config-rust`。日报任务、健康检查脚本、主程序及 Office/PDF 解析程序均指向同一新包。现有任务仅更新执行动作；触发器、权限、启用状态和超时设置逐项核对保持。

| 任务 | 本次实际执行 | 下次计划执行 |
| --- | --- | --- |
| `AI Daily Report` | 14:33:21 启动，退出 0；585 ms 内复用 10 月 27 日已发布日报；已启用、Ready | 2026-10-29 09:00，目标日期 10 月 28 日 |
| `AI Daily Report Health` | 14:33:24 启动，退出 1；7419 ms 内正常完成；已启用、Ready | 2026-10-28 23:00 |

上述执行由 Windows 任务计划程序实际启动。两项日志均为 `execution_status=succeeded`；健康检查为 **0 项错误、1 项预警**，唯一预警 `REPORT_CONTENT_PARTIAL` 来自已发布日报的材料覆盖及旧版事项缩减记录。健康程序的实际退出码与预期退出码均为 1。日报继续每天 09:00 执行，保留 `StartWhenAvailable` 和 6 分钟时限；健康检查继续每天 11:00、23:00 执行。

当时的规则（已于 10 月 29 日把正文硬上限提升至 1,500 字，见上）：新生成日报超过 600 字时，先进入独立精简步骤，保留全部事项、顺序、来源及下一步计划；精简失败才缩减低优先级的非计划事项。该策略仅适用于当时的新生成日报；本次实跑复用了既有发布，新增真实模型调用 **0**。10 月 28 日 09:00 的自然执行仍使用更早版本；当版新版本下一次自然生成尚未发生。

正式配置通过 `doctor --strict`，数据库、两个解析程序通信和 18 份模板均正常。部署前通过格式检查、严格 Clippy、568 项工作区测试及 2 项独立程序包测试；工作区常规忽略 3 项，其中 2 项程序包验收已单独执行。604 字原稿先精简、精简失败保留计划等回归已用本地模型替身验证。新包 469 条文件清单已核验。

切换前后的业务表数量一致：199 份日报、1 份周报、1 份月报、267 条运行记录、1,661 条模型调用记录。217 份报告与附件逐文件哈希保持。处理缓存由 52 份快照变为 51 份：10 月 20 日已完成日报的一份旧输入按现有 7 天规则到期清理，数据库中的过期快照引用同步释放，其余运行字段和已发布报告保持；剩余 51 份快照的大小及哈希一致。

本次证据：[部署最终核验](../.artifacts/personal-summary-compaction-20261028/deployment-01/final-verification.json)、[正式任务实跑](../.artifacts/personal-summary-compaction-20261028/deployment-01/scheduled-verification.json)、[健康快照](../.artifacts/personal-summary-compaction-20261028/deployment-01/health-final.json)、[部署前离线验收](../.artifacts/personal-summary-compaction-20261028/final-verification.json)。配置、任务 XML、报告库及扫描库一致备份位于同一部署目录的 `before`，哈希见 [backup-manifest.json](../.artifacts/personal-summary-compaction-20261028/deployment-01/backup-manifest.json)。

上一版本 `.releases/rust-20261027-collection-clean-01` 完整保留。回退仅恢复程序路径、任务动作和配套配置，继续保留当前业务数据库及新增数据，详见[备份与回退](windows-deployment.md#备份与回退)。

## 前一版本记录：每日扫描生效与旧采集残留清理（2026-10-27，Asia/Shanghai）

以下保留 10 月 27 日当时的验收结果；当前正式版本和任务状态以本页上方的 10 月 28 日记录为准。

正式程序于 **18:55:23** 切换至 `.releases/rust-20261027-collection-clean-01`，活动配置为 `config-rust`。主程序、Office/PDF 解析程序及任务脚本属于同一发布包。

| 任务 | 时间与作用 | 实际回读 |
| --- | --- | --- |
| `AI Daily Report` | 每天 09:00 扫描并汇总前一天 | 已启用、Ready；一个日历触发 |
| `AI Daily Report Health` | 每天 11:00、23:00 巡检 | 已启用、Ready |

日报保留 `StartWhenAvailable`，无登录、日内重复或失败自动重启触发。下一次自然日报为 **2026-10-28 09:00，目标日期 2026-10-27**，尚未观察该次自然执行。扫描以文件修改时间筛选，保存读取时的上下文；无法还原已经覆盖或删除的日内版本，具体材料边界见[日报说明](daily-report-operations.md)。

本轮再次清理了旧配置、历史维护脚本和文档中的残留。14 项采集频率、窗口、队列并发、复制预算及归档输入分组配置已删除，当前程序遇到这些字段会明确报错。旧处理缓存访问、变化准备辅助函数和队列日志字段已移除；压缩与去重维护仅核对 Daily、Health。旧采集验收、性能脚本和采集窗口测试已删除，源码有逐文件核验的恢复备份。当前文档统一说明每日扫描，旧操作规程移入有日期的历史记录。

Collect、Parse 两个任务已注销，检查全部与本项目关联的任务后仅发现上述两项。`collect`、`archive drain/retry/activate-schedule/recover`、`daily --source archive` 及 collect/parse 任务类型均被拒绝。旧配置目录 `config-rust/archive-collector`、旧处理目录 `data/daily_cache/archive` 和旧日内包 `.releases/rust-20261027-health-repair-01` 均未重新出现。前轮删除与恢复清单见[退役最终核验](../.artifacts/personal-collection-retirement-20261027/final-verification.json)。处理缓存保持 **10,479,669 字节，约占 1 GiB 的 0.98%**。

18:55 经任务计划程序实际执行新版 Daily、Health。Daily 在 **546 ms** 内复用 10 月 26 日发布，任务退出 0；Health 正常完成，程序与任务退出 1。当前检查为 **0 error、1 warning**，唯一预警 `REPORT_CONTENT_PARTIAL` 来自原 10 月 26 日材料缺口。本轮真实模型调用新增 **0**。

全部 1,660 条模型调用记录、198 份日报、1 份周报、1 份月报及 266 条运行记录保持；五张历史业务表、215 份报告文件、冻结快照和 `data/archive` 长期证据索引哈希保持。报告库与扫描库完整性正常、外键错误为 0。归档模块保留历史查询、内容核验和存储维护；历史字段及提取身份只服务已保存证据，不创建采集缓存、不恢复后台队列。

验收通过：565 项工作区测试、2 项独立程序包测试、14 项旧配置字段拒绝回归、格式检查、严格 Clippy；Windows 运维 26 项、健康脚本 112 项、日扫任务 6 项检查及个人配置一致性检查；历史去重维护 19 项和压缩调度窗口 13 项测试。程序包 467 条文件清单核验通过，活动配置通过 `doctor --strict`。工作区测试常规忽略 3 项，其中两项独立程序包验收已单独执行通过。

本轮证据：[最终核验](../.artifacts/personal-collection-residue-20261027/final-verification.json)、[残留审计与修改清单](../.artifacts/personal-collection-residue-20261027/residue-audit.json)、[注册任务实跑](../.artifacts/personal-collection-residue-20261027/scheduled-verification.json)、[健康快照](../.artifacts/personal-collection-residue-20261027/health-final.json)。源码恢复包为同目录 `source-before.zip`，729 个文件均已核验；运行配置、任务 XML 和快照位于 `deployment-01/before`。该处历史数据库副本已纳入下述无损归档，原路径可按恢复索引还原。

本轮可回退到保留的 `.releases/rust-20261027-collection-retired-02`，步骤见[备份与回退](windows-deployment.md#备份与回退)。更早的退役、部署和恢复记录见[历史运行状态](archive/history/runtime-through-20261027.md)与[旧采集操作归档](archive/history/collection-operations-through-20261027.md)。

2026-10-27 的追加空间清理已将 27 个静态数据库副本无损归档，并删除 46,832 个与保留原件内容一致的测试材料副本。15 份不同内容的数据库均通过解压校验；恢复索引、压缩包、恢复程序和原件清单位于 `.artifacts/project-space-cleanup-20261027/phase2`。具体恢复方法见[恢复说明](../.artifacts/project-space-cleanup-20261027/phase2/RECOVERY.txt)，磁盘收益和完整核验见[清理核验](../.artifacts/project-space-cleanup-20261027/phase2/final-verification.json)。
