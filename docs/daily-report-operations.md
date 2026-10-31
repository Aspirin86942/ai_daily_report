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

`health` 检查 Daily/Health 两项任务上下文、报告与附件哈希、运行与历史异常、扫描索引、磁盘和数据库完整性；零模型调用。退出码 0/1/2 分别表示健康、预警、不健康。正式健康任务每天 11:00、23:00 执行。

周月报生成、批量补跑、覆盖修复作业、OCR 命令及 OCR 材料覆盖入口已退役。文件格式解析能力保持，图片文字识别保持关闭。
