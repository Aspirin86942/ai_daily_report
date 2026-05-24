"""测试 context compressor 的确定性上下文输出。"""

from pathlib import Path

from src.models.schemas import FileContext, ScanResult
from src.services.context_compressor import (
    ACTION_COMPRESS,
    ACTION_KEEP,
    ACTION_METADATA_ONLY,
    ACTION_OMIT,
    ContextCompressor,
    ContextDecision,
    ContextProfile,
)


def _decision(
    path: str,
    action: str,
    reason: str,
    *,
    priority: int = 10,
    parser_backend: str = "light_text_v1",
    input_chars: int = 0,
) -> ContextDecision:
    return ContextDecision(
        file_path=path,
        extension=Path(path).suffix.lower(),
        size_bytes=123,
        parser_backend=parser_backend,
        worker_lane="direct",
        cache_status="fresh",
        action=action,
        reason=reason,
        priority=priority,
        input_chars=input_chars,
        output_chars=0,
        truncated=False,
        error=None,
    )


def test_compress_keeps_small_file_and_renders_audit_header() -> None:
    profile = ContextProfile.for_report_mode("daily")
    compressor = ContextCompressor()
    context = FileContext(
        file_path="D:/work/report.md",
        file_type=".md",
        content="# 今日工作\n完成 scanner 验证。",
        parser_backend="light_text_v1",
    )
    scan_result = ScanResult(total_files=1, success_count=1, error_count=0, contexts=[context])

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[_decision("D:/work/report.md", ACTION_KEEP, "small_file_keep", input_chars=len(context.content))],
        profile=profile,
    )

    assert "# 文件证据上下文" in compressed.content
    assert "## 本轮摘要" in compressed.content
    assert "## 文件证据" in compressed.content
    assert "D:/work/report.md" in compressed.content
    assert "# 今日工作" in compressed.content
    assert compressed.source_file_count == 1
    assert compressed.included_file_count == 1
    assert compressed.omitted_file_count == 0
    assert compressed.output_chars == len(compressed.content)


def test_compress_limits_single_large_file_by_per_file_budget() -> None:
    profile = ContextProfile.for_report_mode("weekly")
    profile = profile.with_budget(global_context_max_chars=5000, per_file_max_chars=80)
    compressor = ContextCompressor()
    content = "A" * 300
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[FileContext(file_path="D:/work/large.md", file_type=".md", content=content, parser_backend="light_text_v1")],
    )

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[_decision("D:/work/large.md", ACTION_COMPRESS, "medium_text_compress", input_chars=len(content))],
        profile=profile,
    )

    assert "内容已按单文件预算截断" in compressed.content
    assert "A" * 80 in compressed.content
    assert "A" * 120 not in compressed.content
    assert compressed.compressed_file_count == 1


def test_compress_renders_metadata_only_without_body_content() -> None:
    profile = ContextProfile.for_report_mode("weekly")
    compressor = ContextCompressor()
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[FileContext(file_path="D:/work/huge.xlsx", file_type=".xlsx", content="secret body should not enter prompt", parser_backend="office_v1", truncated=True)],
    )

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[_decision("D:/work/huge.xlsx", ACTION_METADATA_ONLY, "file_size_policy", parser_backend="office_v1", input_chars=35)],
        profile=profile,
    )

    assert "huge.xlsx" in compressed.content
    assert "metadata_only" in compressed.content
    assert "file_size_policy" in compressed.content
    assert "secret body should not enter prompt" not in compressed.content
    assert compressed.metadata_only_count == 1


def test_compress_moves_over_budget_files_to_omitted_summary() -> None:
    profile = ContextProfile.for_report_mode("weekly")
    profile = profile.with_budget(global_context_max_chars=1300, per_file_max_chars=500)
    compressor = ContextCompressor()
    contexts = [
        FileContext(file_path="D:/work/a.md", file_type=".md", content="A" * 500, parser_backend="light_text_v1"),
        FileContext(file_path="D:/work/b.md", file_type=".md", content="B" * 500, parser_backend="light_text_v1"),
    ]
    scan_result = ScanResult(total_files=2, success_count=2, error_count=0, contexts=contexts)

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[
            _decision("D:/work/a.md", ACTION_KEEP, "small_file_keep", input_chars=500),
            _decision("D:/work/b.md", ACTION_KEEP, "small_file_keep", input_chars=500),
        ],
        profile=profile,
    )

    assert "D:/work/a.md" in compressed.content
    assert "## 省略文件摘要" in compressed.content
    assert "D:/work/b.md" in compressed.content
    assert compressed.included_file_count == 1
    assert compressed.omitted_file_count == 1
    assert compressed.decisions[1].action == ACTION_OMIT
    assert compressed.decisions[1].reason == "global_budget_exceeded"


def test_compress_empty_scan_returns_auditable_empty_context() -> None:
    profile = ContextProfile.for_report_mode("monthly")
    compressor = ContextCompressor()
    scan_result = ScanResult(total_files=0, success_count=0, error_count=0, contexts=[])

    compressed = compressor.compress(scan_result=scan_result, decisions=[], profile=profile)

    assert "无文件证据" in compressed.content
    assert compressed.source_file_count == 0
    assert compressed.included_file_count == 0
    assert compressed.omitted_file_count == 0
