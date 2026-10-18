# 变化归档与可追溯日报

归档保存每次实际观察到的文件版本。首次采集建立基线，已有内容不计为新增成果；后续变化按首次观察的上海日期进入日报。原始修改时间单独保留。两轮采集之间出现又被覆盖的版本无法恢复。

## 建立和采集

所有命令均使用独立配置目录。PowerShell 调用 Windows 无控制台 EXE 时接管道，以等待完成。

~~~powershell
& $program --config-dir $config collect --baseline | Out-Host
& $program --config-dir $config collect | Out-Host
$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
& $program --config-dir $config task plan --kind collect --sid $sid | Out-Host
& "$release/scripts/install_daily_task.ps1" -ProjectRoot $release -ConfigDirectory $config -Kind collect -Apply
& "$release/scripts/check_daily_task.ps1" -ProjectRoot $release -ConfigDirectory $config -Kind collect
~~~

采集任务以当前用户运行，按 archive.collection_interval_minutes 全天触发（兼容默认 30 分钟；本机当前安排见 current-state.md），单实例，执行上限 20 分钟。定时任务不调用模型。未完成的解析留在数据库队列，下一轮继续。首次清单中暂时被占用的文件仍属于基线，重试不会计作当天新增。

可靠文件身份、变更时间和大小均未变化时，当日普通采集不读取正文。每日独立执行完整哈希校验，每个文件的进度持久化；无法取得可靠身份的文件每轮校验。新增版本先流式复制并计算 SHA-256，再校验稳定性、持久化恢复凭据、原子落盘和登记。A→B→A 保留三次版本记录，只存两份原件。

## 查看依据

~~~powershell
& $program --config-dir $config archive history --path 'D:/work/policy.docx' | Out-Host
& $program --config-dir $config archive show --version $version --unit 12 | Out-Host
& $program --config-dir $config archive diff --from $older --to $newer | Out-Host
& $program --config-dir $config archive retry --version $version | Out-Host
& $program --config-dir $config daily --source archive --target-date 2026-10-17 --no-save | Out-Host
& $program --config-dir $config stats --from 2026-10-17 --to 2026-10-17 --files-csv './all-files.csv' | Out-Host
~~~

查询默认 JSON，可使用 --format text。stats 的汇总显示最多 5,000 条文件记录，CSV 流式导出全部匹配记录；慢文件排名和并行工作总耗时覆盖完整查询范围。并行任务耗时之和与实际完成耗时分别记录，历史缺失值为 null / unknown。

`parse_coverage` 按去重后的解析结果统计成功、部分覆盖、失败、输出超限及各类内容缺口。同一原件被多个文档复用时只计算一次解析；文件数量另见文件记录。正文相同但图片、缺失正文或超限部分无法比较时，日报明确列出待核对缺口。

统计日期按上海时区的实际执行日期筛选，报告对应日期单独保存在 `target_date`。例如 17 日生成 16 日日报，模型用量和成稿耗时都计入 17 日；也可通过 `--run-id` 查看该报告运行的全部阶段。

日报从相邻版本差异准备输入，引用绑定原件版本、解析版本、正文单元、位置、原文和哈希。重要的修改事实必须同时具有前后依据；缺失时进入待确认。表格变化附对应表头。不同标题引用同一稳定事项编号时合并，全日完成后按审批权限与制度、金额及异常、完成或阻塞、期限、一般信息选最多五条重点。

预览和失败重试不消耗变化；正式发布后登记覆盖。晚完成解析的变化进入下一份日报并列原观察日期。归档来源不可用时明确报错。已有日报保持原内容，显式重生成沿用现有审计机制。周报、月报使用 --source db，可读取新旧日报。

日报直接复用归档正文、差异和证据，不启动原目录扫描或解析。无变化且无用户补充时不调用模型；采集、解析缺口仍进入报告。未建立基线时明确报错；目标日期早于或跨越归档开始时间时，报告记录 `ARCHIVE_BASELINE_WINDOW`、上海时间的采集起点及首次基线不计新增的说明，质量为部分覆盖。

日报同时核对目标日期内已完成的采集记录，按当时生效的采集间隔、允许 1 分钟调度偏差检查连续性。停止、漏跑、异常退出或尚未完成的采集不能证明该时段没有变化；缺少记录时输出 `ARCHIVE_COLLECTION_GAP`、上海时间范围和无法判断变化的说明，日报质量为部分覆盖。恢复后的新采集不能消除历史日期内的缺口。已完成但部分文件解析失败的采集继续沿用文件缺口说明；生成历史日报时不会因今天停止采集而误报历史覆盖不足。检查只读取归档索引，不扫描原目录、不启动解析进程、不增加模型请求。

自动恢复按输入来源分开：归档日报只恢复已冻结归档证据的运行，扫描日报只恢复扫描运行。旧失败运行及其快照、调用账本保留，可使用匹配配置显式执行 `daily --source scan --target-date <日期>` 恢复。分类依据是归档库中在运行进入可恢复阶段前登记的证据，不读取旧运行的大份冻结正文；使用同一连接与预编译查询。

模型把合法但带有不确定说明的事实放入确认列表时，程序保留其原文、编号和来源，并归入待确认；不增加模型请求。空内容、超限内容、无效来源和错误引用仍拒绝，待确认事实仍须完整出现在成稿的待确认部分。

## 内容和资源边界

每轮采集保存文件数量、无变化数量、读取量等汇总。文件明细只记录变化或异常；普通无变化和每日哈希校验不再重复保存无变化明细，历史明细继续保留。采集频率变更在索引的 metadata 中追加生效时间，日报与健康检查共用历史频率，不改写旧日期的漏采事实。

Windows NTFS 上默认启用 `archive.compress_extractions`。新解析目录继承透明压缩，正文读取、随机定位、输出预算和证据哈希保持原有口径；不支持压缩的文件系统保留普通文件并记录警告。已有解析存档可运行 `archive optimize`，逐项核验压缩前后哈希并返回实际磁盘占用及节省字节数。操作可重复执行，不删除原件、版本、引用和历史明细。复制备份后实际压缩属性可能不同，仍按原有字节哈希核验。

原件、body.md、units.jsonl、units.idx、索引数据库和冻结日报证据保存于 data_dir/archive。正文按块输出，文件、解析配置和差异结果按哈希复用。现有缓存与输入快照清理不触及该目录。

PDF 记录全文页码、无文本页和图片检查缺口；Office 保留可读取的正文、表格、公式及缓存值。扫描图片、加密、损坏、未支持内容及输出限制均保留缺口，不执行宏、公式或外部链接。

默认总预算 3 GiB，主进程预留 512 MiB，每个解析进程上限 512 MiB，系统预留至少 2 GiB。解析并发 1～5、读取并发 1～2，内存不足时仅向下调整并保留队列。单文件解析输入上限 512 MiB、输出合计上限 256 MiB。可用磁盘低于 20 GiB 提醒，写后不足 5 GiB 则延期写入。瞬时失败最多自动尝试三次；确定性失败等待新版本或显式 retry。

archive.roots 允许本地采集，archive.model_roots 单独授权发送。新增 roots 默认仅本地使用，包括 work_dir 下的子目录。每次实际模型发送重新检查当前配置；冻结日报和周月汇总不能绕过权限收紧。

## 校验、备份和清理

~~~powershell
& $program --config-dir $config archive verify | Out-Host
& $program --config-dir $config archive backup --output 'D:/backup/archive-20261017' | Out-Host
& $program --config-dir $config archive prune --before 2026-01-01 | Set-Content -Encoding utf8NoBOM './prune-plan.json'
& $program --config-dir $config archive prune --execute-plan './prune-plan.json' | Out-Host
~~~

备份使用 SQLite 一致快照，再复制不可变原件、解析结果和日报证据，逐项核对哈希。恢复时保留原目录，将已核验的备份放入一个独立配置的数据目录下 archive，再执行 verify；核验完成后才能使用该配置。备份不含模型凭据。

清理先生成列明版本、引用和字节数的计划。执行时重新核对计划和引用；仍被日报、冻结运行、当前版本、当前前一版或尚未报告变化使用的证据受到保护。先登记清理状态，再逐个删除计划中的文件；中断后可执行同一计划继续，元数据保留。

## 性能验收

scripts/benchmark_archive.py 使用冻结语料和空应用缓存，比较 2×5 个读取/解析并发组合。按格式、大小、历史慢文件分层取样，最快两组各进行三次完整归档；内容签名不同的配置退出速度排名。总耗时接近 3% 时选内存更低者。

Windows Job Object（进程组资源计量）记录包含已退出子进程的 CPU、I/O 和内存提交峰值；工作集每 20 毫秒采样，另列采样限制。操作系统文件缓存未清空。所有原始结果、失败文件、解析器和执行通道留在本地验收目录；不上传原始业务材料。

## 生产启用与回退

正式任务、配置、授权记录、待验收事项及备份位置统一见[当前状态](current-state.md)。部署与回退步骤见[Windows 说明](windows-deployment.md)。

采集使用独立配置目录和独立任务，仅本地归档时关闭 allow_work_dir_for_model。建立真实基线、执行 archive verify 并检查任务定义后，按配置的全天间隔启用采集。切换前保留任务 XML、程序包、配置和一致报告数据库备份，切换后回读任务和配置。

真实跨日采集按实际记录核对，本地模拟、手动启动和构造日期分别记录。旧扫描的三个连续成功窗口仅约束旧版清理，保持既有授权与验收边界。

回退时恢复保存的日报任务 XML 和对应配置，使用匹配旧包；归档采集可独立运行。保留原件、版本索引、日报证据、报告库与调用账本，不用旧备份覆盖新增业务记录。
