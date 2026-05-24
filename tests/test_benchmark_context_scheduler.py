"""测试 context scheduler benchmark 脚本。"""

from datetime import date
from types import SimpleNamespace
from pathlib import Path

import pytest

import scripts.benchmark_context_scheduler as benchmark_module
from scripts.benchmark_context_scheduler import (
    build_benchmark_payload,
    build_context_scheduler_summary,
    render_markdown_report,
    run_benchmark,
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


def _make_args(compression_profile: str | None = None) -> SimpleNamespace:
    return SimpleNamespace(
        start_date=date(2026, 5, 9),
        end_date=date(2026, 5, 24),
        report_mode="weekly",
        compression_profile=compression_profile,
    )


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


def test_run_benchmark_binds_to_returned_context_run_id(monkeypatch) -> None:
    """benchmark 应按本次 context_run_id 取数，而不是读取全局 latest。"""
    compressed = _make_compressed_context()

    class StubScheduler:
        def build_context(self, request) -> SimpleNamespace:
            return SimpleNamespace(
                context_run_id=7,
                compressed_context=compressed,
            )

    class StubStore:
        def latest_context_run(self) -> dict[str, object]:
            return {
                "context_run_id": 99,
                "scan_run_id": 88,
                "compression_profile": "wrong_latest_v1",
                "status": "success",
            }

        def latest_scan_run_detail(self) -> dict[str, int]:
            return {
                "run_id": 88,
                "discovered_count": 88,
                "reused_count": 0,
                "reparsed_count": 88,
            }

        def get_context_run(self, context_run_id: int) -> dict[str, object]:
            assert context_run_id == 7
            return {
                "context_run_id": 7,
                "scan_run_id": 3,
                "compression_profile": "weekly_balanced_v1",
                "status": "success",
            }

        def get_scan_run_detail(self, run_id: int) -> dict[str, int]:
            assert run_id == 3
            return {
                "run_id": 3,
                "discovered_count": 3,
                "reused_count": 1,
                "reparsed_count": 2,
            }

    class StubFileScanner:
        def __init__(self) -> None:
            self.scan_index_store = StubStore()

    monkeypatch.setattr(benchmark_module, "ContextScheduler", StubScheduler)
    monkeypatch.setattr(benchmark_module, "FileScanner", StubFileScanner)

    payload = run_benchmark(_make_args())

    assert payload["context_run"]["context_run_id"] == 7
    assert payload["scan_run"]["run_id"] == 3


def test_run_benchmark_skips_context_lookup_when_context_run_id_missing(
    monkeypatch,
) -> None:
    """没有 context_run_id 时不应回退读取任意 latest context run。"""
    compressed = _make_compressed_context()

    class StubScheduler:
        def build_context(self, request) -> SimpleNamespace:
            return SimpleNamespace(
                context_run_id=None,
                compressed_context=compressed,
            )

    class StubStore:
        def latest_context_run(self) -> dict[str, object]:
            return {
                "context_run_id": 99,
                "scan_run_id": 88,
                "compression_profile": "wrong_latest_v1",
                "status": "success",
            }

        def get_context_run(self, context_run_id: int) -> dict[str, object]:
            raise AssertionError("should not query context run without id")

        def get_scan_run_detail(self, run_id: int) -> dict[str, int]:
            raise AssertionError("should not query scan run without context run")

    class StubFileScanner:
        def __init__(self) -> None:
            self.scan_index_store = StubStore()

    monkeypatch.setattr(benchmark_module, "ContextScheduler", StubScheduler)
    monkeypatch.setattr(benchmark_module, "FileScanner", StubFileScanner)

    payload = run_benchmark(_make_args(compression_profile="manual_profile_v1"))

    assert payload["context_run"] == {}
    assert payload["scan_run"] == {}
    assert payload["parameters"]["compression_profile"] == "manual_profile_v1"


def test_run_benchmark_allows_missing_scan_run_id(monkeypatch) -> None:
    """context run 没有关联 scan_run_id 时应输出空 scan_run。"""
    compressed = _make_compressed_context()

    class StubScheduler:
        def build_context(self, request) -> SimpleNamespace:
            return SimpleNamespace(
                context_run_id=7,
                compressed_context=compressed,
            )

    class StubStore:
        def get_context_run(self, context_run_id: int) -> dict[str, object]:
            return {
                "context_run_id": context_run_id,
                "scan_run_id": None,
                "compression_profile": "weekly_balanced_v1",
                "status": "success",
            }

        def get_scan_run_detail(self, run_id: int) -> dict[str, int]:
            raise AssertionError("should not query scan run without id")

    class StubFileScanner:
        def __init__(self) -> None:
            self.scan_index_store = StubStore()

    monkeypatch.setattr(benchmark_module, "ContextScheduler", StubScheduler)
    monkeypatch.setattr(benchmark_module, "FileScanner", StubFileScanner)

    payload = run_benchmark(_make_args())

    assert payload["context_run"]["context_run_id"] == 7
    assert payload["scan_run"] == {}


def test_run_benchmark_allows_missing_scan_run_row(monkeypatch) -> None:
    """scan_run_id 指向缺失行时应输出空 scan_run。"""
    compressed = _make_compressed_context()

    class StubScheduler:
        def build_context(self, request) -> SimpleNamespace:
            return SimpleNamespace(
                context_run_id=7,
                compressed_context=compressed,
            )

    class StubStore:
        def get_context_run(self, context_run_id: int) -> dict[str, object]:
            return {
                "context_run_id": context_run_id,
                "scan_run_id": 3,
                "compression_profile": "weekly_balanced_v1",
                "status": "success",
            }

        def get_scan_run_detail(self, run_id: int) -> None:
            assert run_id == 3
            return None

    class StubFileScanner:
        def __init__(self) -> None:
            self.scan_index_store = StubStore()

    monkeypatch.setattr(benchmark_module, "ContextScheduler", StubScheduler)
    monkeypatch.setattr(benchmark_module, "FileScanner", StubFileScanner)

    payload = run_benchmark(_make_args())

    assert payload["context_run"]["context_run_id"] == 7
    assert payload["scan_run"] == {}


def test_run_benchmark_propagates_scan_run_read_errors(monkeypatch) -> None:
    """真实 DB/read 错误不能被吞掉，否则 benchmark 会产出误导性成功结果。"""
    compressed = _make_compressed_context()

    class StubScheduler:
        def build_context(self, request) -> SimpleNamespace:
            return SimpleNamespace(
                context_run_id=7,
                compressed_context=compressed,
            )

    class StubStore:
        def get_context_run(self, context_run_id: int) -> dict[str, object]:
            return {
                "context_run_id": context_run_id,
                "scan_run_id": 3,
                "compression_profile": "weekly_balanced_v1",
                "status": "success",
            }

        def get_scan_run_detail(self, run_id: int) -> dict[str, int]:
            raise RuntimeError("db failed")

    class StubFileScanner:
        def __init__(self) -> None:
            self.scan_index_store = StubStore()

    monkeypatch.setattr(benchmark_module, "ContextScheduler", StubScheduler)
    monkeypatch.setattr(benchmark_module, "FileScanner", StubFileScanner)

    with pytest.raises(RuntimeError, match="db failed"):
        run_benchmark(_make_args())
