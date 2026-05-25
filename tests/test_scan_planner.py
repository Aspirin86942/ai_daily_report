"""测试扫描规划边界。"""

from datetime import date
from pathlib import Path
from types import SimpleNamespace

from src.services.scan_planner import ScanPlanner


def test_build_parser_profile_uses_summary_limits_when_requested():
    """摘要模式应切换到缩减解析预算。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
            "summary_excel_max_rows": 10,
            "summary_pdf_max_pages": 2,
            "summary_text_max_chars": 2000,
            "total_max_chars": 50000,
            "parser_profile_version": "v7",
        }
    )

    profile = planner.build_parser_profile(summary_mode=True)

    assert profile == {
        "excel_max_rows": 10,
        "pdf_max_pages": 2,
        "text_max_chars": 2000,
        "office_parser_backend": "rust_office_oxide_v1",
        "pdf_parser_backend": "pdf_text_v1",
        "rust_office_parser_bin": "rust/office_parser/target/release/ai-daily-office-parser",
        "office_parser_fallback_enabled": True,
        "office_parser_fallback_order": [
            "python_office_v1",
            "python_sharepoint_text_v1",
        ],
        "office_fallback_after_timeout": False,
        "office_external_fallback": "disabled",
        "office_legacy_extensions_enabled": False,
        "excel_max_sheets": 2,
        "excel_max_columns": 12,
        "docx_max_paragraphs": 80,
        "docx_max_tables": 8,
        "docx_table_max_rows": 20,
        "docx_table_max_cols": 8,
        "pptx_max_slides": 15,
        "document_excerpt_max_chars": 2000,
        "pptx_include_notes": True,
        "total_max_chars": 50000,
        "summary_mode": True,
        "parser_profile_version": "v7",
        "text_parser_backend": "light_text_v1",
        "direct_text_read_bytes": 262144,
        "log_tail_read_bytes": 262144,
        "text_excerpt_max_chars": 2000,
    }


def test_build_parser_profile_includes_light_text_parser_defaults():
    """parser profile 必须包含轻量文本解析参数，避免 cache 错误复用。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
            "total_max_chars": 50000,
            "parser_profile_version": "v8",
        }
    )

    profile = planner.build_parser_profile(summary_mode=False)

    assert profile["text_parser_backend"] == "light_text_v1"
    assert profile["direct_text_read_bytes"] == 262144
    assert profile["log_tail_read_bytes"] == 262144
    assert profile["text_excerpt_max_chars"] == 6000


def test_build_parser_profile_includes_document_parser_defaults():
    """Office/PDF parser 预算必须进入 profile，避免旧缓存错误复用。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
            "total_max_chars": 50000,
            "parser_profile_version": "v9",
        }
    )

    profile = planner.build_parser_profile(summary_mode=False)

    assert profile["office_parser_backend"] == "rust_office_oxide_v1"
    assert profile["pdf_parser_backend"] == "pdf_text_v1"
    assert profile["rust_office_parser_bin"] == (
        "rust/office_parser/target/release/ai-daily-office-parser"
    )
    assert profile["office_parser_fallback_enabled"] is True
    assert profile["office_parser_fallback_order"] == [
        "python_office_v1",
        "python_sharepoint_text_v1",
    ]
    assert profile["office_fallback_after_timeout"] is False
    assert profile["office_external_fallback"] == "disabled"
    assert profile["office_legacy_extensions_enabled"] is False
    assert profile["excel_max_sheets"] == 5
    assert profile["excel_max_columns"] == 20
    assert profile["docx_max_paragraphs"] == 200
    assert profile["docx_max_tables"] == 20
    assert profile["docx_table_max_rows"] == 50
    assert profile["docx_table_max_cols"] == 12
    assert profile["pptx_max_slides"] == 50
    assert profile["pptx_include_notes"] is True
    assert profile["document_excerpt_max_chars"] == 6000


def test_build_parser_profile_includes_rust_office_backend_and_fallback_keys():
    """Office backend/fallback 配置必须进入 cache key，避免跨 backend 复用旧内容。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
            "office_parser_backend": "rust_office_oxide_v1",
            "rust_office_parser_bin": "rust/office_parser/target/release/ai-daily-office-parser",
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": [
                "python_office_v1",
                "python_sharepoint_text_v1",
            ],
            "office_fallback_after_timeout": False,
            "office_external_fallback": "disabled",
            "office_legacy_extensions_enabled": False,
        }
    )

    profile = planner.build_parser_profile(summary_mode=False)

    assert profile["office_parser_backend"] == "rust_office_oxide_v1"
    assert profile["rust_office_parser_bin"] == (
        "rust/office_parser/target/release/ai-daily-office-parser"
    )
    assert profile["office_parser_fallback_enabled"] is True
    assert profile["office_parser_fallback_order"] == [
        "python_office_v1",
        "python_sharepoint_text_v1",
    ]
    assert profile["office_fallback_after_timeout"] is False
    assert profile["office_external_fallback"] == "disabled"
    assert profile["office_legacy_extensions_enabled"] is False


def test_build_parser_profile_uses_summary_document_limits():
    """summary mode 应使用 Office/PDF 的缩减预算。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "summary_excel_max_rows": 10,
            "excel_max_sheets": 6,
            "summary_excel_max_sheets": 2,
            "excel_max_columns": 20,
            "summary_excel_max_columns": 8,
            "pdf_max_pages": 5,
            "summary_pdf_max_pages": 2,
            "text_max_chars": 6000,
            "summary_text_max_chars": 2000,
            "docx_max_paragraphs": 200,
            "summary_docx_max_paragraphs": 80,
            "docx_max_tables": 20,
            "summary_docx_max_tables": 5,
            "docx_table_max_rows": 50,
            "summary_docx_table_max_rows": 10,
            "docx_table_max_cols": 12,
            "summary_docx_table_max_cols": 6,
            "pptx_max_slides": 50,
            "summary_pptx_max_slides": 12,
            "document_excerpt_max_chars": 6000,
            "summary_document_excerpt_max_chars": 2000,
        }
    )

    profile = planner.build_parser_profile(summary_mode=True)

    assert profile["excel_max_rows"] == 10
    assert profile["excel_max_sheets"] == 2
    assert profile["excel_max_columns"] == 8
    assert profile["pdf_max_pages"] == 2
    assert profile["docx_max_paragraphs"] == 80
    assert profile["docx_max_tables"] == 5
    assert profile["docx_table_max_rows"] == 10
    assert profile["docx_table_max_cols"] == 6
    assert profile["pptx_max_slides"] == 12
    assert profile["document_excerpt_max_chars"] == 2000


def test_build_parser_profile_uses_legacy_direct_text_max_bytes_as_read_budget():
    """旧配置只设置 direct_text_max_bytes 时，应作为读取预算兼容。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
            "direct_text_max_bytes": 8192,
        }
    )

    profile = planner.build_parser_profile(summary_mode=False)

    assert profile["direct_text_read_bytes"] == 8192


def test_build_parser_profile_normalizes_invalid_light_text_budgets():
    """无效/非正 text-like 预算必须归一化，避免 cache key 与运行时漂移。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 0,
            "direct_text_max_bytes": "bad",
            "direct_text_read_bytes": -1,
            "log_tail_read_bytes": None,
            "text_excerpt_max_chars": 0,
        }
    )

    profile = planner.build_parser_profile(summary_mode=False)

    assert profile["text_max_chars"] == 6000
    assert profile["direct_text_read_bytes"] == 262144
    assert profile["log_tail_read_bytes"] == 262144
    assert profile["text_excerpt_max_chars"] == 6000


def test_plan_candidates_splits_cache_hits_and_misses(tmp_path: Path):
    """规划器应把候选文件拆分为缓存命中和未命中两组。"""
    first = tmp_path / "first.md"
    second = tmp_path / "second.md"
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
        }
    )

    plan = planner.plan_candidates(
        candidates=[first, second],
        cached_file_paths={str(second)},
    )

    assert plan["uncached"] == [first]
    assert plan["cached"] == [second]


def test_plan_candidates_uses_inventory_cache_lookup():
    """库存候选应根据 cache_lookup 拆分为缓存命中和未命中。"""
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
        }
    )
    cached_item = SimpleNamespace(
        file_identity="bootstrap:/work/cached.txt",
        path=Path("/work/cached.txt"),
        extension=".txt",
        modified_date=date(2026, 5, 10),
        size_bytes=10,
        source_version="mtime=1:size=10",
    )
    uncached_item = SimpleNamespace(
        file_identity="bootstrap:/work/uncached.txt",
        path=Path("/work/uncached.txt"),
        extension=".txt",
        modified_date=date(2026, 5, 10),
        size_bytes=20,
        source_version="mtime=2:size=20",
    )

    plan = planner.plan_candidates(
        candidates=[cached_item, uncached_item],
        start_date=date(2026, 5, 9),
        end_date=date(2026, 5, 11),
        cache_lookup={
            "bootstrap:/work/cached.txt": True,
            "bootstrap:/work/uncached.txt": False,
        },
    )

    assert plan["cached"] == [cached_item]
    assert plan["uncached"] == [uncached_item]
    assert plan["total_candidates"] == 2


def test_serialize_parser_profile_uses_stable_json_key_order():
    """parser profile 序列化应稳定排序，避免同义配置导致 cache key 漂移。"""
    planner = ScanPlanner(scanner_cfg={})

    serialized = planner.serialize_parser_profile(
        {
            "text_max_chars": 2000,
            "excel_max_rows": 10,
            "parser_profile_version": "v1",
        }
    )

    assert serialized == (
        '{"excel_max_rows":10,"parser_profile_version":"v1","text_max_chars":2000}'
    )
