# 当前运行与验收状态

核验时间：2026-10-17 13:48（Asia/Shanghai）。任务信息来自 Windows 调度器回读，日报来源及模型发送开关来自当前本地配置；历史恢复结果来自下列独立核验材料。

## 正式任务

| 任务 | 程序包 | 配置目录 | 触发 | 回读状态 |
| --- | --- | --- | --- | --- |
| AI Daily Report | .releases/rust-20261017-archive-05 | config-rust | 每日 09:00，报告前一上海自然日 | Ready；下一次 2026-10-18 09:00 |
| AI Daily Report Collect | .releases/rust-20261017-archive-02 | config-rust/archive-collector | 每 30 分钟 | Ready；最近退出码 0 |

日报任务直接执行 ai-daily-report.exe daily --scheduled，当前 archive.daily_source 为 archive。采集任务执行 collect --scheduled，allow_work_dir_for_model 为 false。完整程序路径和参数以实际任务回读为准。

正式归档日报于 2026-10-17 10:50 按用户立即切换授权启用。报告及冻结输入为 v4，模型计划为 v5，工作进程协议为 v4。每日任务回读中的最近退出码仍为 1，对应切换前的旧扫描失败窗口；它不代表新归档包已经执行。首份真实定时归档日报尚待验收。

## 已完成与待验收

已完成：独立采集基线、归档一致性检查、手动调度检查、正式日报包切换、严格自检、任务回读及零模型的归档输入检查。真实基线始于 2026-10-17 约 05:06，保存 2,648 个原件，解析完整 1,323、部分 1,317、失败 8。原有覆盖缺口继续如实展示；现有正文提取范围保持不变，OCR 默认关闭。

2026-10-16 缺失日报已于 2026-10-17 13:10 恢复发布。原运行 4ca505c140e61faa39d2319dda19a43c 复用冻结扫描输入，报告为 v3、模型计划为 v4；新增九次成功模型请求，没有新扫描。覆盖 11 个来源，96 条事实包含 77 条确定事实、19 条待确认事实，引用检查通过，文件与数据库正文哈希一致，质量保持 partial。原失败调用和快照保留。此手动恢复单独记录。

待验收：2026-10-18 正常 09:00 任务生成目标日期 2026-10-17 的首份定时 v4 日报，并核对真实跨日采集。已有 automation 跟进在 08:30 只读预检、10:30 核对实际发布；检查清单见[归档定时验收入口](plans/2026-10-17-change-archive.md)。实验不额外触发日报或模型请求。无变化允许零模型调用，此时验收结论仅覆盖无模型的定时发布路径。

用户已批准暂停 ai 和 rust 两个重复跟进，原配置及历史记录保留。automation 在上述验收完成后停止；Windows 每日出报和半小时采集任务继续启用。

## 保留与回退边界

旧 Python 源码、专属工具、scanner_native、.venv、旧解析程序、旧包、配置备份和业务证据继续保留。旧扫描的三个连续成功窗口仍仅作为旧版清理条件；暂停跟进不改变历史验收结论，也不授权删除旧版材料。归档启用已经完成，跨日采集继续按实际记录核验。

旧扫描失败运行保留匹配配置，可显式恢复。较早未完成的 v3 模型计划由匹配旧包恢复。回退沿用[Windows 回退说明](windows-deployment.md)，保留新增报告、账本、快照和归档证据，不用旧数据库备份覆盖新增业务记录。

## 证据位置

以下路径相对仓库根目录，保留在本机，发布包不包含业务数据：

- 正式启用、自检、任务回读、原配置、任务 XML 与一致报告数据库备份：.artifacts/rust-migration/archive-daily-cutover-20261017/activation-20261017T024656Z/。
- 采集覆盖修复和归档输入测量：.artifacts/rust-migration/archive-daily-cutover-20261017/followup-20261017-1027/。
- 真实基线与采集部署：.artifacts/rust-migration/archive-v1/production/acceptance.json。
- 10 月 16 日恢复及恢复前一致备份：.artifacts/rust-migration/daily-recovery-20261017-01/verification.json。
- 跟进停用前配置：.artifacts/rust-migration/automation-cleanup-20261017-01/。

[历史归档索引](archive/README.md)保存各阶段原始数字、失败记录和结论。后续状态更新集中维护本文，并以实际任务、配置、数据库及文件重新核对。
