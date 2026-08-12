"""scanner_config 拆分与模块边界等价性测试。"""

from __future__ import annotations

from types import SimpleNamespace

import pytest

from src.services.scanner_config import UnknownScannerSettingsError, extract_scanner_settings


def test_extract_settings_passes_explicit_leaves() -> None:
    settings = extract_scanner_settings(
        SimpleNamespace(
            allowed_extensions=[".txt", ".md"],
            max_workers=4,
            total_max_chars=50_000,
        )
    )

    assert settings == {
        "allowed_extensions": [".txt", ".md"],
        "max_workers": 4,
        "total_max_chars": 50_000,
    }


def test_extract_settings_rejects_unknown_leaves() -> None:
    with pytest.raises(UnknownScannerSettingsError) as exc_info:
        extract_scanner_settings(SimpleNamespace(unknown_leaf=1))

    assert exc_info.value.fields == ("unknown_leaf",)


def test_extract_settings_keeps_paths_out_of_native_settings() -> None:
    settings = extract_scanner_settings(
        SimpleNamespace(index_db_path="state/scan_index_v3.sqlite3", office_worker_path="bin/x")
    )

    assert settings == {}


def test_config_delegates_settings_to_scanner_config(monkeypatch) -> None:
    from src.core.config import Config
    from src.services import scanner_config

    scanner = SimpleNamespace(max_workers=2)
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(scanner=scanner)
    seen = []

    def extract_spy(scanner_settings):
        seen.append(scanner_settings)
        return {"delegated": True}

    monkeypatch.setattr(scanner_config, "extract_scanner_settings", extract_spy)

    assert cfg.scanner_settings() == {"delegated": True}
    assert seen == [scanner]
