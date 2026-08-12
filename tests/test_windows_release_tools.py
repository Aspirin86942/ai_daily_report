from __future__ import annotations

import json
import sqlite3
import stat
import zipfile
from datetime import UTC, datetime
from pathlib import Path

import pytest

from windows_release import (
    MANIFEST_SCHEMA,
    ReleaseToolError,
    archive_scanner_database,
    require_supported_python,
    rollback_release_pointer,
    switch_release_pointer,
    validate_wheel,
    validate_wheel_contents,
    verify_release_bundle,
)


def test_runtime_gate_accepts_exact_project_python_and_rejects_wrong_patch() -> None:
    assert require_supported_python()["version"] == "3.13.13"
    with pytest.raises(ReleaseToolError, match="unsupported Python runtime"):
        require_supported_python(expected_version="3.13.12")


def test_wheel_gate_requires_exact_non_abi3_windows_tag(tmp_path: Path) -> None:
    exact = tmp_path / "ai_daily_report-5.0.0-cp313-cp313-win_amd64.whl"
    exact.write_bytes(b"wheel")
    validate_wheel(exact)
    abi3 = tmp_path / "ai_daily_report-5.0.0-cp313-abi3-win_amd64.whl"
    abi3.write_bytes(b"wheel")
    with pytest.raises(ReleaseToolError, match="exact cp313"):
        validate_wheel(abi3)


def test_wheel_content_gate_requires_native_module_and_repaired_dll(tmp_path: Path) -> None:
    wheel = tmp_path / "ai_daily_report-5.0.0-cp313-cp313-win_amd64.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr(
            "ai_daily_scanner_native/ai_daily_scanner_native.cp313-win_amd64.pyd",
            b"native",
        )
    with pytest.raises(ReleaseToolError, match="repaired runtime DLLs"):
        validate_wheel_contents(wheel)
    with zipfile.ZipFile(wheel, "a") as archive:
        archive.writestr("ai_daily_report.libs/zlib-test.dll", b"runtime")
    validate_wheel_contents(wheel)


def test_bundle_verifier_rejects_tamper_and_extra_file(tmp_path: Path) -> None:
    bundle = tmp_path / "bundle"
    wheel = bundle / "wheels" / "ai_daily_report-5.0.0-cp313-cp313-win_amd64.whl"
    worker = bundle / "bin" / "ai-daily-office-parser.exe"
    wheel.parent.mkdir(parents=True)
    worker.parent.mkdir(parents=True)
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr(
            "ai_daily_scanner_native/ai_daily_scanner_native.cp313-win_amd64.pyd",
            b"native",
        )
        archive.writestr("ai_daily_report.libs/zlib-test.dll", b"runtime")
    worker.write_bytes(b"worker")
    records = []
    for path in (worker, wheel):
        relative = path.relative_to(bundle).as_posix()
        from windows_release import sha256_file

        records.append(
            {"path": relative, "size": path.stat().st_size, "sha256": sha256_file(path)}
        )
    manifest = {
        "schema_version": MANIFEST_SCHEMA,
        "release_version": "test",
        "git_commit": "a" * 40,
        "target": "x86_64-pc-windows-msvc",
        "python": {
            "implementation": "CPython",
            "version": "3.13.13",
            "wheel_tag": "cp313-cp313-win_amd64",
        },
        "native": {"module": "ai_daily_scanner_native", "build_identity": "build"},
        "office_worker": {"worker_build": "build"},
        "cargo_lock_sha256": "b" * 64,
        "files": sorted(records, key=lambda item: item["path"]),
    }
    (bundle / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
    verify_release_bundle(bundle)
    extra = bundle / "settings.windows.yaml"
    extra.write_text("secret: forbidden", encoding="utf-8")
    with pytest.raises(ReleaseToolError, match="allowlist"):
        verify_release_bundle(bundle)
    extra.unlink()
    wheel.write_bytes(b"tampered")
    with pytest.raises(ReleaseToolError, match="size mismatch|hash mismatch"):
        verify_release_bundle(bundle)


def test_sqlite_backup_is_integrity_checked_read_only_and_source_unchanged(
    tmp_path: Path,
) -> None:
    source = tmp_path / "previous_scan_index.sqlite3"
    with sqlite3.connect(source) as connection:
        connection.execute("CREATE TABLE evidence(id INTEGER PRIMARY KEY, value TEXT)")
        connection.execute("INSERT INTO evidence(value) VALUES ('kept')")
        connection.execute("PRAGMA user_version=2")
    before = source.read_bytes()
    archive_dir = tmp_path / "archive"
    archive_dir.mkdir()
    manifest = archive_scanner_database(
        source,
        archive_dir,
        timestamp=datetime(2026, 8, 12, 1, 2, 3, tzinfo=UTC),
    )

    assert source.read_bytes() == before
    archive = archive_dir / str(manifest["archive_name"])
    assert archive.is_file()
    assert not (archive.stat().st_mode & stat.S_IWRITE)
    assert manifest["source_user_version"] == 2
    assert manifest["sqlite_backup_api"] is True
    with sqlite3.connect(f"file:{archive.as_posix()}?mode=ro", uri=True) as connection:
        assert connection.execute("SELECT value FROM evidence").fetchone() == ("kept",)


def test_corrupt_sqlite_is_rejected_without_archive_artifacts(tmp_path: Path) -> None:
    source = tmp_path / "broken.sqlite3"
    source.write_bytes(b"not sqlite")
    archive = tmp_path / "archive"
    archive.mkdir()
    with pytest.raises((ReleaseToolError, sqlite3.DatabaseError)):
        archive_scanner_database(source, archive)
    assert list(archive.iterdir()) == []


def test_release_pointer_rollback_restores_old_database_without_deleting_v3(
    tmp_path: Path,
) -> None:
    pointer = tmp_path / "current.json"
    switch_release_pointer(
        pointer,
        release_version="old",
        scanner_db_path="shared/data/db/previous_scan_index.sqlite3",
    )
    switched = switch_release_pointer(
        pointer,
        release_version="new",
        scanner_db_path="shared/data/db/scan_index_v3.sqlite3",
    )
    assert switched["current"]["release_version"] == "new"
    assert switched["previous"]["scanner_db_path"].endswith(
        "previous_scan_index.sqlite3"
    )

    rolled_back = rollback_release_pointer(pointer)
    assert rolled_back["current"]["release_version"] == "old"
    assert rolled_back["previous"]["scanner_db_path"].endswith("scan_index_v3.sqlite3")
    assert json.loads(pointer.read_text(encoding="utf-8")) == rolled_back


def test_release_pointer_rejects_database_path_escape(tmp_path: Path) -> None:
    with pytest.raises(ReleaseToolError, match="unsafe|shared/data/db"):
        switch_release_pointer(
            tmp_path / "current.json",
            release_version="bad",
            scanner_db_path="../real.sqlite3",
        )
