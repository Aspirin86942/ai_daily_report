# 日报使用与恢复

本文适用于 Rust 程序包。正式任务和验收见[当前状态](current-state.md)，旧运行说明见[Python 操作归档](archive/python-daily-operations-2026-10-15.md)。所有命令都可添加 `--config-dir <配置目录>`。

## 日报与周月报

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

配置顺序为 `settings.yaml`、`settings.windows.yaml`、`.secrets.yaml`，再应用 `DAILY_REPORT_<SECTION>__<FIELD>` 环境变量。配置目录父目录下的 `.env` 支持单行 `KEY=value`、引号和注释；不展开变量，进程环境优先。未知配置及退休选项明确报错。

`llm.max_request_utf8_bytes` 控制单次请求的 UTF-8 字节上限，当前值为 `160000`。`llm.facts_max_tokens` 单独控制事实提取输出，默认 `16384`，实际值取配置值与已验证模型输出上限中的较小值；`llm.max_tokens` 控制成稿、周报和月报。单次响应的独立字节上限为 128 KiB，事实输出不再挤占其他批次的成稿空间。

事实提取按完整段落或表格行分批，并携带相关标题、表头和单位。初始批次按事实输出上限的两倍 UTF-8 正文字节规划；这是控制规模的估计值，实际请求仍受请求字节数和模型上下文限制。`llm.max_batches` 默认 `32`；全部摘录无法排入时，在调用前明确报错。输出截断时将原批次拆成两份并全部处理，不能只保留来源开头。单个完整单元仍放不下时停止，保留调用证据。

正式归档日报当前配置为 `llm.max_batches=256`、`llm.max_sources_per_batch=64`，模型名称为 `deepseek-v4-flash`。变化片段分别具有来源编号，因此片段较多时需要同时考虑总批次数和每批来源数。每批仅持有自身引用，全日证据保存在完整计划中；原有单次请求与输出大小限制继续有效。当前生效包及验收结果见[当前状态](current-state.md)。

归档模型输入按 `archive.model_ignored_prefixes` 筛选文件名，默认排除 `.tmp-` 开头的临时产物；原件、变化记录继续保留，并在冻结计划的 `archive_excluded_changes` 中登记处理范围。同一版本、同一变化区间内的相邻新增片段默认按最多 16 个单元、8 KiB 合并，分别由 `model_group_max_units` 和 `model_group_max_utf8_bytes` 控制。每个原始单元的原文位置、版本和哈希继续保留；修改前后对照保持成对，解析不完整的片段单独处理。

正式日报从 `recovery.model_workers` 读取模型并发数，允许 1 至 4，缺省为 1。生产使用 4 并发、`recovery.run_budget_seconds=1800`、`schedule.execution_limit_minutes=40`；生成任务定义时要求任务时限至少比程序时限多一分钟。并发和运行时限属于执行参数，不改变已有冻结请求的身份；恢复旧日报沿用原冻结材料，新输入规则用于新准备的日报。

待确认事实直接由已验证的事实正文与引用生成待确认条目，不再请求模型改写。确定事实按实际数量、字节数和输出容量分组，同主题事实相邻；每组事实 JSON 按成稿输出上限的两倍字节规划。Rust 合并全部正文与待确认，概览最多五条，每个事实编号都须在正文或待确认出现。使用指定日期的 `backfill --dry-run` 检查输入分批，零模型调用；事实提取完成后才能确定实际成稿组数。`daily --dry-run` 按来源准备归档或扫描输入，不调用模型。

累计次数、token 和金额预算由 `recovery.enforce_model_budgets` 控制。当前业务选择为关闭，示例配置保持关闭；实际调用与未知用量继续记录。每个请求仍有最多三次尝试、请求容量、超时和整个运行截止时间。模型响应结构或引用无效时，按 `llm.invalid_response_retries`（默认 1）额外重试，与 `max_retries` 共享单步骤总尝试上限；成稿容量不足时自动拆分，单条事实仍放不下则转为待确认并保留引用。显式补跑和覆盖修复的作业预算独立有效。

事实提取成功后被冻结，成稿失败可复用已有事实。拆分后的步骤有独立编号和父请求哈希；恢复时复用已记录的截断结果和成功子批次，只执行缺失部分。请求体与响应原字节保存到 `data_dir/run_snapshots`。未知调用保持未知并计入预算，恢复不会清空消耗。程序不自动更换服务商、模型或请求格式。当前归档日报使用模型计划 v5、报告格式 v4；保留的扫描日报使用模型计划 v4、报告格式 v3，较早未完成的 v3 模型计划由保留的旧包和对应配置恢复。`truncation_policy` 与 `max_strategy_changes` 只用于旧格式恢复。

`rust-20261018-release-03` 起，单份快照上限为 128 MiB，并且不能超过 `retention.snapshot_max_mib` 配置的总容量。当前总容量为 512 MiB。较大的完整归档输入可以保存和恢复，读写仍验证 SHA-256；模型响应与单次请求各自的大小限制不受影响。

## 恢复与补跑

归档日报因步骤重试额度耗尽等原因无法直接续跑时，先运行 `archive recover --run-id <原运行> --reason "授权恢复原因"`。该命令不调用模型：它核验冻结输入、请求与响应哈希，准备新的恢复运行并关联成功事实及成稿响应；旧运行标记为已被替代，原输入、响应和调用账本保留。随后运行 `daily --target-date YYYY-MM-DD --source archive`，正式日报会接续新恢复运行。事实请求沿用冻结计划，成稿仅在计划及请求完全一致时复用；分组或提示词变化时重新准备对应步骤。恢复准备返回的 `run_id`、`reused_fact_steps`、`reused_composition_steps` 和零模型调用应单独留档。

归档成稿使用短事项编号辅助模型分组；真实事项编号和来源编号由程序按已验证的 `fact_ids` 回填。概览和待确认中没有事实编号的附加说明不进入正文。未知事实编号仍然拒绝；混合不同事项或遗漏输入事实时完整拆分，所有原始响应继续保存。

```powershell
ai-daily-report.exe repair-output --target-date 2026-10-11 | Out-Host
ai-daily-report.exe backfill --start-date 2026-10-01 --end-date 2026-10-11 --dry-run | Out-Host
ai-daily-report.exe backfill --start-date 2026-10-01 --end-date 2026-10-11 --preflight <预检文件> --call-budget <已授权作业次数> --workers 2 | Out-Host
```

`repair-output` 根据已提交数据库重放原正文，零扫描、零模型调用。正文被人工修改时保留文件并返回冲突。正常报告命令可继续未完成的冻结运行。

归档日报使用 `daily --source archive`，直接读取已保存的变化和证据。自动恢复只选择相同来源的未完成运行，旧扫描失败不会阻止归档日报。需要恢复旧扫描运行时，保留匹配业务配置并显式执行 `daily --source scan --target-date <日期>`；原运行、冻结输入和调用账本继续保留。归档入口、基线边界及生产切换条件见 [变化归档](change-archive.md)。

历史日报恢复结果与证据见[当前状态](current-state.md)。

## 健康巡检

```powershell
ai-daily-report.exe health --format text | Out-Host
ai-daily-report.exe health --target-date 2026-10-17 --format json | Out-Host
```

`health` 只读检查默认目标日（上一自然日）的采集连续性、日报是否已发布且正文哈希一致、待续运行与近 24 小时失败运行、归档解析队列、数据目录所在磁盘余量、报告库与归档索引完整性。不调用模型、不扫描原目录、不修改数据库。退出码 0 健康、1 有警告、2 不健康；文本或 JSON 输出可直接审计。基线建立前的时段按基线窗口豁免，不判为采集缺口。

健康任务每日 11:00 与 23:00 执行发布包内 `scripts/health_notify.ps1`：调用 `health --scheduled` 写入调度日志，回读 daily、collect、health 三个计划任务，写 `log_dir/health-YYYY-MM-DD.json`；非健康时弹出 Windows 本地通知，并按同日同错误码去重，通知记录写入 `log_dir/health-notify.log`。锁屏或未登录时通知可能不显示，健康文件与日志仍保留。

## 覆盖修复

```powershell
ai-daily-report.exe coverage plan --dates 2026-10-11 --gap-codes MODEL_INPUT_BOUNDED --scope-root <已批准范围> --call-budget 6 --token-budget 200000 --refresh-preflight | Out-Host
ai-daily-report.exe coverage generate --plan <plan.json> --workers 2 | Out-Host
ai-daily-report.exe coverage publish --candidates <候选清单> | Out-Host
ai-daily-report.exe coverage recover --plan <plan.json> --reason "继续失败成稿" --authorization-note "本次授权说明" --calls-limit 9 | Out-Host
ai-daily-report.exe coverage amend --plan <plan.json> --reason "全部失败后修订参数" | Out-Host
```

准备、生成候选和发布为三个明确动作。候选正文与差异材料保存在隔离作业目录；发布前重新核对计划、原文件、原日报及候选哈希。恢复保留同一作业身份、成功请求和旧消耗；新增预算通过追加记录表达。未知调用阻止部分成功恢复。`--revise-runtime` 仅用于明确确认同一配置下运行实现身份的变化，不自动改写输入。

## OCR 与保留策略

`ocr validate --manifest <清单>` 核对原 PDF 身份、页数、页码、文字文件及哈希。`daily --ocr-manifest` 和 `backfill --ocr-manifest` 使用同一验证逻辑。OCR 文字保持“未核实”，不能直接成为确定事实；已有原生摘录不被 OCR 覆盖。

`ocr run --engine <获准引擎的绝对路径> --file <工作目录内相对路径> --pages 1,2 --output-dir <隔离目录>` 显式运行本地引擎。引擎通过标准输入接收有界 JSON，返回原文件哈希和逐页文字 JSON；文件、页数、内存、输出和运行时间都有上限。PDF 解析失败不会自动启动 OCR。

`cleanup` 默认只列出到期项目；`cleanup --execute` 才执行清理。未完成运行、跨代复用的响应、用户修改和未知文件保留。清理逐文件核对归属与哈希，不递归删除目录，不删除正式报告正文。
