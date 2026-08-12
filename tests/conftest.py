"""共享测试 fixtures：为后续阶段提供跨模块复用的测试设施。"""

from __future__ import annotations

from pathlib import Path

import pytest


@pytest.fixture(scope="session")
def rust_release_binaries() -> dict[str, Path]:
    """Rust release 二进制路径（Windows-first）。缺失时由依赖方决定跳过。"""
    release = Path(__file__).resolve().parents[1] / "rust" / "target" / "release"
    return {
        "scanner_native": release / "ai_daily_scanner_native.dll",
        "office_parser": release / "ai-daily-office-parser.exe",
    }
