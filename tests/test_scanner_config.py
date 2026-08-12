"""scanner_config 拆分与模块边界等价性测试。"""

from __future__ import annotations

from types import SimpleNamespace

import pytest

from src.services.scanner_config import (
    UnknownScannerContractFieldsError,
    extract_scanner_profile,
)


def test_extract_profile_passes_explicit_contract_leaves() -> None:
    profile = extract_scanner_profile(
        SimpleNamespace(
            allowed_extensions=[".txt", ".md"],
            max_workers=4,
            total_max_chars=50_000,
        )
    )

    assert profile == {
        "schema_version": "scanner_profile_v1",
        "allowed_extensions": [".txt", ".md"],
        "max_workers": 4,
        "total_max_chars": 50_000,
    }


def test_extract_profile_rejects_unknown_leaves() -> None:
    with pytest.raises(UnknownScannerContractFieldsError) as exc_info:
        extract_scanner_profile(SimpleNamespace(unknown_leaf=1))

    assert exc_info.value.fields == ("unknown_leaf",)


def test_extract_profile_keeps_infrastructure_out_of_wire() -> None:
    profile = extract_scanner_profile(
        SimpleNamespace(rust_scanner_bin="bin/x", engine="rust_v2")
    )

    assert profile == {"schema_version": "scanner_profile_v1"}


def test_config_delegates_profile_to_scanner_config(monkeypatch) -> None:
    from src.core.config import Config
    from src.services import scanner_config

    scanner = SimpleNamespace(max_workers=2)
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(scanner=scanner)
    seen = []

    def extract_spy(scanner_settings):
        seen.append(scanner_settings)
        return {"delegated": True}

    monkeypatch.setattr(scanner_config, "extract_scanner_profile", extract_spy)

    assert cfg.scanner_contract_profile() == {"delegated": True}
    assert seen == [scanner]
