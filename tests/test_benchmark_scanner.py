"""测试 scanner benchmark 脚本。"""

from datetime import date
from pathlib import Path

from scripts.benchmark_scanner import (
    build_benchmark_payload,
    build_parser_backend_summary,
    render_markdown_report,
    write_report_files,
)
from src.models.schemas import FileContext, ScanResult
from src.services.file_scanner import NOT_PARSED_PARSER_BACKEND
from src.services.light_text_parser import LIGHT_TEXT_PARSER_BACKEND
from src.services.scan_metrics import ExtensionMetrics, ReparseDetail


def _make_reparse_detail(
    extension: str,
    parser_backend: str,
    truncated: bool,
    path: str = "D:\\work\\report.md",
    worker_lane: str = "",
) -> ReparseDetail:
    return ReparseDetail(
        path=path,
        extension=extension,
        file_identity=f"bootstrap:{path.lower()}",
        source_version="mtime=2:size=10",
        cache_status="miss",
        cache_miss_reason="source_version_changed",
        previous_source_version="mtime=1:size=10",
        parse_duration_ms=12,
        parse_status="success",
        parse_error="",
        parser_backend=parser_backend,
        worker_lane=worker_lane,
        truncated=truncated,
    )


def test_build_parser_backend_summary_returns_empty_summary():
    """没有重解析明细时应返回稳定的空 summary。"""
    assert build_parser_backend_summary([]) == {
        "direct_count": 0,
        "not_parsed_count": 0,
        "subprocess_count": 0,
        "truncated_count": 0,
        "by_extension": {},
    }


def test_build_parser_backend_summary_preserves_backend_dimensions_and_sorting():
    """summary 应保留真实 backend 维度，并按 extension 稳定排序。"""
    summary = build_parser_backend_summary(
        [
            _make_reparse_detail(
                extension=".txt",
                parser_backend="custom_backend",
                truncated=True,
                path="D:\\work\\custom.txt",
            ),
            _make_reparse_detail(
                extension=".md",
                parser_backend="",
                truncated=False,
                path="D:\\work\\fallback.md",
            ),
            _make_reparse_detail(
                extension=".txt",
                parser_backend=LIGHT_TEXT_PARSER_BACKEND,
                truncated=True,
                path="D:\\work\\light.txt",
            ),
        ]
    )

    assert summary["direct_count"] == 2
    assert summary["not_parsed_count"] == 1
    assert summary["subprocess_count"] == 0
    assert summary["truncated_count"] == 2
    assert list(summary["by_extension"]) == [".md", ".txt"]
    assert summary["by_extension"][".txt"] == {
        LIGHT_TEXT_PARSER_BACKEND: 1,
        "subprocess": 0,
        "custom_backend": 1,
        NOT_PARSED_PARSER_BACKEND: 0,
        "truncated": 2,
    }
    assert summary["by_extension"][".md"] == {
        LIGHT_TEXT_PARSER_BACKEND: 0,
        "subprocess": 0,
        NOT_PARSED_PARSER_BACKEND: 1,
        "truncated": 0,
    }


def test_build_parser_backend_summary_uses_worker_lane_for_office_pdf_backends():
    """summary 应用 worker_lane 统计 subprocess，用 backend 展示真实解析器。"""
    summary = build_parser_backend_summary(
        [
            _make_reparse_detail(
                extension=".docx",
                parser_backend="office_v1",
                worker_lane="subprocess",
                truncated=True,
                path="D:\\work\\report.docx",
            ),
            _make_reparse_detail(
                extension=".pdf",
                parser_backend="pdf_text_v1",
                worker_lane="subprocess",
                truncated=False,
                path="D:\\work\\report.pdf",
            ),
            _make_reparse_detail(
                extension=".md",
                parser_backend=LIGHT_TEXT_PARSER_BACKEND,
                worker_lane="direct",
                truncated=False,
                path="D:\\work\\note.md",
            ),
            _make_reparse_detail(
                extension=".pptx",
                parser_backend=NOT_PARSED_PARSER_BACKEND,
                worker_lane="not_parsed",
                truncated=False,
                path="D:\\work\\large.pptx",
            ),
        ]
    )

    assert summary["direct_count"] == 1
    assert summary["subprocess_count"] == 2
    assert summary["not_parsed_count"] == 1
    assert summary["truncated_count"] == 1
    assert summary["by_extension"][".docx"]["office_v1"] == 1
    assert summary["by_extension"][".docx"]["subprocess"] == 1
    assert summary["by_extension"][".pdf"]["pdf_text_v1"] == 1
    assert summary["by_extension"][".pdf"]["subprocess"] == 1
    assert summary["by_extension"][".pptx"][NOT_PARSED_PARSER_BACKEND] == 1


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
    reparse_details = [
        ReparseDetail(
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
            parser_backend=LIGHT_TEXT_PARSER_BACKEND,
            worker_lane="direct",
            truncated=True,
        )
    ]

    payload = build_benchmark_payload(
        scan_result=scan_result,
        run_detail=run_detail,
        extension_metrics=extension_metrics,
        reparse_details=reparse_details,
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
    assert payload["reparse_details"] == [
        {
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
            "parser_backend": LIGHT_TEXT_PARSER_BACKEND,
            "worker_lane": "direct",
            "truncated": True,
        }
    ]
    assert payload["parser_backend_summary"] == {
        "direct_count": 1,
        "not_parsed_count": 0,
        "subprocess_count": 0,
        "truncated_count": 1,
        "by_extension": {
            ".md": {
                LIGHT_TEXT_PARSER_BACKEND: 1,
                "subprocess": 0,
                NOT_PARSED_PARSER_BACKEND: 0,
                "truncated": 1,
            }
        },
    }


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
        "reparse_details": [
            {
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
                "parser_backend": LIGHT_TEXT_PARSER_BACKEND,
                "truncated": True,
            }
        ],
        "parser_backend_summary": {
            "direct_count": 1,
            "subprocess_count": 0,
            "truncated_count": 1,
            "by_extension": {
                ".md": {
                    LIGHT_TEXT_PARSER_BACKEND: 1,
                    "subprocess": 0,
                    "truncated": 1,
                }
            },
        },
    }

    markdown = render_markdown_report(payload)

    assert "# Scanner Benchmark Report" in markdown
    assert "| total | 120 |" in markdown
    assert "| discovery | 10 |" in markdown
    assert "| .txt | 2 | 80 | 1 | 1 | 1 |" in markdown
    assert "## Parser Backend Summary" in markdown
    assert "- direct_count: `1`" in markdown
    assert "- not_parsed_count: `0`" in markdown
    assert "- subprocess_count: `0`" in markdown
    assert "- truncated_count: `1`" in markdown
    assert "| .md | light_text_v1 | 1 | 0 | 1 |" in markdown
    assert "## Reparse Details" in markdown
    assert "| .md | source_version_changed | 12 | success | D:\\work\\report.md |" in markdown


def test_render_markdown_report_orders_backend_summary_rows_stably():
    """Markdown backend summary 应按 extension/backend 稳定输出真实 backend 行。"""
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
            "run_id": 7,
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
        "reparse_details": [],
        "parser_backend_summary": build_parser_backend_summary(
            [
                _make_reparse_detail(
                    extension=".txt",
                    parser_backend="custom_backend",
                    truncated=True,
                    path="D:\\work\\custom.txt",
                ),
                _make_reparse_detail(
                    extension=".md",
                    parser_backend="",
                    truncated=False,
                    path="D:\\work\\fallback.md",
                ),
                _make_reparse_detail(
                    extension=".txt",
                    parser_backend=LIGHT_TEXT_PARSER_BACKEND,
                    truncated=True,
                    path="D:\\work\\light.txt",
                ),
            ]
        ),
    }

    markdown = render_markdown_report(payload)

    not_parsed_row = "| .md | not_parsed | 1 | 0 | 0 |"
    light_row = "| .txt | light_text_v1 | 1 | 0 | 2 |"
    custom_row = "| .txt | custom_backend | 1 | 0 | 2 |"
    assert "- not_parsed_count: `1`" in markdown
    assert not_parsed_row in markdown
    assert light_row in markdown
    assert custom_row in markdown
    assert markdown.index(not_parsed_row) < markdown.index(light_row)
    assert markdown.index(light_row) < markdown.index(custom_row)


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
        "reparse_details": [],
    }
    json_out = tmp_path / "benchmark.json"
    markdown_out = tmp_path / "benchmark.md"

    write_report_files(payload, json_out=json_out, markdown_out=markdown_out)

    assert '"start_date": "2026-05-23"' in json_out.read_text(encoding="utf-8")
    assert "Scanner Benchmark Report" in markdown_out.read_text(encoding="utf-8")
