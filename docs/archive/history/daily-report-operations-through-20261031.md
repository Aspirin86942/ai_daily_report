# 日报使用与恢复

本文适用于 Rust 程序包。正式任务和验收见[当前状态](current-state.md)，旧运行说明见[Python 操作归档](archive/python-daily-operations-2026-10-15.md)。所有命令都可添加 `--config-dir <配置目录>`。

## 日报与周月报

当前日常模式为 `report.profile: personal`：每天上海时间 09:00 扫描一次昨天 00:00–24:00 修改的文件，解析后生成简短日报和独立依据附件。扫描采用一个解析进程，最多 120 秒，文件输入最多 18,000 字，为人工补充保留 2,000 字；整体仍为 300 秒。未变文件复用解析缓存，预算或读取限制继续显示。

扫描保留本次读取时的上下文，按完整语句、表格行等既有语义边界选材。文件修改时间只用于日期筛选，不能证明全部内容为当天新增；中间版本、已删除文件及次日扫描前再次改写的文件可能缺失。无材料时明确说明无法据此判断当天没有工作。

Daily 只有 09:00 一个日历触发，错过时由任务计划程序的 `StartWhenAvailable` 补执行；取消 09:30、10:30、登录和失败自动重启。Collect、Parse 任务及执行入口已删除，Health 保留 11:00、23:00 的轻量检查。已有发布直接复用，已冻结的未完成扫描恢复原输入，不重新扫描。扫描模式不自动转去补历史归档日报。

历史报告保留原正文、附件与审计记录。`archive` 仅保留历史查询、核验及存储维护；新日报统一扫描输入。

```powershell
ai-daily-report.exe daily --target-date 2026-10-11 --dry-run | Out-Host
ai-daily-report.exe daily --target-date 2026-10-11 --no-save | Out-Host
ai-daily-report.exe daily --target-date 2026-10-11 | Out-Host
ai-daily-report.exe daily --target-date 2026-10-11 --regenerate --reason "补充已确认材料" | Out-Host
ai-daily-report.exe weekly 2026-W41 --source db | Out-Host
ai-daily-report.exe monthly 2026-10 --source scan | Out-Host
ai-daily-report.exe runs --target-date 2026-10-11 | Out-Host
ai-daily-report.exe list | Out-Host
```

默认目标日是上海时区的昨日，指定日期必须已经结束。`--date` 保留为弃用别名。预检零调用；预览生成正文和完整审计，正式报告指针不变。重复执行已发布日报会复用已有版本，显式重新生成必须说明原因。

周月报的 `db` 来源只汇总已有日报；`scan` 来源直接扫描对应日期范围。历史材料的修改时间只用于范围选择，不能证明工作完成或方案生效。

## 报告结构

`report.profile: personal` 使用报告 v5、冻结输入 v5 和模型计划 v6。新日报提炼 3～5 个重点，事项标题与文字合计以 420～560 个 Unicode 字符为目标、硬上限 1,500 字，材料不足或无变化时允许更短；报告总标题、链接、材料范围说明和附件不计入事项正文字符数。用户陈述、观察、待确认和程序建议有明确标签，同一事项的状态与下一步集中说明。

新请求以 420～560 字为目标，为 1,500 字硬上限预留空间。超过 1,500 字时，先校验所有来源，再将原稿和实测字数交给独立审计的精简步骤；精简须保留事项数量、顺序、类型、确定性和来源引用，尤其保留下一步计划，优先缩短标题、重复背景和冗长表述。初稿未超过 1,500 字时直接发布，不进入精简步骤、不记录内容缺口，也不触发历史恢复告警。精简仍超长或改变事项结构时，在现有重试次数与运行期限内重试；只有精简失败后才按来源优先级省略完整的非计划事项，并记录失败与 `PERSONAL_OUTPUT_REDUCED`；金额、条件和待确认状态不在句中截断。下一步计划本身无法完整容纳时保留异常，不删除计划来凑字数。超过 5 项的响应按格式无效重试。新提示词版本写入模型计划，旧冻结请求继续使用原提示词、指纹与响应处理规则（历史计划仍按 5 项、600 字精简）。

正文保留关键数字、条件和待确认状态，每项通过“查看依据”链接跳到附件中的完整来源列表。“完整依据”链接打开同月 `attachments/YYYY-MM-DD.<内容指纹>.md`，保留用户原文、来源摘录、前后值、版本、位置、观察时间和指纹；相同缺口说明合并计数，具体定位在附件中保留。正文与附件合计最多 100,000 字节。

正文和附件模板一起冻结，最终附件字节及哈希随报告保存到数据库。程序先原子保存附件，再发布正文；重新生成使用新附件名称并保留旧附件。附件缺失可以恢复，已被人工修改的附件保留并报告冲突，健康检查同时核对附件。处理缓存过期不影响已发布内容的修复。历史报告没有附件字段时继续使用原有内嵌依据，读取时保留原来的 10 项、1,500 字校验范围。`--no-save` 预览在同一段输出后附上完整依据，链接在预览内部跳转，不创建正式文件。

人工补充放在 `report.notes_dir/YYYY-MM-DD.md`，也可使用 `daily --target-date YYYY-MM-DD -i "补充内容"`。两处内容正规化换行并按完整重复段落合并，合计最多 2,000 字；超限返回 `USER_INPUT_TOO_LARGE`，原文件保持。用户原文随报告保存。

预览、普通重跑、恢复和重新生成均不受累计模型额度限制。每次发送前事务记录请求，失败、超时、未知结果和进程中断均保留审计。个人模型通信失败后本轮停止发起模型请求；已有有效超长原稿可按上述规则保底，后续触发可继续恢复。格式或引用无效时，在单步骤尝试上限和运行期限内重新生成完整响应。新请求的正文与完整依据超过文件总大小限制时，同样优先保留下一步计划，按完整事项及其依据减量并记录省略。失败结果保留用户原文及材料范围；已有有效日报在恢复失败时保持，历史异常由健康检查持续跟踪。

`data_dir/daily_cache` 保存冻结输入、模型请求与响应，容量预留计入 1 GiB 上限，快照最长保留 7 天。到期清理只处理登记过的日报快照。历史正文、附件、长期归档与模型调用计数继续保留。

`repair-output` 只需报告库中的最终内容即可修复 v5。未发布输入超过 7 天后不再恢复该输入，返回 `PERSONAL_INPUT_EXPIRED`；重新选材使用仍然保存的材料，无须追加调用额度。历史报告直接使用 `repair-output` 修复正文和附件。

以下六节结构、分批事实生成和旧归档恢复说明适用于 `evidence` 与历史报告。

日报正文分为六节：要素速览、内容概览、事项目录（仅事项达到 10 个时出现）、事项详情、待确认、来源与覆盖附录；归档来源（模型计划 v5、报告格式 v4）在附录之后另有“变化依据”。

- 要素速览是一行引用块，给出材料总数、读取成功/失败数、事实总数与确定/待确认数，以及缺口类别与条目数；`no_changes` 不输出该行。
- 事项详情按审批权限、金额异常、完成状态等优先级排序，每个事项有 `m1`、`m2` 等锚点；超过 240 字的背景或详情在句末标点后拆成最多三段，引用仍只出现在该事项的最后一段。
- 来源与覆盖附录中的缺口按错误码聚合，同一错误码只占一行并列出涉及来源；“变化依据”逐条保留事项标识、版本、解析版本、位置、原文与引用 SHA-256。
- 以上排版调整只改变呈现方式，不删减来源标签、引用、SHA-256、缺口原因或质量标记；历史日报正文存储在报告中，不因模板调整重新渲染。

## 历史检索

```powershell
ai-daily-report.exe search --query 审批 --from 2026-10-01 --to 2026-10-18 --kind daily --limit 20 | Out-Host
ai-daily-report.exe search --query 供应商 --source S001 --format json | Out-Host
ai-daily-report.exe search --reindex | Out-Host
```

`search` 只读报告库 `reports.sqlite3`，在独立索引库 `report_search.sqlite3` 上检索日报、周报和月报正文，命中到具体事项片段。关键词不少于 3 个字符时使用 FTS5 trigram 短语匹配，更短的词回退为包含匹配；`--from`/`--to` 只作用于日报（周月报没有自然日标签，带日期过滤时自动排除），`--source` 接受 `S001` 形式的来源标签，`--limit` 默认 20。索引按源库行数与最大更新时间自动重建，`--reindex` 强制重建并输出摘要；索引库可随时删除，下次检索自动重建。该命令不调用模型，报告库字节不变；参数用法错误沿用 CLI 的退出码 2，运行错误（数据库缺失、索引失败、空查询）退出码 1。

## 调用与配置

配置顺序为 `settings.yaml`、`settings.windows.yaml`、`.secrets.yaml`，再应用 `DAILY_REPORT_<SECTION>__<FIELD>` 环境变量。配置目录父目录下的 `.env` 支持单行 `KEY=value`、引号和注释；不展开变量，进程环境优先。未知配置及退休选项明确报错；已删除的模型额度配置字段作为兼容例外被忽略，不参与执行或新快照。

`llm.max_request_utf8_bytes` 控制单次请求的 UTF-8 字节上限，当前值为 `160000`。`llm.facts_max_tokens` 单独控制事实提取输出，默认 `16384`，实际值取配置值与已验证模型输出上限中的较小值；`llm.max_tokens` 控制成稿、周报和月报。单次响应的独立字节上限为 128 KiB，事实输出不再挤占其他批次的成稿空间。

事实提取按完整段落或表格行分批，并携带相关标题、表头和单位。初始批次按事实输出上限的两倍 UTF-8 正文字节规划；这是控制规模的估计值，实际请求仍受请求字节数和模型上下文限制。`llm.max_batches` 默认 `32`；全部摘录无法排入时，在调用前明确报错。输出截断时将原批次拆成两份并全部处理，不能只保留来源开头。单个完整单元仍放不下时停止，保留调用证据。

`archive.model_ignored_prefixes` 筛选允许发给模型的文件名，默认排除 `.tmp-` 开头的临时产物。历史原件、变化记录和冻结输入中的原文位置、版本与哈希保持。旧归档输入分组配置已删除，新日报按扫描输入预算选材。

个人日报整体预算 300 秒，单次扫描最多 120 秒、一个解析进程。`recovery.model_workers` 的分批并发配置用于 evidence 格式的报告恢复；历史冻结请求继续沿用原材料与请求身份。

待确认事实直接由已验证的事实正文与引用生成待确认条目，不再请求模型改写。确定事实按实际数量、字节数和输出容量分组，同主题事实相邻；每组事实 JSON 按成稿输出上限的两倍字节规划。Rust 合并全部正文与待确认，概览最多五条，每个事实编号都须在正文或待确认出现。使用指定日期的 `backfill --dry-run` 检查输入分批，零模型调用；事实提取完成后才能确定实际成稿组数。`daily --dry-run` 准备扫描输入，不调用模型。

累计次数、token 和金额额度模块已删除，包括个人日期额度、运行和自然日额度、补报及覆盖修复作业额度。旧 `recovery.enforce_model_budgets`、`max_calls_per_*`、`max_tokens_per_*`、`max_cost_per_*_microunits` 与 `model_pricing` 配置可读取但不再生效；实际调用与未知用量继续记录。每个请求仍有最多三次尝试、请求容量、超时和整个运行截止时间。模型响应结构或引用无效时，按 `llm.invalid_response_retries`（默认 1）额外重试，与 `max_retries` 共享单步骤总尝试上限；成稿容量不足时自动拆分，单条事实仍放不下则转为待确认并保留引用。补报和覆盖修复直接按已核验计划运行，无须填写预算或追加次数；已完成作业仍不能重复发送请求。

事实提取成功后被冻结，成稿失败可复用已有事实。拆分后的步骤有独立编号和父请求哈希；恢复时复用已记录的截断结果和成功子批次，只执行缺失部分。请求体与响应原字节保存到 `data_dir/run_snapshots`。未知调用保持未知，恢复保留原调用记录与累计计数。程序不自动更换服务商、模型或请求格式。历史归档日报使用模型计划 v5、报告格式 v4；历史 evidence 扫描日报使用模型计划 v4、报告格式 v3，较早未完成的 v3 模型计划由保留的旧包和对应配置恢复。`truncation_policy` 与 `max_strategy_changes` 只用于旧格式恢复。

`evidence` 报告单份快照上限为 128 MiB，总容量由所选配置的 `retention.snapshot_max_mib` 控制。读写验证 SHA-256；模型响应与单次请求仍有各自的大小限制。个人日报的处理缓存使用前文所述的 1 GiB 上限。

## 恢复与补跑

每日扫描中断后，重跑同一目标日会恢复冻结输入。已发布报告重复执行直接复用；需要重新扫描时使用 `daily --target-date YYYY-MM-DD --regenerate --reason "原因"`。旧归档冻结运行需使用退役恢复包中的匹配旧程序和配置，当前程序不再启动采集或归档出报。

```powershell
ai-daily-report.exe repair-output --target-date 2026-10-11 | Out-Host
ai-daily-report.exe backfill --start-date 2026-10-01 --end-date 2026-10-11 --dry-run | Out-Host
ai-daily-report.exe backfill --start-date 2026-10-01 --end-date 2026-10-11 --preflight <预检文件> --workers 2 | Out-Host
```

`repair-output` 根据已提交数据库重放原正文，零扫描、零模型调用。正文被人工修改时保留文件并返回冲突。正常报告命令可继续未完成的冻结运行。

当前运行与验收见[当前状态](current-state.md)，更早的日报恢复结果与证据见[历史运行记录](archive/history/runtime-through-20261027.md)。

## 健康巡检

```powershell
ai-daily-report.exe health --format text | Out-Host
ai-daily-report.exe health --target-date 2026-10-17 --format json | Out-Host
```

`health` 只读检查扫描索引、日报及附件哈希、待续和历史异常、磁盘余量与报告库完整性，不调用模型或扫描原目录。退出码 0 健康、1 预警、2 不健康。历史报告已有缺口继续显示。

健康任务每日 11:00 与 23:00 执行发布包内 `scripts/health_notify.ps1`。Rust 的 `health --scheduled` 先原子写入当天 UTF-8 健康文件及调度日志；`--output <路径>` 可另外输出 JSON 文件。通知脚本显式读取 UTF-8 文件，回读 daily、health 两个任务后补充任务状态，不解析本地代码页转换的 stdout。非健康时弹出 Windows 本地通知，按同日同错误码去重；通知失败另存 `health-notify-error.json`，健康检查结果仍保留。已被同 scope、目标日更晚正式发布恢复的失败归入 `resolved_failures`，原失败账本保持，未恢复失败继续告警。

## 覆盖修复

```powershell
ai-daily-report.exe coverage plan --dates 2026-10-11 --gap-codes MODEL_INPUT_BOUNDED --scope-root <已批准范围> --refresh-preflight | Out-Host
ai-daily-report.exe coverage generate --plan <plan.json> --workers 2 | Out-Host
ai-daily-report.exe coverage publish --candidates <候选清单> | Out-Host
ai-daily-report.exe coverage recover --plan <plan.json> --reason "继续失败成稿" --authorization-note "本次授权说明" | Out-Host
ai-daily-report.exe coverage amend --plan <plan.json> --reason "全部失败后修订参数" | Out-Host
```

准备、生成候选和发布为三个明确动作。候选正文与差异材料保存在隔离作业目录；发布前重新核对计划、原文件、原日报及候选哈希。恢复保留同一作业身份、成功请求和历史调用；每次根据当前调用清单生成可核验的恢复记录。未知调用阻止部分成功恢复。`--revise-runtime` 仅用于明确确认同一配置下运行实现身份的变化，不自动改写输入。

## OCR 与保留策略

`ocr validate --manifest <清单>` 核对原 PDF 身份、页数、页码、文字文件及哈希。`daily --ocr-manifest` 和 `backfill --ocr-manifest` 使用同一验证逻辑。OCR 文字保持“未核实”，不能直接成为确定事实；已有原生摘录不被 OCR 覆盖。

`ocr run --engine <获准引擎的绝对路径> --file <工作目录内相对路径> --pages 1,2 --output-dir <隔离目录>` 显式运行本地引擎。引擎通过标准输入接收有界 JSON，返回原文件哈希和逐页文字 JSON；文件、页数、内存、输出和运行时间都有上限。PDF 解析失败不会自动启动 OCR。

`cleanup` 默认只列出到期项目；`cleanup --execute` 才执行清理。未完成运行、跨代复用的响应、用户修改和未知文件保留。清理逐文件核对归属与哈希，不递归删除目录，不删除正式报告正文。
