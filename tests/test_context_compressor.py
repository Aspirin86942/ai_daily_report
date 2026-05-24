"""测试 context compressor 的确定性上下文输出。"""

from pathlib import Path

from src.models.schemas import FileContext, ScanResult
from src.services.context_compressor import (
    ACTION_COMPRESS,
    ACTION_ERROR,
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


def test_final_budget_trim_preserves_included_evidence_when_omitted_summary_overflows() -> None:
    """省略摘要过长时不能把已纳入的正文整体降级成“预算不足”。"""
    profile = ContextProfile.for_report_mode("weekly")
    profile = profile.with_budget(global_context_max_chars=2500, per_file_max_chars=500)
    compressor = ContextCompressor()
    keep_context = FileContext(
        file_path="D:/work/keep.md",
        file_type=".md",
        content="KEEP_ME weekly evidence",
        parser_backend="light_text_v1",
    )
    omitted_contexts = [
        FileContext(
            file_path=f"D:/work/omitted-{index:03d}.log",
            file_type=".log",
            content="omitted evidence",
            parser_backend="light_text_v1",
        )
        for index in range(80)
    ]
    scan_result = ScanResult(
        total_files=81,
        success_count=81,
        error_count=0,
        contexts=[keep_context, *omitted_contexts],
    )

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[
            _decision(
                "D:/work/keep.md",
                ACTION_KEEP,
                "small_file_keep",
                input_chars=len(keep_context.content),
            ),
            *[
                _decision(
                    context.file_path,
                    ACTION_OMIT,
                    "low_priority",
                    input_chars=len(context.content),
                )
                for context in omitted_contexts
            ],
        ],
        profile=profile,
    )

    assert len(compressed.content) <= profile.global_context_max_chars
    assert "KEEP_ME weekly evidence" in compressed.content
    assert compressed.included_file_count == 1
    assert compressed.omitted_file_count == 80
    assert compressed.output_chars > 1000


def test_truncated_omitted_file_still_counts_as_truncated() -> None:
    """truncated 是源解析事实，即使正文被省略也必须进入审计计数。"""
    profile = ContextProfile.for_report_mode("weekly")
    compressor = ContextCompressor()
    context = FileContext(
        file_path="D:/work/truncated.pdf",
        file_type=".pdf",
        content="text layer preview",
        parser_backend="pdf_text_v1",
        truncated=True,
    )
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[context],
    )

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[
            _decision(
                "D:/work/truncated.pdf",
                ACTION_OMIT,
                "low_priority",
                parser_backend="pdf_text_v1",
                input_chars=len(context.content),
            )
        ],
        profile=profile,
    )

    assert compressed.truncated_file_count == 1
    assert compressed.decisions[0].truncated is True


def test_compress_empty_scan_returns_auditable_empty_context() -> None:
    profile = ContextProfile.for_report_mode("monthly")
    compressor = ContextCompressor()
    scan_result = ScanResult(total_files=0, success_count=0, error_count=0, contexts=[])

    compressed = compressor.compress(scan_result=scan_result, decisions=[], profile=profile)

    assert "无文件证据" in compressed.content
    assert compressed.source_file_count == 0
    assert compressed.included_file_count == 0
    assert compressed.omitted_file_count == 0


def test_profile_contract_defaults_and_summary_fields() -> None:
    daily = ContextProfile.for_report_mode("daily")
    weekly = ContextProfile.for_report_mode("weekly")
    monthly = ContextProfile.for_report_mode("monthly")

    assert daily.compression_profile == "daily_balanced_v1"
    assert daily.global_context_max_chars == 50000
    assert daily.per_file_max_chars == 8000
    assert weekly.compression_profile == "weekly_balanced_v1"
    assert weekly.global_context_max_chars == 50000
    assert weekly.per_file_max_chars == 5000
    assert monthly.compression_profile == "monthly_balanced_v1"
    assert monthly.global_context_max_chars == 60000
    assert monthly.per_file_max_chars == 4000

    assert daily.to_profile_dict() == {
        "version": "context_scheduler_v1",
        "report_mode": "daily",
        "compression_profile": "daily_balanced_v1",
        "global_context_max_chars": 50000,
        "per_file_max_chars": 8000,
        "small_file_max_bytes": 64 * 1024,
        "medium_file_max_bytes": 1024 * 1024,
        "large_file_max_bytes": 10 * 1024 * 1024,
        "priority_policy": "default_v1",
        "compression_policy": "markdown_context_v1",
    }


def test_compressed_context_summary_exposes_plan_contract_fields() -> None:
    profile = ContextProfile.for_report_mode("daily")
    compressor = ContextCompressor()
    content = "daily evidence"
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[FileContext(file_path="D:/work/a.md", file_type=".md", content=content, parser_backend="light_text_v1")],
    )

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[_decision("D:/work/a.md", ACTION_KEEP, "small_file_keep", input_chars=len(content))],
        profile=profile,
    )
    summary = compressed.to_summary()

    assert compressed.input_chars == len(content)
    assert compressed.output_chars == len(compressed.content)
    assert compressed.truncated_file_count == 0
    assert compressed.warnings == []
    assert summary["input_chars"] == len(content)
    assert summary["output_chars"] == len(compressed.content)
    assert summary["compression_ratio"] == compressed.output_chars / compressed.input_chars


def test_compress_respects_decision_order_over_context_order() -> None:
    profile = ContextProfile.for_report_mode("weekly")
    compressor = ContextCompressor()
    contexts = [
        FileContext(file_path="D:/work/b.md", file_type=".md", content="B evidence", parser_backend="light_text_v1"),
        FileContext(file_path="D:/work/a.md", file_type=".md", content="A evidence", parser_backend="light_text_v1"),
    ]
    scan_result = ScanResult(total_files=2, success_count=2, error_count=0, contexts=contexts)

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[
            _decision("D:/work/a.md", ACTION_KEEP, "priority_first", priority=1, input_chars=10),
            _decision("D:/work/b.md", ACTION_KEEP, "priority_second", priority=2, input_chars=10),
        ],
        profile=profile,
    )

    assert compressed.content.index("D:/work/a.md") < compressed.content.index("D:/work/b.md")
    assert [decision.file_path for decision in compressed.decisions] == ["D:/work/a.md", "D:/work/b.md"]


def test_parse_issue_warning_respects_tiny_global_budget() -> None:
    profile = ContextProfile.for_report_mode("weekly")
    profile = profile.with_budget(global_context_max_chars=120, per_file_max_chars=40)
    compressor = ContextCompressor()
    scan_result = ScanResult(
        total_files=1,
        success_count=0,
        error_count=1,
        contexts=[
            FileContext(
                file_path="D:/work/bad.md",
                file_type=".md",
                content="",
                error="parser exploded with long diagnostic text",
                parser_backend="light_text_v1",
            )
        ],
    )

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[
            _decision(
                "D:/work/bad.md",
                ACTION_OMIT,
                "parse_error",
                input_chars=0,
            )
        ],
        profile=profile,
    )

    assert compressed.output_chars == len(compressed.content)
    assert len(compressed.content) <= profile.global_context_max_chars
    assert "预算不足" in compressed.content or any("预算不足" in warning for warning in compressed.warnings)


def test_compress_does_not_mutate_caller_decisions_when_omitting() -> None:
    profile = ContextProfile.for_report_mode("weekly")
    profile = profile.with_budget(global_context_max_chars=1300, per_file_max_chars=500)
    compressor = ContextCompressor()
    contexts = [
        FileContext(file_path="D:/work/a.md", file_type=".md", content="A" * 500, parser_backend="light_text_v1"),
        FileContext(file_path="D:/work/b.md", file_type=".md", content="B" * 500, parser_backend="light_text_v1"),
    ]
    first_decision = _decision("D:/work/a.md", ACTION_KEEP, "small_file_keep", input_chars=500)
    second_decision = _decision("D:/work/b.md", ACTION_KEEP, "small_file_keep", input_chars=500)
    scan_result = ScanResult(total_files=2, success_count=2, error_count=0, contexts=contexts)

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[first_decision, second_decision],
        profile=profile,
    )

    assert second_decision.action == ACTION_KEEP
    assert second_decision.reason == "small_file_keep"
    assert second_decision.output_chars == 0
    assert second_decision.truncated is False
    assert compressed.decisions[1].action == ACTION_OMIT
    assert compressed.decisions[1].reason == "global_budget_exceeded"


def test_compress_reports_missing_context_decision_as_error() -> None:
    profile = ContextProfile.for_report_mode("weekly")
    compressor = ContextCompressor()
    valid_context = FileContext(
        file_path="D:/work/a.md",
        file_type=".md",
        content="A evidence",
        parser_backend="light_text_v1",
    )
    scan_result = ScanResult(total_files=1, success_count=1, error_count=0, contexts=[valid_context])

    compressed = compressor.compress(
        scan_result=scan_result,
        decisions=[
            _decision("D:/work/a.md", ACTION_KEEP, "small_file_keep", input_chars=len(valid_context.content)),
            _decision("D:/work/missing.md", ACTION_KEEP, "small_file_keep", input_chars=10),
        ],
        profile=profile,
    )

    missing_decision = compressed.decisions[1]
    assert any("D:/work/missing.md" in warning for warning in compressed.warnings)
    assert compressed.error_file_count == 1
    assert missing_decision.action == ACTION_ERROR
    assert missing_decision.reason == "missing_context"
    assert "D:/work/missing.md" in compressed.content
    assert "missing_context" in compressed.content
