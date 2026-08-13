# 扫描器优化与性能提升设计

Status: DRAFT
Mode: Builder

## 1. 问题陈述

当前扫描器实现集中在 [src/services/file_scanner.py](/D:/bochu_work/ai_daily_report/src/services/file_scanner.py) 一个类中，承担了目录遍历、文件筛选、并发调度、格式解析、超时控制、内容预算和结果聚合等多种职责。这个实现已经能工作，但在两个维度上存在明显瓶颈：

1. 目录遍历慢  
   当前扫描以 `os.walk()` 为主，每次扫描都要从工作目录递归遍历，再根据 `mtime` 和扩展名过滤。工作目录一旦变大，扫描成本会首先消耗在“找到文件”上，而不是“解析文件”上。

2. 单文件解析慢  
   对 PDF / Excel / DOCX / PPTX 这类重格式文件，当前解析代价高且波动大。即使已经增加了单文件超时，仍然存在进程频繁拉起、重复解析未变更文件、不同格式混跑导致吞吐不稳定的问题。

这次设计的目标不是继续在现有 `FileScanner` 上叠条件分支，而是给出一套中期重构方案，并进一步给出终极优化路线图，让扫描器从“单类实现”演进到“可持续扩展的扫描子系统”。

## 2. 已确认约束

以下约束已在本轮对话中确认：

- 主瓶颈优先级：`目录遍历慢 + 单文件解析慢`
- 允许引入持久化缓存 / 索引
- 一致性要求：强一致
- 文件系统假设：本地 NTFS 为主
- 本轮要的是详细方案，不是立即继续改生产代码

补充边界：

- CLI 外部调用接口尽量保持稳定
- 现有 `ScanResult` / `FileContext` 兼容性优先
- 可接受为了性能和可维护性拆模块、引入新服务和新存储表
- 终极路线图可以包含 Windows/NTFS 强绑定能力，但要显式说明退化路径

## 3. 当前基线判断

基于当前实现，可以把现状归纳为五个结构性问题：

### 3.1 发现与解析耦合

`scan_files()` 现在直接做：

- 目录遍历
- 日期过滤
- 扩展名过滤
- 忽略规则过滤
- 并发调度
- 文件解析
- 全局字符预算

这导致任何一个环节要优化，都必须改同一个类，同一个调用链。

### 3.2 目录发现成本与目录规模线性绑定

哪怕当天只改了 2 个文件，只要工作目录里有 20 万个文件，当前扫描仍然会近似付出一次全量递归遍历成本。这是最典型的“发现阶段压过业务阶段”的性能问题。

### 3.3 解析缓存粒度不足

虽然现在已经加了文件大小上限、文本预算和单文件超时，但只要文件命中这次扫描条件，就会重新读取和重新解析。对“文件没变但再次被时间窗口命中”的场景，没有真正的解析结果复用。

### 3.4 超时模型正确但成本偏高

当前单文件超时已经转向“独立子进程 + 父进程回收”，这是方向正确的，因为它能真正中断。但如果每个重文件都单独 spawn 一个进程，那么：

- 启动开销高
- Windows 下进程启动抖动明显
- 对大量中小文件不划算

### 3.5 缺少扫描级观测指标

当前日志更偏执行过程，不足以回答这些关键问题：

- 这次扫描慢，是慢在发现还是慢在解析？
- 哪个格式最贵？
- 缓存命中率多少？
- 超时率、跳过率、失败率是多少？
- NTFS Journal 是否真的减少了全量遍历？

如果这些指标没有沉淀，后续优化会再次退化为“凭感觉调参数”。

## 4. 设计前提

本设计采用以下前提：

1. Windows + NTFS 是主路径，不为非 NTFS 环境牺牲主方案上限
2. 强一致优先于“非常激进的近似缓存”
3. 目录发现和文件解析必须拆成两个子系统
4. 解析 worker 应该是长生命周期池，而不是每文件一个短命进程
5. 终极形态必须能回答“为什么快了”“快在哪”“哪里还慢”

## 5. 候选方案

### APPROACH A: NTFS Journal + SQLite 索引 + 受监督解析工作池

Summary: 用 NTFS USN Journal 做目录发现，用 SQLite 做文件索引和解析缓存，用长生命周期 worker 池执行重格式解析。

Effort: L  
Risk: Med

Pros:

- 同时命中“目录遍历慢”和“单文件解析慢”
- 强一致可落到 `FRN / USN` 级别，而不是只靠 `mtime`
- 能从架构上把发现、规划、解析、聚合分开

Cons:

- Windows / NTFS 绑定明显
- 初次实现复杂度高
- 需要新增索引表、checkpoint 和 worker 监督逻辑

### APPROACH B: 元数据缓存 + 保守全量遍历 + 格式分级解析

Summary: 保留 `os.walk()`，但持久化 `path / mtime / size / parser_profile`，只重解析变更文件，同时做轻重格式分流。

Effort: M  
Risk: Low

Pros:

- 改造成本低
- 能快速落地

Cons:

- 目录特别大时仍然被 `os.walk()` 卡住
- 强一致只能做到工程近似，不是 NTFS 原生级别

### APPROACH C: 目录发现服务与解析服务先解耦，缓存延后

Summary: 先把职责拆开，目录发现可预留 NTFS Journal 接口，但第一阶段不引入正式解析缓存。

Effort: M/L  
Risk: Med

Pros:

- 架构边界清楚
- 适合作为大型重构前置阶段

Cons:

- 只能显著解决目录遍历慢
- 单文件解析慢的收益有限

## 6. 推荐方案

推荐采用 `APPROACH A`。

理由很直接：这次目标不是“让代码看起来更干净”，而是要解决两个最贵的主瓶颈。如果继续保留全量递归遍历，或者只做轻量缓存，发现阶段的成本仍然会随目录规模线性增长；如果继续让重格式文件每次都独立 spawn 短命进程，解析阶段的成本仍然会在中高文件量下抬头。

`APPROACH A` 是唯一一条能同时把“目录发现”和“重格式解析”两个核心代价都切掉的中期路径。

## 7. 推荐架构

### 7.1 模块划分

建议把扫描器拆成以下 6 个单元：

1. `FileDiscoveryService`
2. `ScanIndexStore`
3. `ScanPlanner`
4. `ParserSupervisor`
5. `FormatParsers`
6. `ResultAggregator`

### 7.2 各单元职责

#### `FileDiscoveryService`

职责：

- 维护扫描根目录
- 初次建立目录快照
- 读取 NTFS USN Journal 获取增量变更
- 在 journal 不可用或 checkpoint 失效时回退全量发现

输出：

- 变更文件集合
- 删除文件集合
- 需要全量重建的信号

#### `ScanIndexStore`

职责：

- 保存文件库存 `file_inventory`
- 保存解析缓存 `parse_cache`
- 保存扫描运行信息 `scan_runs`
- 保存 NTFS checkpoint

关键字段建议：

- `volume_id`
- `file_reference_number`
- `usn`
- `path`
- `mtime_ns`
- `size_bytes`
- `extension`
- `parser_profile`
- `parse_status`
- `parse_error`
- `content_excerpt`
- `content_hash`

这里的关键点是：**路径不再是唯一身份**。在 NTFS 上，强一致主键应优先依赖 `volume + FRN`，路径只是可读定位字段。

#### `ScanPlanner`

职责：

- 把“发现到的变化”转成“本次真正需要参与扫描的候选文件”
- 应用日期范围、扩展名、忽略模式、大小限制、模式预算
- 决定哪些文件可复用缓存，哪些必须重解析

Planner 存在的意义是让“发现到变化”和“需要参与业务扫描”之间多一层确定性决策，不把业务规则混进发现层。

#### `ParserSupervisor`

职责：

- 维护长生命周期解析 worker 池
- 区分轻量文本路径和重格式路径
- 负责单任务超时、worker 重启、异常隔离
- 返回结构化的解析结果

关键要求：

- 不再每文件新建进程
- worker 按格式分 lane，例如 `pdf lane`、`office lane`
- 单个 worker 超时或崩溃时只重建该 worker，不影响整次扫描

#### `FormatParsers`

职责：

- 提供纯解析函数
- 每个格式一个清晰入口
- 不承担调度和缓存决策

建议拆分为：

- `text_parser.py`
- `excel_parser.py`
- `pdf_parser.py`
- `pptx_parser.py`
- `docx_parser.py`

#### `ResultAggregator`

职责：

- 合并缓存结果与新解析结果
- 应用 `total_max_chars`
- 统计成功 / 失败 / 超时 / 复用 / 跳过
- 生成与现有接口兼容的 `ScanResult`

## 8. 数据流

完整链路建议如下：

1. `FileDiscoveryService` 获取变更文件
2. `ScanIndexStore` 更新库存和 checkpoint
3. `ScanPlanner` 生成候选集与重解析集
4. 小文本文件走主进程快速路径
5. PDF / Excel / DOCX / PPTX 走 `ParserSupervisor`
6. 解析结果回写 `ScanIndexStore`
7. `ResultAggregator` 聚合输出 `ScanResult`

## 9. 强一致策略

既然你明确选择“强一致”，这里就必须定死：

- 文件一旦变更，当次扫描必须重新发现
- 只要 `USN` 或 `FRN` 对应元数据变化，必须重新评估缓存是否失效
- 不允许“本次先用旧缓存，下次再补”
- 如果 NTFS Journal checkpoint 丢失、根目录搬迁、卷标变化或索引损坏，必须触发全量重建

也就是说，性能优化不能建立在“先凑合用旧结果”的前提上。

## 10. 性能优化策略

### 10.1 目录发现层

- 主路径：USN Journal 增量发现
- 回退路径：首次 bootstrap 全量扫描
- 避免每次都 `os.walk()`
- 排除目录在 discovery 阶段就收口，不把无关文件送入后续 planner

### 10.2 解析层

- 文本类：主进程内读取，按预算截断
- 重格式：长生命周期 worker 池
- 格式级并发：PDF 与 Excel 分 lane，避免互相挤占
- 大文件提前跳过：在真正解析前就做大小门禁

### 10.3 缓存层

- 解析缓存粒度按“文件身份 + parser_profile”
- `parser_profile` 需要包含会影响输出的参数，例如：
  - `summary_mode`
  - `excel_max_rows`
  - `pdf_max_pages`
  - `text_max_chars`
  - parser 版本号

只要 profile 变化，即使文件没变，也要视为缓存失效。

## 11. 可观测性设计

这部分必须作为一等能力，而不是“最后顺手加日志”。

至少记录以下指标：

- `discovery_duration_ms`
- `planner_duration_ms`
- `parse_duration_ms`
- `aggregation_duration_ms`
- `total_scan_duration_ms`
- `files_discovered`
- `files_considered`
- `files_reused_from_cache`
- `files_reparsed`
- `files_timed_out`
- `files_skipped_by_size`
- `files_failed`
- `files_by_extension`

推荐输出两层：

1. 运行日志：面向排障
2. `scan_runs` 表：面向趋势分析和回归比较

这样后续才能回答“本次比上次快了多少”和“慢在哪个阶段”。

## 12. 错误模型

建议把错误收敛成稳定类别，而不是只保留原始异常文本：

- `timeout`
- `file_too_large`
- `unsupported_format`
- `decode_error`
- `parser_crash`
- `subprocess_invalid_payload`
- `inventory_resync_required`

外部仍然可以保留 `message`，但内部统计和监控必须依赖稳定 `error_code`。

## 13. 测试策略

### 13.1 Discovery 测试

- 初始 bootstrap
- 增量 journal 更新
- journal 失效触发全量重建
- 排除目录生效

### 13.2 Planner 测试

- 日期范围过滤
- 扩展名过滤
- 大小限制过滤
- 缓存命中 / 失效判定

### 13.3 ParserSupervisor 测试

- 正常解析
- 单任务超时
- worker 崩溃后自动重建
- lane 隔离

### 13.4 Cache 测试

- 文件未变 + profile 未变命中缓存
- 文件未变 + profile 变化强制重解析
- 文件变更强制重解析

### 13.5 集成测试

- 小文本全链路
- PDF / Excel 混合链路
- 强一致回退链路

## 14. 分阶段实施计划

### Phase 1: 结构解耦

目标：先把单类实现拆出边界，不引入 NTFS Journal。

交付：

- `FileDiscoveryService`
- `ScanPlanner`
- `ResultAggregator`
- `FormatParsers`

收益：

- 风险最低
- 为后续索引和 worker 池准备接口

### Phase 2: SQLite 索引与解析缓存

目标：解决重复解析问题。

交付：

- `ScanIndexStore`
- `file_inventory`
- `parse_cache`
- `scan_runs`

收益：

- 大幅减少重复读文件和重复解析

### Phase 3: NTFS Journal 增量发现

目标：解决目录遍历慢。

交付：

- NTFS checkpoint
- FRN/USN 身份模型
- journal 增量同步
- bootstrap 回退链路

收益：

- 从“每次全量遍历”切到“按变更发现”

### Phase 4: 长生命周期解析 worker 池

目标：解决重格式解析慢和每文件 spawn 成本高。

交付：

- `ParserSupervisor`
- lane 化 worker 池
- timeout / restart / crash recovery

收益：

- 单文件慢解析不会拖垮全局吞吐

### Phase 5: 指标化与调优

目标：让性能优化进入可量化状态。

交付：

- 扫描分阶段指标
- extension 级耗时统计
- 缓存命中率报表
- 回归基准数据

收益：

- 后续每次优化都有证据支持

## 15. 终极优化路线图

这里的“终极优化”不是一次性全做，而是按成熟度递进。

### Roadmap 0: 当前已完成的基础保护

- 文本类扩展范围扩充
- 文件大小门禁
- 单文件超时
- 大小写不敏感扩展名匹配

这是保底层，不是终局。

### Roadmap 1: 中期可交付目标

目标：从单类扫描器升级到可维护扫描子系统。

标志：

- 发现 / 规划 / 解析 / 聚合解耦
- SQLite 索引可用
- 基本缓存命中能力可用
- 超时与失败有稳定错误模型

### Roadmap 2: 高性能稳定版

目标：让日常扫描主要消耗在“增量变化”上，而不是“目录规模”上。

标志：

- NTFS Journal 增量发现上线
- 绝大多数扫描不再触发全量递归遍历
- 重格式解析进入 worker 池
- 扫描级指标稳定可观测

### Roadmap 3: 终极工程化版本

目标：让扫描器具备长期演进能力。

标志：

- 多扫描根目录支持
- parser_profile 版本治理
- 扫描结果可追溯
- 自动基准回归
- 解析器能力可插拔

到这个阶段，扫描器已经不再只是“一个服务类”，而是“一个有索引、调度、缓存、观测和恢复能力的本地扫描平台”。

## 16. 成功标准

这份方案落地后，至少应满足以下结果：

1. 大目录下的扫描时间不再与全目录规模线性绑定
2. 未变化文件默认不再重复解析
3. PDF / Excel / DOCX / PPTX 慢文件不会拖垮整次扫描
4. 强一致要求能在 NTFS 主路径下成立
5. 扫描性能可以按阶段和按格式被量化分析
6. 上层 CLI 不需要感知内部重构细节

## 17. 非目标

这次方案明确不追求以下事情：

- 非 Windows 主路径下做到同等上限
- 直接重写所有 parser 库
- 把扫描结果改成搜索引擎或向量数据库
- 一次性引入分布式扫描
- 为了“更优雅”牺牲现有接口兼容性

## 18. 目标 / 输入 / 输出 / 伪代码草案 / 风险点

### 目标

把当前单类扫描器重构成一个面向 NTFS 本地工作目录的高一致、高性能扫描子系统，优先解决目录发现慢和重格式解析慢的问题。

### 输入

- 扫描根目录
- 日期范围 / summary_mode
- 扩展名白名单、忽略模式、大小门限、字符预算、解析 profile
- 上次扫描 checkpoint

### 输出

- 与现有 `ScanResult` 兼容的扫描结果
- 可复用的解析缓存
- 扫描统计与性能指标

### 伪代码草案

```python
# 目标：
# - 先用 NTFS 增量发现变化文件
# - 再决定哪些文件需要真正重解析
# - 把轻文件和重文件走不同执行路径

def run_scan(scan_request, runtime_config, dependencies):
    # 1. 发现变化文件
    # 为什么这样做：避免每次都对整个目录递归遍历
    discovery_result = dependencies.discovery_service.collect_changes(
        roots=scan_request.roots,
        last_checkpoint=dependencies.index_store.last_checkpoint(),
    )

    # 2. 如果 journal 不可用，则做强一致回退
    if discovery_result.requires_full_resync:
        inventory_snapshot = dependencies.discovery_service.bootstrap_full_scan(
            scan_request.roots
        )
        dependencies.index_store.replace_inventory(inventory_snapshot)
    else:
        dependencies.index_store.apply_changes(discovery_result.changes)

    # 3. 生成本次候选文件
    # 为什么单独做 planner：让“发现逻辑”和“业务扫描命中逻辑”分开
    candidates = dependencies.planner.build_candidates(
        inventory=dependencies.index_store.query_inventory(scan_request.date_range),
        runtime_config=runtime_config,
    )

    # 4. 拆分缓存命中与重解析任务
    reused_results = []
    parse_tasks = []
    for candidate in candidates:
        if dependencies.index_store.has_fresh_cache(
            candidate.file_identity,
            candidate.parser_profile,
        ):
            reused_results.append(
                dependencies.index_store.load_cache(candidate.file_identity)
            )
        else:
            parse_tasks.append(candidate)

    # 5. 轻文本和重格式分开执行
    fast_tasks, heavy_tasks = split_tasks(parse_tasks)
    fast_results = parse_fast_text_files(fast_tasks, runtime_config)
    heavy_results = dependencies.parser_supervisor.run(heavy_tasks, runtime_config)

    # 6. 回写缓存和统计
    all_results = reused_results + fast_results + heavy_results
    dependencies.index_store.save_parse_results(all_results)
    dependencies.index_store.save_scan_metrics(all_results, discovery_result)

    # 7. 聚合成兼容输出
    return dependencies.result_aggregator.build_scan_result(
        all_results,
        total_max_chars=runtime_config.total_max_chars,
    )
```

### 风险点 / 边界条件

- NTFS Journal checkpoint 丢失时必须触发全量重建，不能静默降级
- worker 池若设计不当，可能引入资源泄漏
- parser_profile 必须纳入缓存键，否则会出现“文件没变但输出规则已变”的脏缓存
- 若后续需要兼容网络盘 / 同步盘，必须设计明确的退化实现，而不是假装与 NTFS 等价

