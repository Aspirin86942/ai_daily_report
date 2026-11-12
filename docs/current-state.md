# 当前运行与验收状态

## 十倍材料容量已正式生效（2026-11-12，Asia/Shanghai）

正式程序于 **11:32:27** 切换至 `.releases/rust-20261112-material-range-10x-01`，活动配置仍为 `config-rust`。Daily、Health 两项任务及配套解析程序已回读确认；触发时间、补跑设置、权限和时限保持。文件材料上限提高到 **180,000 字符**，人工补充保留 2,000 字符，总输入上限为 182,000 字符。

配套配置为单文件摘录 60,000 字符、Excel 每表 500 条行记录、PDF 50 页、解析上下文 5,000,000 字符、模型请求配置上限 1,600,000 字节、正文及依据附件合计 1,000,000 字节。实际扫描记录已核对这些数值。Excel 工作表和列数、摘要目标、扫描 120 秒及应用 300 秒时限保持；请求仍受已确认的模型上下文容量约束。

真实模型接口检查和 11 月 11 日日报预览均成功，新增 **2 次真实模型调用，全部成功**。预览观察 51 份文件、选入 **50 份材料、179,932 字符**，生成 **5 项、585 字**，全程 **24.964 秒**，其中模型调用 19.470 秒。预览单独留档，原有 **259 份报告与附件全部保持原哈希**；当前发布及历史报告数量保持。

**11:38:49** 经任务计划程序实际启动新版 Daily，**683 ms** 内复用既有日报，退出 0、模型调用 0。即时健康检查为 **0 项错误、2 项预警**，仍为 11 月 3 日历史解析缺口和当前发布报告的材料范围限制。下一次自然日报为 **2026-11-13 09:00**，汇总 11 月 12 日。

验证通过：505 项 Rust 工作区测试、3 项独立程序包验收、28 项 Windows 运维检查、格式与严格 Clippy，程序包 465 项文件核验通过。旧冻结输入的限额兼容、已有响应复用、大附件发布与恢复均有回归验证。同批隔离样本保持 51 份有效材料，材料由 17,986 增至 179,981 字符。证据见[最终核验](../.artifacts/material-range-10x-20261112/final-verification.json)。

配置、两项任务 XML、报告库与扫描库一致备份位于 `.artifacts/material-range-10x-20261112/deployment/before`，哈希见同级 `backup-manifest.json`。恢复旧提取限额的配置位于 `.artifacts/material-range-10x-20261112/rollback-config`，继续使用新版读取较大的依据附件；上一正式程序包完整保留。回退保留新增报告、预览及调用账本。

## 健康检查到期判断已修复，11 月 9 日日报已恢复（2026-11-11，Asia/Shanghai）

正式程序于 **09:17:31** 切换至 `.releases/rust-20261111-health-due-01`，活动配置仍为 `config-rust`。Daily、Health 两项任务及配套解析程序已回读确认；触发时间、补跑设置、权限和时限保持。模型连接继续使用本机 Magpie 的 `openai/gpt-6.1-terra`。

健康检查未指定日期时与日报调度共用目标日判断：09:00 前检查前天，09:00 起检查昨天。尚未发布的报告在计划时间加 300 秒运行预算内显示尚未到期；延迟补跑的实际运行仍在预算内时显示正在生成。预算到期后仍未发布继续报错，已发布文件校验和历史异常检查始终保留。正常运行不再立即计入待续运行告警。

11 月 9 日日报已重新扫描当前可见材料并发布：选入 **27 份材料、17,996 个字符，生成 3 项总结**。本次新增真实模型调用 **1 次，成功**；正文、依据附件、原报告和旧失败记录均已核验。原有 255 份报告文件与附件中，仅目标日报正文按授权替换，其余原文件哈希保持。

正式健康脚本以 `-NoNotify` 实跑成功，当前为 **0 项错误、2 项预警**：11 月 3 日的历史解析缺口及当前报告的材料提取范围限制。日报缺少有效总结的错误已解除。提前检查、09:00 生成窗口、延迟补跑、预算到期和历史失败保留均有边界测试；下一次自然早间补跑尚未发生。

验证通过：498 项 Rust 工作区测试、3 项独立程序包验收、28 项 Windows 任务检查、112 项健康通知检查、格式与严格 Clippy；发布包 465 项文件核验通过。证据见[最终核验](../.artifacts/health-due-20261111/final-verification.json)。配置、任务 XML、报告库和扫描库一致备份位于 `.artifacts/health-due-20261111/deployment/before`，哈希见同级 `backup-manifest.json`；上一正式包完整保留。回退只恢复程序路径及任务动作，保留新增报告和调用账本。

## 公司网关与 Magpie 双连接已生效（2026-11-08，Asia/Shanghai）

当前正式包为 `.releases/rust-20261108-model-services-02`，00:51:28 完成任务与解析程序路径切换，活动配置仍为 `config-rust`。默认连接为 `openai`：通过本机 Magpie 的 `http://127.0.0.1:3425/v1` 调用 `openai/gpt-6.1-terra`。`company` 保留原公司网关 `https://ai-gateway.fsdev.cn` 与 `deepseek-v4-flash`。两种连接已完成实机双向切换，最终回到 Magpie，切换本身零模型调用。

在项目目录执行 `scripts/set_model_service.ps1 -Service openai -Apply` 或 `-Service company -Apply`，会同步改变手工命令与定时任务读取的模型连接。连接参数保存于 `config-rust/model-services.json`，生效覆盖文件为 `config-rust/settings.windows.yaml`；私密凭据保存在本地 `.secrets.yaml`，这些文件均受 Git 忽略规则保护。Magpie 模式需要本机服务持续运行，公司模式仍需要公司网络或 VPN。完整用法见[连接选择说明](daily-report-operations.md#选择公司网关或-openai)。

新增 OpenAI `reasoning_effort` 参数支持；当前设置为 `none`，请求使用 `max_completion_tokens`。公司网关的请求参数、旧配置指纹和冻结请求恢复行为保持兼容。切换器在应用前验证凭据和模型验收记录，原子更新覆盖文件，并在回读失败时恢复原配置。

本次新增 **3 次真实模型调用，全部成功**：1 次虚构材料接口检查，以及 11 月 5、6 日日报恢复。两份恢复均重新扫描当前可见材料，并保留旧正文、附件、运行记录及调用账本。

| 目标日期 | 恢复结果 | 文件材料 | 模型调用 |
| --- | --- | --- | --- |
| 2026-11-05 | 4 项、546 字，正文与附件哈希匹配 | 23 份，17,960 字符 | 1 次 |
| 2026-11-06 | 4 项、525 字，正文与附件哈希匹配 | 5 份，17,886 字符 | 1 次 |

两份新报告均无模型失败缺口，`quality=partial` 仅保留 `EXCERPT_BOUNDED`、`PARSE_BOUNDED`。对最新已到期目标 11 月 6 日的健康检查为 **0 项错误、2 项预警**，分别来自材料范围与 11 月 3 日的既有历史解析缺口。Daily、Health 回读均符合计划，09:00 日报及 11:00/23:00 健康触发、补跑设置和权限保持。

验证通过：492 项 Rust 工作区测试、28 项 Windows 任务检查、3 项独立程序包验收、格式与严格 Clippy，最终发布包 465 项文件核验通过。完整记录见[最终核验](../.artifacts/personal-model-services-20261108/final-verification.json)。原配置、任务 XML、数据库与冻结材料备份位于同目录 `deployment-01/before`；最终脚本切换前的配置及任务备份位于 `deployment-02/before`。回退程序时应一并恢复相应的模型覆盖配置，业务数据和新增调用账本继续保留。

## 健康任务补跑已修复，日报恢复等待公司网络（2026-11-07，Asia/Shanghai）

正式程序于 **18:36:38** 切换至 `.releases/rust-20261107-health-catchup-01`，活动配置保持 `config-rust`。健康任务定义已补充 `StartWhenAvailable=true`，错过 11:00 或 23:00 后可在任务可运行时补执行；源码、正式任务和任务回读保持一致。两个任务的执行动作及配置中的两个解析程序路径已同步更新。触发时间、执行身份、权限、其他设置逐项保持。

| 项目 | 实际验证 | 下一次计划执行 |
| --- | --- | --- |
| `AI Daily Report` | 18:37:26 经任务计划程序启动完成，退出 0，复用既有报告，模型调用 0 | 2026-11-08 09:00 |
| `AI Daily Report Health` | 18:37:29 经任务计划程序启动完成，`execution_status=succeeded`；程序退出 2、任务退出 1，符合未恢复日报的告警状态 | 2026-11-07 23:00 |

本次通过 490 项 Rust 工作区测试、28 项 Windows 任务检查、2 项独立程序包验收、格式与严格 Clippy，463 项发布文件核验通过。正式配置通过 `doctor --strict`，数据库、9 份模板和两个解析程序通信正常。补跑设置已经回读，尚未经历下一次自然漏跑后的补执行。

11 月 5、6 日扫描预检分别取得 18、5 份材料，未再出现扫描未完成或解析程序通信失败。模型网关 `ai-gateway.fsdev.cn` 当前解析至公司内网 `10.1.250.37`，直连和现有代理均超时，因此尚未执行重新生成；两份原报告继续保留。11 月 3 日历史解析缺口也仍在原告警中。连接公司网络后可使用 `daily --target-date YYYY-MM-DD --regenerate --reason "..."` 恢复，原始运行和调用记录继续保留。

切换前配置、任务 XML、报告库与扫描库一致备份、处理缓存和历史冻结文件位于 `.artifacts/personal-recovery-20261107/deployment-01/before`，哈希见同级 `backup-manifest.json`；上一正式包 `.releases/rust-20261104-xls-formula-01` 保留。回退只恢复配置及任务 XML，保留当前业务数据和调用账本。验证证据见[本地验收](../.artifacts/personal-recovery-20261107/verification.json)、[任务实跑](../.artifacts/personal-recovery-20261107/deployment-01/scheduled-acceptance.json)和[最终核验](../.artifacts/personal-recovery-20261107/deployment-01/final-verification.json)。

## XLS 中文公式修复已正式切换（2026-11-04，Asia/Shanghai）

正式程序于 **17:47:09** 切换至 `.releases/rust-20261104-xls-formula-01`，活动配置保持 `config-rust`。本次将 calamine 从 0.36.1 固定升级到 0.37.0，修复 XLS 公式中 UTF-16 字符串长度计算错误导致解析进程退出的问题。两个解析程序路径及 Daily、Health 任务的执行动作和工作目录已更新；触发器、权限、启用状态和超时逐项保持。

修复包通过 490 项 Rust 工作区测试、2 项独立程序包验收、格式与严格 Clippy，463 项发布文件核验通过。正式配置通过 `doctor --strict`，9 份模板、报告库、扫描库及配套解析程序握手正常。

| 项目 | 本次实际验证 | 下一次计划执行 |
| --- | --- | --- |
| `AI Daily Report` | 17:48:11 经任务计划程序手动启动，退出 0；693 ms 内复用 11 月 3 日日报，模型调用 0 | 2026-11-05 09:00 |
| `AI Daily Report Health` | 新包健康脚本以 `-NoNotify` 手动执行成功，0 项错误、2 项既有材料预警；任务回读符合计划 | 2026-11-04 23:00 |
| 问题 XLS 原文件 | 已安装的 Office 解析程序退出 0，按现有预算提取 4,492 个字符、保留 63 条来源行范围；原文件哈希保持 | — |

11 月 3 日日报继续保留原生成结果，`HISTORICAL_REPORT_ATTENTION` 和 `REPORT_CONTENT_PARTIAL` 仍关联该报告的既有解析及覆盖缺口。本次切换新增真实模型调用 **0**；下一次自然生成尚未发生。

233 份报告及附件、33 份处理缓存文件、4,244 份历史冻结文件、11,071 份归档文件逐文件哈希保持。扫描库全部业务表保持；报告、运行和模型调用数量保持，现有到期清理规则仅清空 11 条运行和 10 条调用的过期文件引用，原记录已保存在切换前数据库备份中。

当前回退包 `.releases/rust-20261101-personal-only-01` 完整保留。配置、两项任务 XML、报告库与扫描库一致备份和冻结文件位于 `.artifacts/xls-parser-fix-20261104/deployment-01/before`，哈希见同级 [backup-manifest.json](../.artifacts/xls-parser-fix-20261104/deployment-01/backup-manifest.json)。回退仅恢复配置和程序路径，当前业务数据继续保留。完整证据见[部署最终核验](../.artifacts/xls-parser-fix-20261104/deployment-01/final-verification.json)和[修复验证记录](../.artifacts/xls-parser-fix-20261104/verification.json)。

## 前期空间清理记录（2026-11-03）

2026-11-03 空间清理更新：按用户明确授权，已删除部署前回滚副本、历史数据库恢复压缩包、退役采集恢复包及 57 个旧发布目录。当前运行包 `rust-20261101-personal-only-01` 保持；另保留 `rust-20261029-summary-1500-01`，用于仍需旧程序提供的历史档案维护。正式数据、当前配置和验收记录保持。下文所述已删除备份的位置仅代表当时的历史状态，相关回滚副本已无法用于恢复。

## 个人日报收敛版已正式切换（2026-11-01，Asia/Shanghai）

源码已收敛为个人日报工具，周月报生成、旧通用日报、批量补跑、覆盖修复及 OCR 命令退役。历史检索、归档查询核验与备份、已保存正文和附件恢复继续可用；新增 `repair-output --run-id` 恢复旧日报及周月报。

本地通过 488 项工作区测试、两项独立程序包验收、格式与严格 Clippy、26 项任务契约、6 项日扫脚本和 112 项健康通知检查。十组固定语料对照的首次解析中位数由 477.08 ms 变为 460.01 ms，缓存复用由 69.24 ms 变为 70.12 ms，峰值工作集基本持平，均在 5% 回退门槛内。

正式程序于 **00:55:52** 切换至 `.releases/rust-20261101-personal-only-01`，活动配置保持 `config-rust`。主程序、Office/PDF 解析程序和健康脚本均来自已验收的 `personal-ablation-20261101-final` 包。仅更新两项任务的执行动作与工作目录、配置中的两个解析程序路径；触发器、权限、启用状态和超时逐项保持。

| 任务 | 本次实际执行 | 下次计划执行 |
| --- | --- | --- |
| `AI Daily Report` | 00:56:13 启动，退出 0；663 ms 内复用 10 月 30 日日报，模型调用 0 | 2026-11-01 09:00，目标日期 10 月 31 日 |
| `AI Daily Report Health` | 00:56:33 启动，执行成功；程序退出 2、任务退出 1，原因见下文 | 2026-11-01 11:00，之后每天 11:00、23:00 |

凌晨手动触发时，日报按最近已到期的 09:00 日程选择 10 月 30 日；健康检查默认选择昨天 10 月 31 日，因此记录 `REPORT_NOT_PUBLISHED`。该日日报尚未到今天 09:00 的生成时间。健康程序实际退出码和预期均为 2，脚本执行状态为 `succeeded`，通知关注状态使任务返回 1。另以 `health --target-date 2026-10-30` 验证已到期报告，结果为 **0 项错误、1 项既有材料覆盖预警** `REPORT_CONTENT_PARTIAL`。新包下一次自然生成尚未发生。

包内 460 条文件清单核验通过，正式配置通过 `doctor --strict`（9 份模板、两个解析程序握手、报告库和扫描库）。切换复用既有模型自检记录，本次新增真实模型调用 **0**，调用记录保持 1,668 条，运行记录保持 272 条，202 份日报、1 份周报和 1 份月报保持。

225 份报告及附件、4,244 份历史冻结文件、11,071 份归档文件逐文件哈希保持；扫描数据库全部业务表保持。现有 7 天处理缓存规则清理 3 份到期文件，并释放 4 条已结束运行和 7 条调用的过期文件引用；运行内容、调用状态与用量保持，过期文件已保存在切换前备份中。

切换前配置、两项任务 XML、报告库与扫描库一致备份、冻结文件及逐文件哈希位于 `.artifacts/personal-ablation-20261031/deployment-01/before` 和同级 [backup-manifest.json](../.artifacts/personal-ablation-20261031/deployment-01/backup-manifest.json)。上一正式包 `.releases/rust-20261029-summary-1500-01` 完整保留。回退只恢复程序路径、任务动作与配置，当前数据库及新增业务数据继续保留，详见[备份与回退](windows-deployment.md#备份与回退)。

本次证据：[部署最终核验](../.artifacts/personal-ablation-20261031/deployment-01/final-verification.json)、[已到期报告健康检查](../.artifacts/personal-ablation-20261031/deployment-01/health-published-target.json)、[消融结果](plans/2026-10-31-personal-ablation-results.md)和[交付记录](../.artifacts/personal-ablation-20261031/delivery.json)。原始基线、逐项补丁、恢复源码与已验收候选包一并保留。


## 前一版本记录：日报正文字数硬上限提升至 1,500 字并恢复 10 月 28 日日报（2026-10-29，Asia/Shanghai）

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
