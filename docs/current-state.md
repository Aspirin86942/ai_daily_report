# 当前运行与验收状态

核验时间：2026-10-18 03:55（Asia/Shanghai）。任务信息来自 Windows 调度器回读，日报来源及模型发送开关来自当前本地配置；历史恢复结果来自下列独立核验材料。

## 正式任务

| 任务 | 程序包 | 配置目录 | 触发 | 回读状态 |
| --- | --- | --- | --- | --- |
| AI Daily Report | .releases/rust-20261018-release-01 | config-rust | 每日 09:00（兜底 09:30、10:30）+ 登录后 1 分钟 | Ready；下一次 2026-10-18 09:00 |
| AI Daily Report Collect | .releases/rust-20261017-archive-02 | config-rust/archive-collector | 每 30 分钟 | Ready；最近退出码 0 |
| AI Daily Report Health | .releases/rust-20261018-release-01 | config-rust | 每日 11:00、23:00 | Ready；下一次 2026-10-18 11:00 |

日报任务直接执行 ai-daily-report.exe daily --scheduled，当前 archive.daily_source 为 archive；三个日历触发器加登录后 1 分钟兜底，首次成功后的后续触发快速返回。采集任务执行 collect --scheduled，allow_work_dir_for_model 为 false，本轮未改动。健康任务执行 scripts/health_notify.ps1，只读巡检并写健康文件与本地通知，不调用模型。完整程序路径和参数以实际任务回读为准。

正式归档日报于 2026-10-17 10:50 按用户立即切换授权启用；2026-10-18 03:40 左右按本轮部署授权将日报与健康任务切换到 `rust-20261018-release-01`（包含定时链路可靠性、日报可读性、历史检索与工程清理）。部署门禁、`doctor --strict`、`doctor --check-model`（1 次合成请求）均通过，任务注册后回读一致；旧包与切换前任务 XML 保留。首份由新包生成的真实定时 v4 日报尚待验收。

## 已完成与待验收

已完成：独立采集基线、归档一致性检查、手动调度检查、正式日报包切换、严格自检、任务回读及零模型的归档输入检查。真实基线始于 2026-10-17 约 05:06，保存 2,648 个原件，解析完整 1,323、部分 1,317、失败 8。原有覆盖缺口继续如实展示；现有正文提取范围保持不变，OCR 默认关闭。

2026-10-16 缺失日报已于 2026-10-17 13:10 恢复发布。原运行 4ca505c140e61faa39d2319dda19a43c 复用冻结扫描输入，报告为 v3、模型计划为 v4；新增九次成功模型请求，没有新扫描。覆盖 11 个来源，96 条事实包含 77 条确定事实、19 条待确认事实，引用检查通过，文件与数据库正文哈希一致，质量保持 partial。原失败调用和快照保留。此手动恢复单独记录。

2026-10-18 03:40–03:55 完成本批部署：发布包 `rust-20261018-release-01`（445 个文件、约 30 MB）构建并校验；日报任务切换为新包并增加 09:30、10:30 兜底触发器；健康任务注册（11:00、23:00）；采集任务保持原样。部署期间发现并修复任务回读比较多触发器的缺陷（Windows 导出会省略默认值，原比较按单触发器字段处理，导致任务被临时禁用）；修复后重新注册，回读一致。切换前任务 XML 已保存。部署后手动运行 `health`：唯一错误为 `REPORT_NOT_PUBLISHED`（目标日 2026-10-17 尚未发布），其他检查通过，属预期。

待验收：2026-10-18 正常 09:00 任务由新包生成目标日期 2026-10-17 的首份定时 v4 日报；09:30、10:30 兜底触发不得重复发布；11:00 健康任务写入健康文件并按状态通知；并核对真实跨日采集。已有 automation 跟进在 08:30 只读预检、10:30 核对实际发布；检查清单见[归档定时验收入口](plans/2026-10-17-change-archive.md)。无变化允许零模型调用，此时验收结论仅覆盖无模型的定时发布路径。

用户已批准暂停 ai 和 rust 两个重复跟进，原配置及历史记录保留。automation 在上述验收完成后停止；Windows 每日出报和半小时采集任务继续启用。

## 仓库内已完成与上线状态

以下记录为 2026-10-18 的仓库与上线状态，正式验收仍沿用本文“已完成与待验收”：

| 项目 | 状态 | 入口/提交 |
| --- | --- | --- |
| 定时链路可靠性 | 已上线（`rust-20261018-release-01`） | [可靠性设计](plans/2026-10-17-scheduled-reliability-design.md)；`204cd7d` |
| 日报可读性 | 已上线（同一发布包） | [可读性设计](plans/2026-10-18-readability-design.md)；`c126bd5` |
| 历史检索（F1） | 已上线（同一发布包，`search` 命令） | [历史检索设计](plans/2026-10-18-history-search-design.md)；`eaf2f68` |
| 工程清理（E） | 已合并（文档、脚本说明与产物清理） | [工程清理设计](plans/2026-10-18-engineering-cleanup-design.md)；`5efc95e` |

新包同时包含可读性模板与提示词变更，因此首份定时日报的版式与措辞与旧包不同，验收按新包记录。`search` 首次运行会在 `data/db` 生成 `report_search.sqlite3` 索引，可删除重建，报告库只读。

## 保留与回退边界

旧 Python 源码、专属工具、scanner_native、.venv、旧解析程序、旧包、配置备份和业务证据继续保留。旧扫描的三个连续成功窗口仍仅作为旧版清理条件；暂停跟进不改变历史验收结论，也不授权删除旧版材料。归档启用已经完成，跨日采集继续按实际记录核验。

回退：旧日报包 `.releases/rust-20261017-archive-05` 与采集包 `archive-02` 保留；切换前任务 XML 在 `data/task_backups/` 与 `.artifacts/rust-migration/deploy-20261018/daily-before.xml`。回退沿用[Windows 回退说明](windows-deployment.md)：先禁用新任务、恢复旧任务 XML 并启用，保留新增报告、账本、快照和归档证据，不用旧数据库备份覆盖新增业务记录。

## 证据位置

以下路径相对仓库根目录，保留在本机，发布包不包含业务数据：

- 本批部署：切换前后任务 XML、任务回读、`doctor --strict` 与 `health` 输出：.artifacts/rust-migration/deploy-20261018/。
- 正式启用、自检、任务回读、原配置、任务 XML 与一致报告数据库备份：.artifacts/rust-migration/archive-daily-cutover-20261017/activation-20261017T024656Z/。
- 采集覆盖修复和归档输入测量：.artifacts/rust-migration/archive-daily-cutover-20261017/followup-20261017-1027/。
- 真实基线与采集部署：.artifacts/rust-migration/archive-v1/production/acceptance.json。
- 10 月 16 日恢复及恢复前一致备份：.artifacts/rust-migration/daily-recovery-20261017-01/verification.json。
- 跟进停用前配置：.artifacts/rust-migration/automation-cleanup-20261017-01/。

[历史归档索引](archive/README.md)保存各阶段原始数字、失败记录和结论。后续状态更新集中维护本文，并以实际任务、配置、数据库及文件重新核对。
