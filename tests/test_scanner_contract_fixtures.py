"""Windows-first scanner 合同资产测试。"""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

import pytest
import yaml

PROJECT_ROOT = Path(__file__).resolve().parents[1]
CONTRACT_DIR = PROJECT_ROOT / "docs" / "contracts"
FIXTURE_DIR = PROJECT_ROOT / "tests" / "fixtures" / "scanner_contract" / "v1"

SCHEMA_FILES = {
    "build-context-request-v1.schema.json",
    "context-envelope-v1.schema.json",
    "diagnostic-v1.schema.json",
    "doctor-request-v1.schema.json",
    "doctor-response-v1.schema.json",
    "normalized-scanner-settings.schema.json",
    "scanner-settings.schema.json",
}

REQUIRED_VALID_FIXTURES = {
    "diagnostic.json",
    "doctor-request.json",
    "doctor-response-error.json",
    "doctor-response-ok.json",
    "doctor-response-partial.json",
    "normalized-settings-daily.json",
    "normalized-settings-monthly.json",
    "normalized-settings-weekly.json",
    "request-windows-unc.json",
    "request-v2.json",
    "request.json",
    "response-error.json",
    "response-ok.json",
    "response-partial.json",
}

REQUIRED_INVALID_CLASSES = {
    "bound",
    "canonicalization",
    "echo",
    "enum",
    "nullability",
    "optionality",
    "path",
    "request_id",
    "status_invariant",
    "type",
    "unknown_field",
}

REQUIRED_INVALID_CASES = {
    "build_request_compression_profile_mode_mismatch",
    "build_request_dotted_module_empty_segment",
    "build_request_braced_request_id",
    "build_request_relative_adapter_path",
    "build_request_relative_work_dir",
    "build_request_reversed_date_range",
    "build_request_unknown_field",
    "context_error_nonempty_context",
    "context_error_without_error",
    "context_ok_empty_context",
    "context_ok_null_run_id",
    "context_ok_with_error",
    "context_partial_empty_context",
    "context_partial_null_run_id",
    "context_partial_without_warning",
    "context_response_request_id_mismatch",
    "context_summary_count_invariant",
    "diagnostic_invalid_stage",
    "diagnostic_missing_nullable_backend",
    "doctor_error_without_error",
    "doctor_invalid_check_status",
    "doctor_ok_with_error",
    "doctor_partial_without_warning",
    "normalized_context_threshold_order",
    "profile_array_over_limit",
    "profile_char_budget_over_limit",
    "profile_extension_invalid",
    "profile_fallback_backend_invalid",
    "profile_integer_boolean",
    "profile_integer_float",
    "profile_integer_numeric_string",
    "profile_null_optional_leaf",
    "profile_read_budget_over_limit",
    "profile_string_over_limit",
    "profile_timeout_over_limit",
    "profile_unknown_infrastructure_leaf",
}

REQUIRED_SEMANTIC_CASES = {
    "build_request_reversed_date_range",
    "context_response_request_id_mismatch",
    "context_summary_count_invariant",
    "normalized_set_array_not_sorted",
    "profile_integer_float",
}


def _load_json(path: Path) -> object:
    return json.loads(path.read_text(encoding="utf-8"))


def _iter_strings(value: object):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for key, item in value.items():
            yield from _iter_strings(key)
            yield from _iter_strings(item)
    elif isinstance(value, list):
        for item in value:
            yield from _iter_strings(item)


def test_contract_schema_and_fixture_manifest_is_complete() -> None:
    """所有 DTO schema 和 golden fixture 必须由同一 manifest 明确登记。"""
    assert {path.name for path in CONTRACT_DIR.glob("*.schema.json")} == (
        SCHEMA_FILES
    )
    for schema_name in SCHEMA_FILES:
        schema = _load_json(CONTRACT_DIR / schema_name)
        assert isinstance(schema, dict)
        assert schema["$schema"] == (
            "https://json-schema.org/draft/2020-12/schema"
        )
        assert schema["type"] == "object"
        assert schema["additionalProperties"] is False

    manifest = _load_json(FIXTURE_DIR / "fixture-manifest.json")
    assert isinstance(manifest, dict)
    assert manifest["schema_version"] == "scanner_fixture_manifest_v1"
    entries = manifest["valid_fixtures"]
    assert entries
    registered_files = {entry["file"] for entry in entries}
    assert len(entries) == len(registered_files)
    actual_files = {
        path.name
        for path in FIXTURE_DIR.glob("*.json")
        if path.name not in {"fixture-manifest.json", "invalid-cases.json"}
    }
    assert registered_files == actual_files == REQUIRED_VALID_FIXTURES
    for entry in entries:
        assert entry["schema"] in SCHEMA_FILES
        assert isinstance(_load_json(FIXTURE_DIR / entry["file"]), dict)


def test_invalid_fixture_corpus_covers_every_contract_rule_class() -> None:
    """无效 corpus 必须覆盖计划点名的每类拒绝规则。"""
    corpus = _load_json(FIXTURE_DIR / "invalid-cases.json")
    assert isinstance(corpus, dict)
    assert corpus["schema_version"] == "scanner_invalid_fixture_corpus_v1"
    cases = corpus["cases"]
    names = {case["name"] for case in cases}
    assert len(names) == len(cases)
    assert REQUIRED_INVALID_CASES <= names
    semantic_names = {
        case["name"]
        for case in cases
        if case["validation_layer"] == "semantic"
    }
    assert semantic_names == REQUIRED_SEMANTIC_CASES
    covered_classes = {
        rule_class
        for case in cases
        for rule_class in case["rule_classes"]
    }
    assert REQUIRED_INVALID_CLASSES <= covered_classes
    for case in cases:
        assert case["schema"] in SCHEMA_FILES
        assert case["validation_layer"] in {"schema", "semantic"}
        assert case["rule_classes"]
        assert isinstance(case["payload"], dict)
        related_payloads = case.get("related_payloads", [])
        if "echo" in case["rule_classes"]:
            assert case["validation_layer"] == "semantic"
            assert related_payloads
        for related in related_payloads:
            assert set(related) == {"role", "schema", "payload"}
            assert related["role"] == "request"
            assert related["schema"] in SCHEMA_FILES
            assert isinstance(related["payload"], dict)

    by_name = {case["name"]: case["payload"] for case in cases}
    assert len(by_name["profile_array_over_limit"]["ignored_patterns"]) == 257
    assert len(by_name["profile_string_over_limit"]["ignored_patterns"][0]) == 1025
    assert type(by_name["profile_integer_float"]["max_workers"]) is float
    assert by_name["profile_integer_numeric_string"]["max_workers"] == "4"


@pytest.mark.skipif(
    sys.platform != "win32",
    reason="Windows-first contract validation uses PowerShell Test-Json",
)
def test_valid_and_invalid_fixtures_match_draft_2020_12_schemas(
    tmp_path: Path,
) -> None:
    """Windows 门禁必须实际执行 Schema，而不是只检查 JSON 语法。"""
    powershell = shutil.which("pwsh") or shutil.which("powershell")
    assert powershell is not None

    manifest = _load_json(FIXTURE_DIR / "fixture-manifest.json")
    corpus = _load_json(FIXTURE_DIR / "invalid-cases.json")
    assert isinstance(manifest, dict)
    assert isinstance(corpus, dict)

    checks = []
    for entry in manifest["valid_fixtures"]:
        checks.append(
            {
                "name": entry["file"],
                "schema": str(CONTRACT_DIR / entry["schema"]),
                "payload": _load_json(FIXTURE_DIR / entry["file"]),
                "expected_valid": True,
            }
        )
    for case in corpus["cases"]:
        checks.append(
            {
                "name": case["name"],
                "schema": str(CONTRACT_DIR / case["schema"]),
                "payload": case["payload"],
                "expected_valid": case["validation_layer"] == "semantic",
            }
        )
        for related in case.get("related_payloads", []):
            checks.append(
                {
                    "name": f'{case["name"]}:{related["role"]}',
                    "schema": str(CONTRACT_DIR / related["schema"]),
                    "payload": related["payload"],
                    "expected_valid": True,
                }
            )

    checks_path = tmp_path / "schema-checks.json"
    checks_path.write_text(
        json.dumps(checks, ensure_ascii=False),
        encoding="utf-8",
    )
    script = r"""
$ErrorActionPreference = 'Stop'
$checks = Get-Content `
  -LiteralPath $env:AI_DAILY_SCHEMA_CHECKS `
  -Raw `
  -Encoding UTF8 | ConvertFrom-Json
foreach ($check in $checks) {
  $payload = $check.payload | ConvertTo-Json -Depth 100 -Compress
  try {
    $actual = Test-Json `
      -Json $payload `
      -SchemaFile $check.schema `
      -ErrorAction Stop
  } catch {
    $actual = $false
  }
  if ([bool]$actual -ne [bool]$check.expected_valid) {
    throw "Schema expectation mismatch: $($check.name)"
  }
}
"""
    completed = subprocess.run(
        [
            powershell,
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            script,
        ],
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=120,
        check=False,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        env={
            **os.environ,
            "AI_DAILY_SCHEMA_CHECKS": str(checks_path),
        },
    )
    assert completed.returncode == 0, (
        completed.stdout + "\n" + completed.stderr
    )


def test_contract_fixture_corpus_is_synthetic_and_non_secret() -> None:
    """合同资产只能含合成路径/文本，不能固化业务目录或凭据字段。"""
    forbidden = ('"api_key"', "OPENAI_API_KEY", "DEEPSEEK_API_KEY")
    corpus_text = "\n".join(
        path.read_text(encoding="utf-8")
        for path in sorted(FIXTURE_DIR.glob("*.json"))
    )
    assert not any(value in corpus_text for value in forbidden)

    tracked_contract_docs = (
        PROJECT_ROOT
        / "docs"
        / "adr"
        / "0002-windows-first-rust-scanner-core.md",
        PROJECT_ROOT / "docs" / "windows-deployment.md",
        CONTRACT_DIR / "scanner-context-v1.md",
    )
    assert all(
        "D:\\" not in path.read_text(encoding="utf-8")
        for path in tracked_contract_docs
    )

    allowed_absolute_prefixes = (
        "C:\\scanner-fixtures\\",
        "\\\\fixture-server\\",
        "\\\\?\\C:\\scanner-fixtures\\",
    )
    for path in FIXTURE_DIR.glob("*.json"):
        for value in _iter_strings(_load_json(path)):
            is_windows_absolute = (
                len(value) >= 3 and value[1:3] in {":\\", ":/"}
            ) or value.startswith("\\\\")
            if is_windows_absolute:
                assert value.startswith(allowed_absolute_prefixes)

    unc_request = _load_json(FIXTURE_DIR / "request-windows-unc.json")
    assert isinstance(unc_request, dict)
    assert unc_request["work_dir"].startswith("\\\\fixture-server\\")

def test_frozen_defaults_match_current_python_contract() -> None:
    """示例配置必须继续提供明确的 scanner settings。"""
    example = yaml.safe_load(
        (PROJECT_ROOT / "config" / "settings.example.yaml").read_text(
            encoding="utf-8"
        )
    )
    scanner_cfg = example["scanner"]
    assert scanner_cfg["allowed_extensions"] == [
        ".xlsx",
        ".xls",
        ".pptx",
        ".pdf",
        ".txt",
        ".md",
        ".docx",
        ".csv",
        ".json",
        ".log",
    ]
    assert scanner_cfg["ignored_patterns"] == ["~$*", "*.tmp"]
    assert scanner_cfg["excluded_dirs"] == []
    assert {
        key: scanner_cfg[key]
        for key in (
            "max_workers",
            "max_file_size_mb",
            "discovery_timeout_seconds",
            "file_timeout_seconds",
            "file_timeout_by_extension",
            "total_max_chars",
        )
    } == {
        "max_workers": 4,
        "max_file_size_mb": 50,
        "discovery_timeout_seconds": 30,
        "file_timeout_seconds": 30,
        "file_timeout_by_extension": {".pdf": 45, ".xlsx": 60, ".xls": 60},
        "total_max_chars": 50000,
    }
