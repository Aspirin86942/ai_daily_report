# Cold Scanner Run Module Design

Status: IMPLEMENTED
Mode: improve-codebase-architecture + TDD
Date: 2026-06-11

## 目标

把 **Cold scanner run** 的生命周期顺序从 `FileScanner.scan_files()` 收进一个 deep Module: `ColdScannerRun`。

这个 Module 负责一轮 scanner run 的顺序知识:

- 默认日期范围。
- runtime state reset。
- discovery stage。
- inventory/cache stage。
- parse planning。
- cached context aggregation。
- uncached parse execution。
- parse cache write-back。
- reparse detail recording。
- extension metrics recording。
- final aggregation consistency check。
- scan run metrics persistence。
- `scan_run_id` 回填到 `ScanResult`。

`FileScanner` 保留 parser behavior、文件类型解析、worker lane、Office fallback、parse cache helper 和兼容旧测试的 item helper。当前阶段让 `FileScanner.scan_files()` 成为 thin Adapter，委托给 `ColdScannerRun`。

相关 glossary:

- `Cold scanner run`
- `Rust CLI JSON contract`
- `Hybrid Office fallback policy`

## 输入

`ColdScannerRun` 接收一个 scanner Adapter。当前唯一 Adapter 是 `FileScanner`。

Adapter 必须提供:

- `work_dir`
- `scanner_cfg`
- `discovery_service`
- `scan_planner`
- `scan_index_store`
- `last_reparse_details`
- `_office_parse_audits`
- `_normalize_discovered_files()`
- `_get_cached_contexts()`
- `_extract_uncached_content_with_duration()`
- `_record_reparse_detail()`
- `_write_parse_cache()`
- `_record_reparse_exception()`
- `_item_path()`
- `_item_identity()`
- `_item_extension()`
- `_item_source_version()`

`scan_files()` 输入:

- `start_date: date | None`
- `end_date: date | None`
- `summary_mode: bool`

## 输出

成功输出:

- `ScanResult`
- `ScanResult.scan_run_id` 必须是本轮新写入的 `scan_runs.run_id`
- `ScanResult.total_files == success_count + error_count`
- `scan_index_store.save_scan_run_metrics()` 被调用一次
- `FileScanner.last_reparse_details` 只包含本轮重解析明细

失败输出:

- 单文件 parser exception 被转为文件级 error context。
- 单文件 parser exception 写入 parse cache error。
- 单文件 parser exception 进入 reparse detail。
- 整轮 scanner 不因为单文件 parser exception 中断。

## 非目标

- 不改 parser behavior。
- 不改 Office fallback policy。
- 不改 Rust CLI JSON contract。
- 不改 scan cache schema。
- 不改 benchmark schema。
- 不改 `ScanPlanner` parser profile。
- 不把 `FileScanner` 的所有 helper 一次性搬空。
- 不新增第二个 scanner Adapter。

## 当前实现

新增 Module:

- `src/services/cold_scanner_run.py`

当前 Interface:

```python
class ColdScannerRun:
    def __init__(self, scanner: Any) -> None: ...

    def scan_files(
        self,
        start_date: date | None = None,
        end_date: date | None = None,
        summary_mode: bool = False,
    ) -> ScanResult: ...
```

`FileScanner.scan_files()` 当前只保留外部兼容签名，并委托:

```python
return ColdScannerRun(self).scan_files(
    start_date=start_date,
    end_date=end_date,
    summary_mode=summary_mode,
)
```

## 测试策略

新增测试:

- `tests/test_cold_scanner_run.py`

覆盖:

- empty run 会清空本轮 runtime state。
- empty run 会 replace 空 inventory。
- empty run 会保存 scan metrics。
- empty run 会回填 `scan_run_id`。
- cached / uncached run 会执行 inventory/cache/plan/parse/aggregation 顺序。
- cached context 先进入 aggregator。
- uncached context 会记录 reparse detail 并写回 parse cache。
- parser profile limits 会排除 `total_max_chars` 和 `summary_mode` 后传给 parser。

保留现有测试:

- `tests/test_file_scanner.py`
- `tests/test_scan_metrics.py`
- `tests/test_scan_index_store.py`
- `tests/test_scan_planner.py`
- `tests/test_scan_aggregator.py`

推荐验证:

```powershell
conda run -n test python -m pytest tests/test_cold_scanner_run.py tests/test_file_scanner.py tests/test_scan_metrics.py tests/test_scan_index_store.py tests/test_scan_planner.py tests/test_scan_aggregator.py -q
conda run -n test python -m pytest tests -q
conda run -n test python -m compileall main.py src tests
```

## 风险点 / 边界条件

- 当前 Adapter Interface 仍较宽，因为 parser behavior 和 item helper 还留在 `FileScanner`。这是有意的第一步，避免在同一改动里重写 parser policy。
- `ThreadPoolExecutor` 被移动到 `ColdScannerRun`，但 worker callable 仍来自 `FileScanner`。
- `ScanAggregator` 被移动到 `ColdScannerRun` 使用，但聚合规则未改变。
- 空扫描必须继续覆盖 inventory，否则后续计划可能读取旧发现快照。
- `scan_run_id` 必须从本轮 `save_scan_run_metrics()` 返回值回填，不能读取 latest。
- `last_reparse_details` 和 `_office_parse_audits` 必须在每轮 run 开始清空，避免 benchmark evidence 串轮。
- 单文件 exception 必须进入 parse cache、reparse detail、aggregator 三处审计载体。

## 伪代码草案

```python
# [伪代码草案]
# 目标：把 scanner run 生命周期顺序集中到 ColdScannerRun，
#       让 FileScanner.scan_files() 只保留外部调用入口。
#
# 输入：
# - scanner: FileScanner Adapter，提供 discovery、planner、store、parser helper
# - start_date/end_date: 本轮日期范围；缺省时使用昨日到今日
# - summary_mode: 是否使用缩减 parser profile
#
# 输出：
# - ScanResult: 包含 contexts、success/error counts 和本轮 scan_run_id
# - scan_runs: 写入本轮完整 metrics
# - parse_cache/reparse_details: 仅对 uncached parser 路径写入

def scan_files(start_date=None, end_date=None, summary_mode=False):
    # 1. 默认日期在 run Module 里统一处理，避免多个入口产生不同日期口径
    if start_date is None:
        start_date = today() - one_day()
    if end_date is None:
        end_date = today()

    metrics = ScanMetricsCollector.start()

    # 2. 每轮开始先清空 runtime evidence，避免 benchmark 和 audit 串轮
    scanner.last_reparse_details = []
    scanner._office_parse_audits = {}

    with metrics.measure_stage("discovery"):
        discovered = scanner.normalize(
            scanner.discovery_service.bootstrap_full_scan(start_date, end_date)
        )
    metrics.set_discovered_count(len(discovered))

    if not discovered:
        with metrics.measure_stage("inventory_cache"):
            # 空扫描也要写入空 inventory，防止复用上一轮发现快照
            scanner.scan_index_store.replace_inventory([])
            metrics.set_plan_counts(reused_count=0, reparsed_count=0)

        result = ScanResult(total_files=0, success_count=0, error_count=0, contexts=[])
        metrics.set_result_counts(0, 0)
        return persist_metrics_and_attach_run_id(result, metrics)

    with metrics.measure_stage("inventory_cache"):
        profile = scanner.scan_planner.build_parser_profile(summary_mode)
        profile_key = scanner.scan_planner.serialize_parser_profile(profile)

        # 3. inventory 是后续 cache freshness 和 planning 的统一快照
        scanner.scan_index_store.replace_inventory(discovered_to_inventory_rows(discovered))
        inventory_items = scanner.scan_index_store.query_inventory(start_date, end_date)
        cache_probes = probe_each_inventory_item(inventory_items, profile_key)
        planned = scanner.scan_planner.plan_candidates(
            candidates=inventory_items,
            start_date=start_date,
            end_date=end_date,
            cache_lookup=cache_status_to_lookup(cache_probes),
        )
        metrics.set_plan_counts(
            reused_count=len(planned["cached"]),
            reparsed_count=len(planned["uncached"]),
        )
        limits = parser_limits_without_run_only_keys(profile)
        cached_contexts = scanner._get_cached_contexts(planned["cached"], profile_key)

    aggregator = ScanAggregator(profile["total_max_chars"])
    add_cached_contexts(aggregator, planned["cached"], cached_contexts)

    with metrics.measure_stage("parse"):
        for item in parallel(planned["uncached"]):
            try:
                context, duration_ms = scanner._extract_uncached_content_with_duration(
                    item,
                    limits,
                )
                metrics.record_extension_result(item.extension, duration_ms, context.error)
                scanner._record_reparse_detail(item, cache_probes[item.id], duration_ms, context)
                scanner._write_parse_cache(item, profile_key, context)
                aggregator.add_context(context)
            except Exception as exc:
                # 单文件失败进入三处审计载体，整轮 run 继续
                metrics.record_extension_result(item.extension, 0, str(exc))
                scanner.scan_index_store.upsert_parse_cache(..., parse_status="error")
                scanner._record_reparse_exception(item, cache_probes[item.id], str(exc))
                aggregator.add_exception(item.path, exc)

    with metrics.measure_stage("aggregation"):
        assert aggregator.success_count + aggregator.error_count == planned["total_candidates"]
        result = aggregator.build_result(planned["total_candidates"])

    metrics.set_result_counts(result.success_count, result.error_count)
    return persist_metrics_and_attach_run_id(result, metrics)
```

