# 历史归档查询与维护

当前日报每天 09:00 扫描前一天修改的文件。旧日内采集与补解析已退役，归档目录仅保留历史原件、全文、位置、版本和报告依据。

## 查询与核验

所有命令均可添加 `--config-dir <配置目录>`。以下查询读取 `data_dir/archive`，不会采集源文件、启动解析进程或调用模型。

```powershell
ai-daily-report.exe archive history --path "<历史原文件路径>" | Out-Host
ai-daily-report.exe archive show --version <版本编号> --unit 1 | Out-Host
ai-daily-report.exe archive diff --from <旧版本编号> --to <新版本编号> | Out-Host
ai-daily-report.exe archive verify | Out-Host
ai-daily-report.exe stats --from 2026-10-01 --to 2026-10-26 | Out-Host
```

历史统计中的 `collections`、`parse_runs` 对应已保存的执行记录。`historical_pending_extractions` 表示旧库中尚未完成的提取记录，不会触发补解析，也不属于当前健康检查的队列。

## 存储维护

`archive backup --output <新目录>` 保存一致备份；`archive optimize` 校验并透明压缩历史提取文件；`archive prune --before YYYY-MM-DD` 生成清理计划，只有显式 `--execute-plan <计划文件>` 才执行。涉及写入或删除时沿用相应授权范围，保留报告引用、未完成历史记录和不可重建证据。

`archive` 配置分组保留模型发送权限、历史存储压缩、表格比较键及用于识别历史提取结果的参数。采集频率、工作时段、队列并发、复制预算、`daily_source` 和旧输入分组设置已删除；在当前配置中出现会明确报错。历史提取参数只用于识别已保存结果，日常扫描预算由扫描配置和个人日报期限控制。

日报及附件修复使用 `repair-output`，直接读取报告库中的最终字节。恢复旧日内链路需匹配旧程序和独立配置，具体恢复资料见[Windows 回退说明](windows-deployment.md#备份与回退)。旧操作流程保存在[历史操作记录](archive/history/collection-operations-through-20261027.md)。

当前任务、验收和恢复清单见[当前状态](current-state.md)。
