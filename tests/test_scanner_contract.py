"""Rust/Python scanner v1 共享合同与 workspace 门禁。"""

from __future__ import annotations

import json
import pickle
from pathlib import Path
from types import SimpleNamespace

import pytest

try:
    import tomllib
except ModuleNotFoundError:  # Python 3.10 compatibility job
    import tomli as tomllib

from src.core.config import Config, SCANNER_CONTRACT_FIELDS
from src.services.scanner_config import SCANNER_PROFILE_V2_ONLY_FIELDS
from src.models.scanner_contract import (
    WorkerParseRequest,
    build_rust_core_crashed_envelope,
    validate_contract_payload,
)

PROJECT_ROOT = Path(__file__).resolve().parents[1]
CONTRACT_DIR = PROJECT_ROOT / "docs" / "contracts"
FIXTURE_DIR = (
    PROJECT_ROOT / "tests" / "fixtures" / "scanner_contract" / "v1"
)


def _load_json(path: Path) -> object:
    return json.loads(path.read_text(encoding="utf-8"))


def test_python_contract_accepts_and_round_trips_every_golden_fixture() -> None:
    """Python 与 Rust 后续实现必须消费同一份 manifest，不能另造样例。"""
    manifest = _load_json(FIXTURE_DIR / "fixture-manifest.json")
    assert isinstance(manifest, dict)

    for entry in manifest["valid_fixtures"]:
        payload = _load_json(FIXTURE_DIR / entry["file"])
        assert isinstance(payload, dict)
        parsed = validate_contract_payload(entry["schema"], payload)
        assert parsed.model_dump(mode="json", exclude_unset=True) == payload


def test_python_contract_rejects_every_invalid_fixture() -> None:
    """Schema 与跨 DTO 语义反例都必须在 Python 合同边界失败。"""
    corpus = _load_json(FIXTURE_DIR / "invalid-cases.json")
    assert isinstance(corpus, dict)

    for case in corpus["cases"]:
        with pytest.raises(ValueError, match=".+"):
            validate_contract_payload(
                case["schema"],
                case["payload"],
                related_payloads=case.get("related_payloads", []),
            )


def test_worker_contract_rejects_embedded_nul_in_absolute_path() -> None:
    payload = _load_json(FIXTURE_DIR / "worker-parse-xlsx-request.json")
    assert isinstance(payload, dict)
    payload["file_path"] = "C:\\evidence\x00hidden.xlsx"

    with pytest.raises(ValueError, match="path must be absolute"):
        WorkerParseRequest.model_validate(payload)


def test_scanner_contract_profile_copies_only_present_raw_leaves() -> None:
    """Python 只透传显式 scanner 叶子，不扩默认值或基础设施字段。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".xlsx"],
            ignored_patterns=["*归档*.xlsx"],
            max_workers=3,
            office_fallback_after_timeout=False,
            engine="rust_v2",
            rust_scanner_bin="bin/scanner",
            rust_office_parser_bin="bin/office-worker",
            rust_index_db_path="state/scan_index_v2.sqlite3",
            rust_process_timeout_seconds=45,
        )
    )

    profile = cfg.scanner_contract_profile()

    assert profile == {
        "schema_version": "scanner_profile_v1",
        "allowed_extensions": [".xlsx"],
        "ignored_patterns": ["*归档*.xlsx"],
        "max_workers": 3,
        "office_fallback_after_timeout": False,
    }
    pickle.dumps(profile)


def test_scanner_contract_profile_allowlist_matches_wire_schema() -> None:
    """配置提取 allowlist 必须与版本化 raw profile schema 同步。

    The v1|v2 raw profile schema accepts both `scanner_profile_v1` and
    `scanner_profile_v2`; the extraction allowlist must cover every v1 leaf
    plus every v2-only leaf (spec Part 8.1).
    """
    schema = _load_json(CONTRACT_DIR / "scanner-profile-request-v1.schema.json")
    assert isinstance(schema, dict)
    expected = set(schema["properties"]) - {"schema_version"}
    assert expected == set(SCANNER_CONTRACT_FIELDS) | SCANNER_PROFILE_V2_ONLY_FIELDS


def test_scanner_contract_profile_rejects_unknown_candidate_leaf() -> None:
    """拼错或未版本化的新 scanner 叶子不得被静默丢弃。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".txt"],
            unexpected_contract_leaf=1,
        )
    )

    with pytest.raises(ValueError, match="unexpected_contract_leaf"):
        cfg.scanner_contract_profile()


def test_rust_workspace_keeps_discovery_as_a_library_only() -> None:
    """Discovery 保留 workspace library，不再生成独立生产 helper。"""
    workspace_manifest = PROJECT_ROOT / "rust" / "Cargo.toml"
    workspace_lock = PROJECT_ROOT / "rust" / "Cargo.lock"
    assert workspace_manifest.is_file()
    assert workspace_lock.is_file()
    assert not (PROJECT_ROOT / "rust" / "discovery" / "Cargo.lock").exists()
    assert not (PROJECT_ROOT / "rust" / "office_parser" / "Cargo.lock").exists()
    assert (PROJECT_ROOT / "rust" / "discovery" / "src" / "lib.rs").is_file()
    assert not (PROJECT_ROOT / "rust" / "discovery" / "src" / "main.rs").exists()

    workspace = tomllib.loads(workspace_manifest.read_text(encoding="utf-8"))
    assert workspace["workspace"]["resolver"] == "2"
    assert set(workspace["workspace"]["members"]) == {
        "discovery",
        "office_parser",
        "scanner_cli",
        "scanner_contract",
        "scanner_core",
    }

    active_files = (
        "config/settings.example.yaml",
        "src/core/config.py",
        "src/core/healthcheck.py",
        "src/services/context_scheduler.py",
        "src/services/rust_context_client.py",
        ".github/workflows/ci.yml",
        "scripts/deploy_windows.ps1",
        "README.md",
        "docs/scanner-backends.md",
    )
    combined = "\n".join(
        (PROJECT_ROOT / path).read_text(encoding="utf-8")
        for path in active_files
    )
    assert "rust/discovery/target" not in combined
    assert "rust/office_parser/target" not in combined
    assert "rust\\discovery\\target" not in combined
    assert "rust\\office_parser\\target" not in combined
    assert "rust/target/release/ai-daily-scanner" in combined
    assert "rust/target/release/ai-daily-office-parser" in combined


def test_python_builds_one_strict_rust_core_crashed_envelope() -> None:
    envelope = build_rust_core_crashed_envelope(
        request_id="11111111-1111-4111-8111-111111111111",
        duration_ms=17,
    )

    assert envelope.status == "error"
    assert envelope.engine_version == "unknown"
    assert envelope.engine_build == "unknown"
    assert envelope.file_context == ""
    assert envelope.scan_run_id is None
    assert envelope.context_run_id is None
    assert envelope.summary.total_duration_ms == 17
    assert envelope.error is not None
    assert envelope.error.error_code == "RUST_CORE_CRASHED"
    assert envelope.error.stage == "process"
    assert envelope.error.file_path is None
    assert envelope.error.backend is None
