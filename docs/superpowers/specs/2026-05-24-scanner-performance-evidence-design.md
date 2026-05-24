# Scanner Performance Evidence Design

Status: APPROVED
Mode: Builder

## 目标

为现有 scanner 增加一套可复核的性能证据层，先证明慢点位于目录发现、库存与缓存判断、文件解析还是结果聚合，再决定是否继续做 NTFS Journal、常驻 worker 池，或局部引入 Rust / Go。

本次不改变 daily / weekly / monthly 的上层调用契约，不改变 `ScanResult` / `FileContext` 的业务语义，不引入 Rust / Go。

## 输入

- 扫描日期范围：`start_date` / `end_date`
- 扫描模式：`summary_mode`
- 现有 scanner 配置：扩展名、忽略规则、并发数、解析预算、索引库路径
- 当前 SQLite `scan_index.sqlite3`
- 可选 benchmark 输出路径：JSON 和 Markdown

## 输出

- 生产链路每次扫描的阶段耗时：
  - `total_duration_ms`
  - `discovery_duration_ms`
  - `inventory_cache_duration_ms`
  - `parse_duration_ms`
  - `aggregation_duration_ms`
- 生产链路每次扫描的结果计数：
  - `discovered_count`
  - `reused_count`
  - `reparsed_count`
  - `success_count`
  - `error_count`
  - `timeout_count`
- 按扩展名统计的重解析指标：
  - `extension`
  - `file_count`
  - `parse_duration_ms`
  - `success_count`
  - `error_count`
  - `timeout_count`
- `scripts/benchmark_scanner.py` 输出的 JSON / Markdown benchmark 报告

## 设计原则

1. 证据优先。先量化阶段耗时和扩展名分布，再决定后续优化方向。
2. 保持兼容。`ScanResult` 不新增字段，避免影响 report 生成链路。
3. 指标持久化。SQLite 保存结构化指标，日志输出中文摘要，benchmark 脚本读取同一套指标。
4. 口径明确。扩展名指标只统计本轮实际重解析文件，不把 cache reuse 误算成解析耗时。
5. 失败可审计。timeout 和 parser error 进入计数与扩展名明细，不静默吞掉。

## 架构

新增一个轻量指标模块：

- `src/services/scan_metrics.py`
  - `ScanRunMetrics`：保存单次扫描阶段耗时和结果计数
  - `ExtensionMetrics`：保存单扩展名重解析数量、耗时、成功、失败、超时
  - `ScanMetricsCollector`：负责计时、累加扩展名指标、生成摘要

扩展现有存储层：

- `src/services/scan_index_store.py`
  - 兼容迁移 `scan_runs` 表新增列
  - 新增 `scan_extension_metrics` 表
  - `save_scan_run_metrics()` 继续兼容旧入参，并支持完整指标与扩展名明细
  - `latest_scan_run()` 保持旧返回值，避免破坏现有测试和调用方
  - 新增 `latest_scan_run_detail()` 和 `list_extension_metrics(run_id)`

接入现有编排层：

- `src/services/file_scanner.py`
  - 在 `scan_files()` 内包裹 discovery、inventory/cache、parse、aggregation 阶段
  - 对未缓存文件的解析任务记录每个文件的解析耗时和结果
  - 扫描结束时把完整指标写入 SQLite，并输出一行中文指标摘要

新增 benchmark 入口：

- `scripts/benchmark_scanner.py`
  - 调用真实 `FileScanner.scan_files()`，不绕开生产链路
  - 读取最新 scan run detail 与 extension metrics
  - 支持 stdout JSON、`--json-out` 和 `--markdown-out`

## 非目标

- 不重写 scanner 为 Rust 或 Go
- 不接入 NTFS Journal
- 不把每文件 `spawn` 改成长生命周期 worker 池
- 不改变 parser 输出文本
- 不改 weekly / daily report prompt 和 template
- 不把 benchmark 结果直接送入 LLM

## 风险点 / 边界条件

- 阶段耗时是 wall clock 口径，多线程解析阶段的 `parse_duration_ms` 表示整段解析墙钟时间；扩展名明细是每个 worker 内部解析耗时累加，两者不要求相等。
- 旧数据库可能已经存在 `scan_runs` 表，必须用 `ALTER TABLE ... ADD COLUMN` 做兼容迁移，不能删表重建。
- benchmark 脚本必须显式 UTF-8 写文件，Markdown 输出避免 Excel/WPS 乱码不是本次目标。
- 若扫描为空，也要写入一条完整指标记录，避免 latest 指标停留在上一轮。

## 伪代码草案

```python
# [伪代码草案]
# 目标：对真实 scanner 链路分阶段计时，并把证据写入 SQLite 和 benchmark 报告
# 输入：
# - scan_request: start_date、end_date、summary_mode
# - scanner: 现有 FileScanner 实例
# - metrics_store: 现有 ScanIndexStore
# 输出：
# - scan_result: 兼容现有上层调用的 ScanResult
# - run_metrics: 可落库、可 benchmark 读取的结构化指标

def scan_files_with_metrics(scan_request, scanner, metrics_store):
    # 1. 初始化 collector：从最外层开始计时，贴近用户体感总耗时
    metrics = ScanMetricsCollector.start()

    # 2. discovery 阶段：只负责目录遍历与文件候选发现
    with metrics.measure_stage("discovery"):
        discovered_files = scanner.discovery_service.bootstrap_full_scan(
            scan_request.start_date,
            scan_request.end_date,
        )

    # 3. inventory/cache 阶段：库存快照、缓存命中判断和 planner 分流
    with metrics.measure_stage("inventory_cache"):
        metrics.set_discovered_count(len(discovered_files))
        scanner.scan_index_store.replace_inventory(discovered_files)
        inventory_items = scanner.scan_index_store.query_inventory(...)
        plan = scanner.scan_planner.plan_candidates(...)
        metrics.set_plan_counts(
            reused_count=len(plan.cached),
            reparsed_count=len(plan.uncached),
        )

    # 4. parse 阶段：只处理未命中缓存的文件
    with metrics.measure_stage("parse"):
        for item in plan.uncached:
            # 为什么每个文件单独计时：后续才能判断具体是哪类扩展名拖慢
            file_timer = Timer.start()
            context = scanner._extract_content_with_timeout(item.path, limits)
            metrics.record_extension_result(
                extension=item.extension,
                duration_ms=file_timer.elapsed_ms(),
                success=context.error is None,
                timeout=is_timeout_error(context.error),
            )

    # 5. aggregation 阶段：合并缓存和新解析结果，应用全局字符预算
    with metrics.measure_stage("aggregation"):
        scan_result = scanner.aggregator.build_result(...)
        metrics.set_result_counts(
            success_count=scan_result.success_count,
            error_count=scan_result.error_count,
        )

    # 6. 持久化指标：保持旧 latest_scan_run 兼容，同时写入完整 detail 和扩展名明细
    run_metrics = metrics.finish()
    metrics_store.save_scan_run_metrics(run_metrics)
    logger.info(run_metrics.to_summary_line())

    return scan_result
```
