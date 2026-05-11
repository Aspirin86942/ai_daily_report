"""测试扫描规划边界。"""

from pathlib import Path

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
        }
    )

    profile = planner.build_parser_profile(summary_mode=True)

    assert profile == {
        "excel_max_rows": 10,
        "pdf_max_pages": 2,
        "text_max_chars": 2000,
        "total_max_chars": 50000,
    }


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
