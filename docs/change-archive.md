# 历史归档查询与备份

归档保存旧流程观察到的不可变原件、全文提取、位置和报告依据，位于 `data_dir/archive`，结构保持 schema v1。当前程序提供历史读取、内容核验和一致备份。

```powershell
ai-daily-report.exe --config-dir <配置目录> archive history --path <历史路径> | Out-Host
ai-daily-report.exe --config-dir <配置目录> archive show --version <版本编号> | Out-Host
ai-daily-report.exe --config-dir <配置目录> archive show --version <版本编号> --unit 1 | Out-Host
ai-daily-report.exe --config-dir <配置目录> archive diff --from <旧版本编号> --to <新版本编号> | Out-Host
ai-daily-report.exe --config-dir <配置目录> archive verify | Out-Host
ai-daily-report.exe --config-dir <配置目录> archive backup --output <全新备份目录> | Out-Host
ai-daily-report.exe --config-dir <配置目录> stats --format json | Out-Host
```

差异查询选择两个版本已经保存的共同提取配置；多个共同配置按保存顺序选择最近一组。查询使用已保存的正文、位置和哈希，无需启动解析程序、重新读取工作文件或调用模型。缺失或损坏的历史材料会明确报错。

`archive backup` 保存一致数据库及相关证据；目标须为尚不存在的绝对路径目录。备份保留原件、提取内容与验证所需的文件。实际目录写入沿用授权范围。

历史优化、清理及旧生成续跑使用保留的原版本与配套配置。旧命令和操作记录见[历史说明](archive/history/change-archive-through-20261031.md)。个人日报的恢复与检索见[日报使用与恢复](daily-report-operations.md)。
