"""测试 scanner benchmark 脚本。"""

from datetime import date
from pathlib import Path

from scripts.benchmark_scanner import (
    build_benchmark_payload,
    render_markdown_report,
    write_report_files,
)
from src.models.schemas import FileContext, ScanResult
from src.services.scan_metrics import ExtensionMetrics


def test_build_benchmark_payload_uses_scan_result_and_metrics():
    """benchmark payload 应组合扫描结果、run detail 和扩展名明细。"""
    scan_result = ScanResult(
        total_files=2,
        success_count=1,
        error_count=1,
        contexts=[
            FileContext(
                file_path="a.txt",
                file_type=".txt",
                content="hello",
                error=None,
            )
        ],
    )
    run_detail = {
        "run_id": 7,
        "discovered_count": 2,
        "reused_count": 0,
        "reparsed_count": 2,
        "total_duration_ms": 120,
        "discovery_duration_ms": 10,
        "inventory_cache_duration_ms": 20,
        "parse_duration_ms": 80,
        "aggregation_duration_ms": 5,
        "success_count": 1,
        "error_count": 1,
        "timeout_count": 1,
    }
    extension_metrics = [
        ExtensionMetrics(
            extension=".txt",
            file_count=2,
            parse_duration_ms=80,
            success_count=1,
            error_count=1,
            timeout_count=1,
        )
    ]

    payload = build_benchmark_payload(
        scan_result=scan_result,
        run_detail=run_detail,
        extension_metrics=extension_metrics,
        start_date=date(2026, 5, 23),
        end_date=date(2026, 5, 24),
        summary_mode=True,
    )

    assert payload["parameters"] == {
        "start_date": "2026-05-23",
        "end_date": "2026-05-24",
        "summary_mode": True,
    }
    assert payload["scan_result"] == {
        "total_files": 2,
        "success_count": 1,
        "error_count": 1,
    }
    assert payload["metrics"]["run_id"] == 7
    assert payload["extension_metrics"] == [
        {
            "extension": ".txt",
            "file_count": 2,
            "parse_duration_ms": 80,
            "success_count": 1,
            "error_count": 1,
            "timeout_count": 1,
        }
    ]


def test_render_markdown_report_contains_stage_and_extension_metrics():
    """Markdown 报告应包含阶段耗时和扩展名明细表。"""
    payload = {
        "parameters": {
            "start_date": "2026-05-23",
            "end_date": "2026-05-24",
            "summary_mode": False,
        },
        "scan_result": {
            "total_files": 2,
            "success_count": 1,
            "error_count": 1,
        },
        "metrics": {
            "run_id": 7,
            "discovered_count": 2,
            "reused_count": 0,
            "reparsed_count": 2,
            "total_duration_ms": 120,
            "discovery_duration_ms": 10,
            "inventory_cache_duration_ms": 20,
            "parse_duration_ms": 80,
            "aggregation_duration_ms": 5,
            "success_count": 1,
            "error_count": 1,
            "timeout_count": 1,
        },
        "extension_metrics": [
            {
                "extension": ".txt",
                "file_count": 2,
                "parse_duration_ms": 80,
                "success_count": 1,
                "error_count": 1,
                "timeout_count": 1,
            }
        ],
    }

    markdown = render_markdown_report(payload)

    assert "# Scanner Benchmark Report" in markdown
    assert "| total | 120 |" in markdown
    assert "| discovery | 10 |" in markdown
    assert "| .txt | 2 | 80 | 1 | 1 | 1 |" in markdown


def test_write_report_files_writes_utf8_json_and_markdown(tmp_path: Path):
    """脚本输出文件应显式用 UTF-8，保留中文字段。"""
    payload = {
        "parameters": {
            "start_date": "2026-05-23",
            "end_date": "2026-05-24",
            "summary_mode": False,
        },
        "scan_result": {
            "total_files": 0,
            "success_count": 0,
            "error_count": 0,
        },
        "metrics": {
            "run_id": 1,
            "discovered_count": 0,
            "reused_count": 0,
            "reparsed_count": 0,
            "total_duration_ms": 0,
            "discovery_duration_ms": 0,
            "inventory_cache_duration_ms": 0,
            "parse_duration_ms": 0,
            "aggregation_duration_ms": 0,
            "success_count": 0,
            "error_count": 0,
            "timeout_count": 0,
        },
        "extension_metrics": [],
    }
    json_out = tmp_path / "benchmark.json"
    markdown_out = tmp_path / "benchmark.md"

    write_report_files(payload, json_out=json_out, markdown_out=markdown_out)

    assert '"start_date": "2026-05-23"' in json_out.read_text(encoding="utf-8")
    assert "Scanner Benchmark Report" in markdown_out.read_text(encoding="utf-8")
