"""测试扫描聚合边界。"""

from src.models.schemas import FileContext
from src.services.scan_aggregator import ScanAggregator


def test_global_limit_replaces_first_over_budget_success_context():
    """首个超出全局预算的成功上下文应被占位文本替换。"""
    aggregator = ScanAggregator(total_max_chars=5)

    aggregator.add_context(
        FileContext(
            file_path="first.txt",
            file_type=".txt",
            content="123456",
            error=None,
        )
    )

    assert aggregator.contexts[0].content == "(已达全局字符上限，内容省略)"
    assert aggregator.contexts[0].error is None


def test_global_limit_placeholder_preserves_parser_metadata():
    """全局省略占位上下文应保留解析后端和截断元数据。"""
    aggregator = ScanAggregator(total_max_chars=5)

    aggregator.add_context(
        FileContext(
            file_path="first.log",
            file_type=".log",
            content="123456",
            error=None,
            parser_backend="light_text_v1",
            truncated=True,
        )
    )

    stored_context = aggregator.contexts[0]
    assert stored_context.content == "(已达全局字符上限，内容省略)"
    assert stored_context.parser_backend == "light_text_v1"
    assert stored_context.truncated is True


def test_error_context_preserved_after_global_limit():
    """进入全局省略后，错误上下文仍应保留原始错误信息。"""
    aggregator = ScanAggregator(total_max_chars=5)
    aggregator.add_context(
        FileContext(
            file_path="first.txt",
            file_type=".txt",
            content="123456",
            error=None,
        )
    )

    aggregator.add_context(
        FileContext(
            file_path="broken.txt",
            file_type=".txt",
            content="",
            error="parse failed",
        )
    )

    assert aggregator.contexts[1].content == ""
    assert aggregator.contexts[1].error == "parse failed"


def test_build_result_preserves_counts():
    """聚合结果应保持 success/error/total_files 一致。"""
    aggregator = ScanAggregator(total_max_chars=100)
    aggregator.add_context(
        FileContext(
            file_path="ok.txt",
            file_type=".txt",
            content="ok",
            error=None,
        )
    )
    aggregator.add_exception(file_path="broken.txt", error=RuntimeError("boom"))

    result = aggregator.build_result(total_files=2)

    assert result.total_files == 2
    assert result.success_count == 1
    assert result.error_count == 1
    assert len(result.contexts) == 2


def test_add_cached_context_reuses_aggregation_logic():
    """缓存上下文入口应复用相同的预算和计数逻辑。"""
    aggregator = ScanAggregator(total_max_chars=5)

    aggregator.add_cached_context(
        FileContext(
            file_path="cached.txt",
            file_type=".txt",
            content="123456",
            error=None,
        )
    )

    assert aggregator.success_count == 1
    assert aggregator.contexts[0].content == "(已达全局字符上限，内容省略)"
