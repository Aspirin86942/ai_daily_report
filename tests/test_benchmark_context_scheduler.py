"""测试 context scheduler benchmark 脚本。"""

from pathlib import Path

from scripts.benchmark_context_scheduler import (
    build_benchmark_payload,
    build_context_scheduler_summary,
    render_markdown_report,
    write_report_files,
)
from src.services.context_compressor import CompressedContext


def _make_compressed_context() -> CompressedContext:
    compressed = CompressedContext.empty()
    compressed.source_file_count = 3
    compressed.included_file_count = 2
    compressed.omitted_file_count = 1
    compressed.metadata_only_count = 1
    compressed.compressed_file_count = 1
    compressed.error_file_count = 0
    compressed.truncated_file_count = 1
    compressed.input_chars = 1000
    compressed.output_chars = 250
    compressed.decisions = []
    return compressed


def test_build_context_scheduler_summary_counts_actions_and_backends() -> None:
    """summary 应直接暴露 CompressedContext 的压缩统计。"""
    compressed = _make_compressed_context()

    summary = build_context_scheduler_summary(compressed)

    assert summary["source_file_count"] == 3
    assert summary["compression_ratio"] == 0.25


def test_build_benchmark_payload_contains_context_summary() -> None:
    """benchmark payload 应包含 context run、scan run 和压缩摘要。"""
    compressed = _make_compressed_context()

    payload = build_benchmark_payload(
        compressed_context=compressed,
        context_run={"context_run_id": 7, "status": "success"},
        parameters={
            "start_date": "2026-05-09",
            "end_date": "2026-05-24",
            "report_mode": "weekly",
            "compression_profile": "weekly_balanced_v1",
        },
        scan_run={
            "run_id": 3,
            "discovered_count": 3,
            "reused_count": 1,
            "reparsed_count": 2,
        },
    )

    assert payload["context_run"] == {"context_run_id": 7, "status": "success"}
    assert payload["context_scheduler_summary"]["source_file_count"] == 3
    assert payload["parameters"]["report_mode"] == "weekly"
    assert payload["scan_run"]["run_id"] == 3


def test_render_markdown_report_mentions_context_summary() -> None:
    """Markdown 报告应包含 context scheduler 摘要段落和压缩率字段。"""
    payload = build_benchmark_payload(
        compressed_context=_make_compressed_context(),
        context_run={"context_run_id": 7, "status": "success"},
        parameters={
            "start_date": "2026-05-09",
            "end_date": "2026-05-24",
            "report_mode": "weekly",
            "compression_profile": "weekly_balanced_v1",
        },
        scan_run={
            "run_id": 3,
            "discovered_count": 3,
            "reused_count": 1,
            "reparsed_count": 2,
        },
    )

    markdown = render_markdown_report(payload)

    assert "# Context Scheduler Benchmark Report" in markdown
    assert "## Context Scheduler Summary" in markdown
    assert "compression_ratio" in markdown


def test_write_report_files_writes_json_and_markdown(tmp_path: Path) -> None:
    """脚本输出文件应显式用 UTF-8 写入 JSON 和 Markdown。"""
    payload = build_benchmark_payload(
        compressed_context=_make_compressed_context(),
        context_run={"context_run_id": 7, "status": "success"},
        parameters={
            "start_date": "2026-05-09",
            "end_date": "2026-05-24",
            "report_mode": "weekly",
            "compression_profile": "weekly_balanced_v1",
        },
        scan_run={
            "run_id": 3,
            "discovered_count": 3,
            "reused_count": 1,
            "reparsed_count": 2,
        },
    )
    json_out = tmp_path / "context_scheduler.json"
    markdown_out = tmp_path / "context_scheduler.md"

    write_report_files(
        payload,
        json_out=json_out,
        markdown_out=markdown_out,
    )

    assert '"context_scheduler_summary"' in json_out.read_text(encoding="utf-8")
    assert "# Context Scheduler Benchmark Report" in markdown_out.read_text(
        encoding="utf-8"
    )
