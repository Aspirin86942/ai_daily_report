"""Windows Rust production-default deployment and end-to-end gates."""

from __future__ import annotations

from datetime import date
from pathlib import Path
import sys
from types import SimpleNamespace

import pytest
import yaml

from src.core.healthcheck import collect_healthcheck
from src.services.context_scheduler import (
    ContextScheduleRequest,
    ContextScheduler,
)
from src.services.rust_context_client import RustContextClient


PROJECT_ROOT = Path(__file__).resolve().parents[1]
SCANNER_BIN = (
    PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-scanner.exe"
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
        for path in (SCANNER_BIN, OFFICE_BIN)
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
        scanner_engine="rust_v2",
        rust_scanner_bin=str(SCANNER_BIN),
        rust_index_db_path=str(scan_db_path),
        rust_process_timeout_seconds=60.0,
        work_dir=work_dir,
        scanner_contract_profile=lambda: raw_profile.copy(),
        scanner_config={
            "max_workers": 2,
            "allowed_extensions": [".txt"],
            "office_parser_backend": "rust_office_oxide_v1",
            "rust_office_parser_bin": str(OFFICE_BIN),
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": ["python_office_v1"],
            "office_legacy_extensions_enabled": False,
        },
        llm_provider="deepseek",
        llm_config={"model_id": "synthetic-no-network"},
        deepseek_api_key="synthetic-doctor-placeholder",
        openai_api_key="",
        reports_dir=root / "shared" / "reports",
        db_dir=root / "shared" / "db",
    )


def _schedule_request() -> ContextScheduleRequest:
    return ContextScheduleRequest(
        report_mode="daily",
        source="scan",
        start_date=date(2000, 1, 1),
        end_date=date(2099, 12, 31),
    )


def test_deploy_windows_builds_rust_and_finishes_with_strict_doctor() -> None:
    script = (PROJECT_ROOT / "scripts" / "deploy_windows.ps1").read_text(
        encoding="utf-8"
    )

    assert "[switch]$BuildRust" not in script
    assert "if ($BuildRust)" not in script
    assert '"build",' in script
    assert '"--workspace",' in script
    assert '"--release",' in script
    assert '"--locked"' in script
    assert '@("main.py", "doctor", "--strict")' in script
    assert "Copy-Item -LiteralPath $exampleSettings" in script
    assert "Keeping existing config\\settings.windows.yaml" in script
    assert "api_key" not in script.lower()


def test_windows_ci_keeps_all_production_gates_in_one_job() -> None:
    workflow = yaml.safe_load(
        (PROJECT_ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
    )
    jobs = workflow["jobs"]

    assert "windows-production" in jobs
    windows = jobs["windows-production"]
    assert windows["runs-on"] == "windows-latest"
    steps = windows["steps"]
    combined = "\n".join(str(step) for step in steps)

    for required in (
        "requirements.lock",
        "cargo fmt",
        "cargo clippy",
        "cargo test",
        "cargo build",
        "doctor --strict",
        "pytest tests/",
        "test_windows_rust_core_e2e.py",
        "deploy_windows.ps1",
        "Get-FileHash",
    ):
        assert required in combined
    assert combined.count("deploy_windows.ps1") >= 2
    assert "AI_DAILY_TEST_FORBID_LLM" in combined
    assert "task11-preserve.sentinel" in combined

    compatibility = jobs["linux-compatibility"]
    assert compatibility["runs-on"] == "ubuntu-latest"
    compatibility_text = str(compatibility).lower()
    assert "upload-artifact" not in compatibility_text
    assert "release" not in compatibility_text


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
    result = ContextScheduler(runtime_config=cfg).build_context(
        _schedule_request()
    )

    assert doctor.errors == []
    assert doctor.info["Scanner Engine"] == "rust_v2"
    assert doctor.info["Rust scan_db_parent"] == "ok"
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
    client = RustContextClient(
        config=cfg,
        project_root=PROJECT_ROOT,
        scanner_binary=SCANNER_BIN,
        scan_db_path=scan_db_path,
        office_worker_path=OFFICE_BIN,
        timeout_seconds=60,
    )

    cold = client.build_context(_schedule_request())
    warm = client.build_context(_schedule_request())
    source.write_text("changed cache evidence with new size", encoding="utf-8")
    changed = client.build_context(_schedule_request())

    assert cold.status == warm.status == changed.status == "ok"
    assert cold.scan_run_id is not None
    assert warm.scan_run_id is not None
    assert changed.scan_run_id is not None
    cold_audit = client.inspect_run(cold.scan_run_id)
    warm_audit = client.inspect_run(warm.scan_run_id)
    changed_audit = client.inspect_run(changed.scan_run_id)

    assert [
        (item.cache_status, item.cache_miss_reason)
        for item in cold_audit.files
    ] == [
        ("miss", "new_file")
    ]
    assert [
        (item.cache_status, item.cache_miss_reason)
        for item in warm_audit.files
    ] == [
        ("fresh", "")
    ]
    assert [
        (item.cache_status, item.cache_miss_reason)
        for item in changed_audit.files
    ] == [("miss", "source_version_changed")]
