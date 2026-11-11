# 个人日报使用与恢复

当前源码专注个人日报。正式程序版本和任务回读见[当前状态](current-state.md)；停用流程的原操作说明见[历史记录](archive/history/daily-report-operations-through-20261031.md)。

## 生成与补充

`daily` 默认汇总上海时间上一个已结束自然日。每天 09:00 的任务扫描一次工作目录；错过触发时间由任务计划程序补执行。已发布日期再次触发时复用原报告，零模型调用。

```powershell
ai-daily-report.exe --config-dir <配置目录> daily --dry-run | Out-Host
ai-daily-report.exe --config-dir <配置目录> daily --target-date 2026-10-30 -i "补充说明" | Out-Host
ai-daily-report.exe --config-dir <配置目录> daily --target-date 2026-10-30 --no-save | Out-Host
ai-daily-report.exe --config-dir <配置目录> daily --target-date 2026-10-30 --regenerate --reason "补充材料后重新生成" | Out-Host
```

人工补充可通过 `-i` 或 `report.notes_dir/YYYY-MM-DD.md` 提供。两处补充去除重复段落后合计最多 2,000 字，文件材料最多 18,000 字。手工指定旧日期取得的是当前仍可见的文件内容，覆盖边界保留在报告中。

## 选择公司网关或 OpenAI

在项目目录运行下面的命令。`company` 使用公司网关，`openai` 使用 OpenAI 兼容接口，当前连接本机 Magpie；切换后，手工命令和定时任务都读取同一份生效配置。

```powershell
# 查看目标接口、模型及密钥是否已配置，零模型调用。
.\scripts\set_model_service.ps1 -Service openai

# 首次使用 OpenAI：用虚构材料完成接口检查，然后切换。
.\scripts\set_model_service.ps1 -Service openai -Apply -CheckModel

# 切回已验证的公司网关，零模型调用。
.\scripts\set_model_service.ps1 -Service company -Apply
```

OpenAI 密钥使用项目根目录现有 `.env` 中的 `OPENAI_API_KEY`，或 `config-rust/.secrets.yaml` 的 `api.openai_api_key`。添加密钥时保留已有公司网关配置，密钥不进入 Git 或诊断输出。缺少密钥、接口验证失败或配置回读不一致时，切换不会生效。

`config-rust/model-services.json` 保存两种连接的模型、地址和代理参数；仓库中的安装示例为 `scripts/model-services.example.json`，发布包将其放在 `config/model-services.example.json`。切换器只更新 `settings.windows.yaml` 中的 `llm`、`proxy` 分组，并保留其余设置和原文件备份；该覆盖文件使用 JSON 文本，程序按 YAML 兼容格式读取。已有非 JSON 的覆盖文件需要先保留原件并转换格式。项目 `.env` 或进程中的 `DAILY_REPORT_LLM__*`、`DAILY_REPORT_PROXY__*` 具有更高优先级，应避免同时配置同一项。

当前 OpenAI 连接地址为 `http://127.0.0.1:3425/v1`，模型为 Magpie 提供的 `openai/gpt-6.1-terra`，使用 `reasoning_effort: none` 和 `max_completion_tokens`。公司网关保持原有 DeepSeek 请求参数。模型参数依据 [GPT-6 官方说明](https://developers.openai.com/api/docs/guides/latest-model)与 [Chat Completions 参数说明](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)，具体路由以 Magpie 的模型列表和接口实测为准。旧冻结请求继续按保存时的格式校验和恢复。

公司网关需要公司网络或 VPN；本机 OpenAI 入口需要 Magpie 持续运行并监听 3425 端口，程序直接访问本机接口。使用 OpenAI 官方服务时，可在连接配置中改为 `https://api.openai.com/v1`、官方模型名称及对应密钥。连接可单独设置 `proxy.https_proxy`，只作用于该程序。选择一项连接后，请求只发送给所选服务。

## 摘要与依据

摘要通常为 3～5 项，目标 420～560 字，最多 5 项、1,500 字；材料稀少时允许更短。未超过硬上限的初稿完整发布。超长时使用单独记录的精简请求，保持事项数量、顺序、类别、确定程度和引用；精简失败才缩减完整事项，保留下一步计划。

完整摘录、来源位置和指纹形成独立 Markdown 依据附件，与摘要合计最多 100,000 字节。数据库保存两者的原始字节和哈希。程序先发布附件，再发布摘要，最后更新发布状态。

## 故障恢复与历史查询

```powershell
ai-daily-report.exe --config-dir <配置目录> runs --target-date 2026-10-30 | Out-Host
ai-daily-report.exe --config-dir <配置目录> repair-output --target-date 2026-10-30 | Out-Host
ai-daily-report.exe --config-dir <配置目录> repair-output --run-id <运行编号> | Out-Host
ai-daily-report.exe --config-dir <配置目录> search --query 审批 --kind all | Out-Host
```

目标日期与运行编号只能选择一项。运行编号可定位旧日报、周报或月报。输出恢复读取数据库原文及附件，零扫描、零模型调用；检测到人工编辑时保留文件并报告冲突。

个人日报执行中断后，重新运行该日期会读取冻结输入、验证请求和响应身份，复用已有成功响应，仅执行尚未完成的个人步骤。源文件后续变化不会改变该冻结输入。处理材料过期后，已保存的发布正文及附件仍可恢复。

新程序保留旧通用日报与周月报的历史记录；这些旧生成任务的续跑使用原版本及配套配置。旧 `evidence` 配置可以查询和恢复历史，用于生成时会明确提示功能退役。新配置默认使用个人日报。

## 预算与巡检

程序总预算 300 秒，270 秒后停止启动新工作；扫描最多 120 秒、一个解析进程。每个模型步骤仍有有限尝试和请求超时，通信失败后停止本轮请求。未知结果继续保留在调用账本中。预览、失败和重试均留有记录；累计调用次数、token 与金额不作为调用阻断额度。

处理文件与空间预留位于 `data_dir/daily_cache`，合计最多 1 GiB，正文材料最长保留 7 天。诊断细节保留 30 天、最多 8 MiB；个人运行日志保留 30 天、最多 2 MiB。历史异常在材料过期后仍可见，并在当前发布解决问题后解除。

`health` 检查 Daily/Health 两项任务上下文、报告与附件哈希、运行与历史异常、扫描索引、磁盘和数据库完整性；零模型调用。退出码 0/1/2 分别表示健康、预警、不健康。正式健康任务每天 11:00、23:00 执行，启用 `StartWhenAvailable`，错过计划时间后在任务可运行时补执行。

未指定目标日期时，健康检查与日报任务使用相同的上海时间计划：09:00 前检查前天，09:00 起检查昨天。缺失报告在计划触发时间加 300 秒运行预算内显示尚未到期；实际补跑仍在预算内时显示正在生成。超过预算仍未发布继续报错。已发布文件仍立即校验，历史失败、损坏文件和过期未完成运行继续告警。

周月报生成、批量补跑、覆盖修复作业、OCR 命令及 OCR 材料覆盖入口已退役。文件格式解析能力保持，图片文字识别保持关闭。
