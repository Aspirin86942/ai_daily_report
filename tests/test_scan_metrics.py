"""测试 scanner 性能指标模型。"""

from src.services.scan_metrics import (
    ReparseDetail,
    ScanMetricsCollector,
    is_timeout_error,
)


def test_reparse_detail_serializes_stable_payload():
    """重解析明细应输出 benchmark 需要的稳定字段。"""
    detail = ReparseDetail(
        path="D:\\work\\report.md",
        extension=".md",
        file_identity="bootstrap:d:\\work\\report.md",
        source_version="mtime=2:size=10",
        cache_status="miss",
        cache_miss_reason="source_version_changed",
        previous_source_version="mtime=1:size=10",
        parse_duration_ms=12,
        parse_status="success",
        parse_error="",
    )

    assert detail.to_dict() == {
        "path": "D:\\work\\report.md",
        "extension": ".md",
        "file_identity": "bootstrap:d:\\work\\report.md",
        "source_version": "mtime=2:size=10",
        "cache_status": "miss",
        "cache_miss_reason": "source_version_changed",
        "previous_source_version": "mtime=1:size=10",
        "parse_duration_ms": 12,
        "parse_status": "success",
        "parse_error": "",
    }


def test_reparse_detail_normalizes_duration_and_none_version():
    """重解析明细应稳定保留 None，并把负耗时归零。"""
    detail = ReparseDetail(
        path="D:\\work\\report.md",
        extension=".md",
        file_identity="bootstrap:d:\\work\\report.md",
        source_version="mtime=2:size=10",
        cache_status="miss",
        cache_miss_reason="source_version_changed",
        previous_source_version=None,
        parse_duration_ms=-1,
    )

    payload = detail.to_dict()

    assert payload["parse_duration_ms"] == 0
    assert payload["previous_source_version"] is None


def test_metrics_collector_builds_scan_run_detail():
    """collector 应把阶段耗时和扫描计数汇总成稳定 run metrics。"""
    collector = ScanMetricsCollector()

    collector.record_stage_duration("discovery", 12)
    collector.record_stage_duration("inventory_cache", 8)
    collector.record_stage_duration("parse", 30)
    collector.record_stage_duration("aggregation", 4)
    collector.set_discovered_count(5)
    collector.set_plan_counts(reused_count=2, reparsed_count=3)
    collector.set_result_counts(success_count=2, error_count=1)
    metrics = collector.finish(total_duration_ms=60)

    assert metrics.total_duration_ms == 60
    assert metrics.discovery_duration_ms == 12
    assert metrics.inventory_cache_duration_ms == 8
    assert metrics.parse_duration_ms == 30
    assert metrics.aggregation_duration_ms == 4
    assert metrics.discovered_count == 5
    assert metrics.reused_count == 2
    assert metrics.reparsed_count == 3
    assert metrics.success_count == 2
    assert metrics.error_count == 1
    assert metrics.timeout_count == 0


def test_metrics_collector_aggregates_extension_results():
    """扩展名指标应按小写扩展名累加数量、耗时、失败和超时。"""
    collector = ScanMetricsCollector()

    collector.record_extension_result(".PDF", duration_ms=100, error=None)
    collector.record_extension_result(
        ".pdf",
        duration_ms=250,
        error="timeout: file parse exceeded 45s",
    )
    collector.record_extension_result(".xlsx", duration_ms=30, error="bad workbook")
    metrics = collector.finish(total_duration_ms=400)

    by_extension = {item.extension: item for item in metrics.extension_metrics}

    assert by_extension[".pdf"].file_count == 2
    assert by_extension[".pdf"].parse_duration_ms == 350
    assert by_extension[".pdf"].success_count == 1
    assert by_extension[".pdf"].error_count == 1
    assert by_extension[".pdf"].timeout_count == 1
    assert by_extension[".xlsx"].file_count == 1
    assert metrics.timeout_count == 1


def test_is_timeout_error_requires_timeout_prefix():
    """只有稳定 timeout 前缀进入 timeout 统计，普通异常不能混入。"""
    assert is_timeout_error("timeout: file parse exceeded 30s") is True
    assert is_timeout_error("TimeoutError from parser") is False
    assert is_timeout_error("bad workbook") is False
    assert is_timeout_error(None) is False


def test_scan_run_metrics_summary_line_is_stable():
    """摘要行应使用中文稳定字段名，便于日志中快速定位瓶颈。"""
    collector = ScanMetricsCollector()
    collector.record_stage_duration("discovery", 10)
    collector.record_stage_duration("inventory_cache", 20)
    collector.record_stage_duration("parse", 70)
    collector.record_stage_duration("aggregation", 5)
    collector.set_discovered_count(4)
    collector.set_plan_counts(reused_count=1, reparsed_count=3)
    collector.set_result_counts(success_count=2, error_count=1)
    collector.record_extension_result(
        ".pdf",
        duration_ms=70,
        error="timeout: file parse exceeded 45s",
    )

    metrics = collector.finish(total_duration_ms=120)

    assert metrics.to_summary_line() == (
        "扫描指标: 总耗时 120ms, discovery 10ms, inventory/cache 20ms, "
        "parse 70ms, aggregation 5ms, 发现 4 个, 缓存复用 1 个, "
        "重解析 3 个, 成功 2 个, 失败 1 个, 超时 1 个"
    )
