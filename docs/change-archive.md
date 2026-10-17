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

采集任务以当前用户运行，每 30 分钟触发，单实例，执行上限 20 分钟。定时任务不调用模型。未完成的解析留在数据库队列，下一轮继续。首次清单中暂时被占用的文件仍属于基线，重试不会计作当天新增。

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

## 内容和资源边界

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

采集使用独立配置目录和独立的 `AI Daily Report Collect` 任务。部署时保留原日报任务 XML、程序包和配置；采集配置关闭 `allow_work_dir_for_model`，不需要复制模型凭据。建立真实基线、执行 archive verify 并检查任务定义后，才启用每 30 分钟采集。

日报来源切换需要同时满足原 Rust 切换记录的三个连续真实调度窗口，以及采集任务跨上海自然日的真实运行证据。只读核对计划任务事件、运行时间、归档 collection、版本观察日期和缺口记录；本地模拟、手动启动和构造日期不能计作这些真实窗口。保留每次检查原件，未到日期时保持待验收。

满足门槛后备份将要修改的日报配置和任务定义，确认没有仍须使用旧模型计划的未完成运行，使用新包检查配置、模板与工作进程，再将日报配置的 archive.daily_source 设为 archive，并更新日报任务程序路径。普通定时日报沿用已授权模型范围和预算设置；验收不额外调用远程模型，不重写历史日报。

需要回退时，恢复保存的日报任务 XML 和对应配置，继续使用保留的旧发布包。归档目录继续保留，采集任务可以独立运行；回退不删除原件、版本索引、日报证据或已有报告。
