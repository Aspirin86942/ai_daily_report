"""Windows Rust production-default deployment and end-to-end gates."""

from __future__ import annotations

from datetime import date
from pathlib import Path
import sys
from types import SimpleNamespace

import pytest

from src.core.healthcheck import collect_healthcheck
from src.services.native_scanner import NativeScanner, ScanRequest


PROJECT_ROOT = Path(__file__).resolve().parents[1]
NATIVE_EXTENSION = (
    PROJECT_ROOT / "rust" / "target" / "release" / "ai_daily_scanner_native.dll"
)
OFFICE_BIN = (
    PROJECT_ROOT
    / "rust"
    / "target"
    / "release"
    / "ai-daily-office-parser.exe"
)


def _require_windows_release() -> None:
    if sys.platform != "win32":
        pytest.skip("Windows production E2E")
    missing = [
        path.name
        for path in (NATIVE_EXTENSION, OFFICE_BIN)
        if not path.is_file()
    ]
    if missing:
        pytest.fail(
            "release binaries are required: " + ", ".join(sorted(missing))
        )


def _runtime_config(
    root: Path,
    work_dir: Path,
    scan_db_path: Path,
) -> SimpleNamespace:
    raw_profile = {
        "schema_version": "scanner_profile_v1",
        "allowed_extensions": [".txt"],
        "ignored_patterns": ["~$*", "*.tmp"],
        "max_workers": 2,
    }
    return SimpleNamespace(
        rust_office_parser_bin=str(OFFICE_BIN),
        rust_index_db_path=str(scan_db_path),
        work_dir=work_dir,
        scanner_contract_profile=lambda: raw_profile.copy(),
        llm_provider="deepseek",
        llm_config={"model_id": "synthetic-no-network"},
        deepseek_api_key="synthetic-doctor-placeholder",
        openai_api_key="",
        reports_dir=root / "shared" / "reports",
        db_dir=root / "shared" / "db",
    )


def _scan_request() -> ScanRequest:
    return ScanRequest(
        report_mode="daily",
        start_date=date(2000, 1, 1),
        end_date=date(2099, 12, 31),
    )


def test_real_rust_chinese_path_e2e(tmp_path: Path) -> None:
    _require_windows_release()
    root = tmp_path / "Windows 验收 根目录"
    work_dir = root / "业务 合成数据"
    work_dir.mkdir(parents=True)
    (work_dir / "日报 证据.txt").write_text(
        "synthetic Windows Rust context evidence",
        encoding="utf-8",
    )
    scan_db_path = root / "shared" / "db" / "scan_index_v2.sqlite3"
    scan_db_path.parent.mkdir(parents=True)
    cfg = _runtime_config(root, work_dir, scan_db_path)

    doctor = collect_healthcheck(
        project_root=PROJECT_ROOT,
        config_obj=cfg,
        strict=True,
    )
    result = NativeScanner(cfg, project_root=PROJECT_ROOT).build_context(
        _scan_request()
    ).envelope

    assert doctor.errors == []
    assert doctor.info["Scanner Interface"] == "native"
    assert doctor.info["Scanner scan_db_parent"] == "ok"
    assert result.status == "ok"
    assert result.summary.source_file_count == 1
    assert result.summary.success_count == 1
    assert result.scan_run_id is not None
    assert "synthetic Windows Rust context evidence" in result.file_context


def test_real_rust_cold_warm_cache_e2e(tmp_path: Path) -> None:
    _require_windows_release()
    work_dir = tmp_path / "cache 合成目录"
    work_dir.mkdir()
    source = work_dir / "唯一 文件.txt"
    source.write_text("cold cache evidence", encoding="utf-8")
    scan_db_path = tmp_path / "state" / "scan_index_v2.sqlite3"
    scan_db_path.parent.mkdir()
    cfg = _runtime_config(tmp_path, work_dir, scan_db_path)
    scanner = NativeScanner(
        cfg,
        project_root=PROJECT_ROOT,
    )

    cold = scanner.build_context(_scan_request())
    warm = scanner.build_context(_scan_request())
    source.write_text("changed cache evidence with new size", encoding="utf-8")
    changed = scanner.build_context(_scan_request())

    assert cold.envelope.status == warm.envelope.status == changed.envelope.status == "ok"
    assert cold.evidence is not None
    assert warm.evidence is not None
    assert changed.evidence is not None

    assert [
        (item.parse_cache_status, item.cache_miss_reason)
        for item in cold.evidence.files
    ] == [
        ("miss", "new_file")
    ]
    assert [
        (item.parse_cache_status, item.cache_miss_reason)
        for item in warm.evidence.files
    ] == [
        ("snapshot", "")
    ]
    assert [
        (item.parse_cache_status, item.cache_miss_reason)
        for item in changed.evidence.files
    ] == [("miss", "source_version_changed")]
