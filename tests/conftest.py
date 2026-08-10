"""共享测试 fixtures：为后续阶段提供跨模块复用的测试设施。"""

from __future__ import annotations

import os
from pathlib import Path

import pytest


def pytest_collection_modifyitems(config, items):
    """性能门禁仅在 CI(GitHub Actions 设 CI=true)或显式 RUN_PERF_GATES=1 时全跑;
    本地开发默认跳过,避免环境性能地板触发红牌。"""
    if os.environ.get("CI") or os.environ.get("RUN_PERF_GATES") == "1":
        return
    for item in items:
        if "perf_gate" in item.keywords:
            item.add_marker(
                pytest.mark.skip(
                    reason="性能门禁仅 CI 全跑(本地环境与验收预算不符);"
                    "如需本地全跑设 RUN_PERF_GATES=1"
                )
            )


@pytest.fixture(scope="session")
def rust_release_binaries() -> dict[str, Path]:
    """Rust release 二进制路径（Windows-first）。缺失时由依赖方决定跳过。"""
    release = Path(__file__).resolve().parents[1] / "rust" / "target" / "release"
    return {
        "scanner": release / "ai-daily-scanner.exe",
        "office_parser": release / "ai-daily-office-parser.exe",
    }
