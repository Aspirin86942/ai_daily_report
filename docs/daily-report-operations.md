# 日报使用与恢复

本文适用于 Rust 程序包。当前正式任务切换状态见 [执行计划](plans/2026-10-14-rust-execution-plan.md)；旧运行说明保存在 [Python 操作归档](archive/python-daily-operations-2026-10-15.md)。所有命令都可添加 `--config-dir <配置目录>`。

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

## 调用与配置

配置顺序为 `settings.yaml`、`settings.windows.yaml`、`.secrets.yaml`，再应用 `DAILY_REPORT_<SECTION>__<FIELD>` 环境变量。配置目录父目录下的 `.env` 支持单行 `KEY=value`、引号和注释；不展开变量，进程环境优先。未知配置及退休选项明确报错。

`llm.max_request_utf8_bytes` 控制单次请求的 UTF-8 字节上限，当前值为 `160000`。`llm.facts_max_tokens` 单独控制事实提取输出，默认 `16384`，实际值取配置值与已验证模型输出上限中的较小值；`llm.max_tokens` 控制成稿、周报和月报。单次响应的独立字节上限为 128 KiB，事实输出不再挤占其他批次的成稿空间。

事实提取按完整段落或表格行分批，并携带相关标题、表头和单位。初始批次按事实输出上限的两倍 UTF-8 正文字节规划；这是控制规模的估计值，实际请求仍受请求字节数和模型上下文限制。`llm.max_batches` 默认 `32`；全部摘录无法排入时，在调用前明确报错。输出截断时将原批次拆成两份并全部处理，不能只保留来源开头。单个完整单元仍放不下时停止，保留调用证据。

成稿按实际事实数量、字节数和输出容量分组；大批量分组时将确定事实和待确认分别安排，同主题事实相邻。每组事实 JSON 按成稿输出上限的两倍字节规划，Rust 合并全部正文与待确认，概览最多五条。每个事实编号都须在正文或待确认出现。使用指定日期的 `backfill --dry-run` 检查输入分批，零模型调用；事实提取完成后才能确定实际成稿组数。`daily --dry-run` 只检查扫描。

累计次数、token 和金额预算由 `recovery.enforce_model_budgets` 控制。当前业务选择为关闭，示例配置保持关闭；实际调用与未知用量继续记录。每个请求仍有最多三次尝试、请求容量、超时和整个运行截止时间。显式补跑和覆盖修复的作业预算独立有效。

事实提取成功后被冻结，成稿失败可复用已有事实。拆分后的步骤有独立编号和父请求哈希；恢复时复用已记录的截断结果和成功子批次，只执行缺失部分。请求体与响应原字节保存到 `data_dir/run_snapshots`。未知调用保持未知并计入预算，恢复不会清空消耗。程序不自动更换服务商、模型或请求格式。日报模型计划升级为 v4，报告格式仍为 v3；升级前未完成的 v3 模型计划由保留的旧包和对应配置恢复。`truncation_policy` 与 `max_strategy_changes` 只用于旧格式恢复。

## 恢复与补跑

```powershell
ai-daily-report.exe repair-output --target-date 2026-10-11 | Out-Host
ai-daily-report.exe backfill --start-date 2026-10-01 --end-date 2026-10-11 --dry-run | Out-Host
ai-daily-report.exe backfill --start-date 2026-10-01 --end-date 2026-10-11 --preflight <预检文件> --call-budget <已授权作业次数> --workers 2 | Out-Host
```

`repair-output` 根据已提交数据库重放原正文，零扫描、零模型调用。正文被人工修改时保留文件并返回冲突。正常报告命令可继续未完成的冻结运行。

补跑先冻结预检，执行时创建一致数据库备份，保留原始作业预算和所有日期状态。`--only-failed` 只重试已确认失败日期，`--only-no-model` 只处理无需模型的日期。成功响应和未知调用均保留。原预检和状态文件变化时停止。

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
