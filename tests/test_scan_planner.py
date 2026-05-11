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
        "total_max_chars": 50000,
        "summary_mode": True,
        "parser_profile_version": "v7",
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
