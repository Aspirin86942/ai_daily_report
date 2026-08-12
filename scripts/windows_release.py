"""Windows native release packaging, database archival, and pointer tools."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import re
import shutil
import sqlite3
import stat
import subprocess
import sys
import zipfile
from datetime import UTC, datetime
from pathlib import Path, PurePosixPath
from typing import Any, Iterable
from urllib.parse import quote


PYTHON_VERSION = "3.13.13"
WHEEL_TAG = "cp313-cp313-win_amd64"
MANIFEST_SCHEMA = "ai_daily_windows_release_v2"
POINTER_SCHEMA = "ai_daily_release_pointer_v1"
RELEASE_PATTERN = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
GIT_PATTERN = re.compile(r"^[0-9a-f]{40}$")
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")


class ReleaseToolError(RuntimeError):
    """A release input or invariant failed validation."""


def require_supported_python(
    executable: Path | None = None,
    *,
    expected_version: str = PYTHON_VERSION,
) -> dict[str, object]:
    """Return interpreter identity or reject anything outside the exact contract."""
    if executable is None:
        identity = {
            "implementation": platform.python_implementation(),
            "version": platform.python_version(),
            "bits": 64 if sys.maxsize > 2**32 else 32,
            "platform": sys.platform,
        }
    else:
        completed = subprocess.run(
            [
                str(executable),
                "-c",
                (
                    "import json,platform,struct,sys;"
                    "print(json.dumps({'implementation':platform.python_implementation(),"
                    "'version':platform.python_version(),'bits':struct.calcsize('P')*8,"
                    "'platform':sys.platform}))"
                ),
            ],
            check=True,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
        identity = json.loads(completed.stdout)
    expected = {
        "implementation": "CPython",
        "version": expected_version,
        "bits": 64,
        "platform": "win32",
    }
    if identity != expected:
        raise ReleaseToolError(
            f"unsupported Python runtime: expected {expected}, found {identity}"
        )
    return identity


def validate_wheel(path: Path) -> None:
    """Require the non-abi3 CPython 3.13 Windows x64 wheel."""
    if not path.is_file():
        raise ReleaseToolError(f"wheel does not exist: {path}")
    pattern = re.compile(
        rf"^ai_daily_report-[A-Za-z0-9_.!+-]+-{re.escape(WHEEL_TAG)}\.whl$"
    )
    if not pattern.fullmatch(path.name):
        raise ReleaseToolError(
            f"wheel must use the exact {WHEEL_TAG} tag: {path.name}"
        )


def validate_wheel_contents(path: Path) -> None:
    """Require the native module and repaired non-system runtime libraries."""
    validate_wheel(path)
    try:
        with zipfile.ZipFile(path) as archive:
            names = set(archive.namelist())
    except zipfile.BadZipFile as exc:
        raise ReleaseToolError("native wheel is not a valid zip archive") from exc
    native = [
        name
        for name in names
        if name.startswith("ai_daily_scanner_native/")
        and name.endswith(".cp313-win_amd64.pyd")
    ]
    bundled_dlls = [
        name
        for name in names
        if name.startswith("ai_daily_report.libs/") and name.lower().endswith(".dll")
    ]
    if len(native) != 1 or not bundled_dlls:
        raise ReleaseToolError(
            "native wheel must contain one cp313 module and repaired runtime DLLs"
        )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def create_release_bundle(
    *,
    repo_root: Path,
    output_dir: Path,
    wheel_path: Path,
    office_worker_path: Path,
    release_version: str,
    git_commit: str,
    native_build_identity: str,
) -> dict[str, object]:
    """Create an exact allowlisted release directory and its hash manifest."""
    repo_root = repo_root.resolve(strict=True)
    output_dir = output_dir.resolve()
    wheel_path = wheel_path.resolve(strict=True)
    office_worker_path = office_worker_path.resolve(strict=True)
    validate_wheel_contents(wheel_path)
    if not RELEASE_PATTERN.fullmatch(release_version):
        raise ReleaseToolError("release_version is invalid")
    git_commit = git_commit.lower()
    if not GIT_PATTERN.fullmatch(git_commit):
        raise ReleaseToolError("git_commit must be a full 40-character SHA")
    if output_dir.exists():
        raise ReleaseToolError(f"output directory already exists: {output_dir}")
    if not output_dir.parent.is_dir():
        raise ReleaseToolError("output directory parent must already exist")

    hello = _read_worker_hello(office_worker_path)
    if hello.get("worker_build") != native_build_identity:
        raise ReleaseToolError("native and Office worker build identities differ")

    payload: list[tuple[Path, str]] = [
        (wheel_path, f"wheels/{wheel_path.name}"),
        (office_worker_path, "bin/ai-daily-office-parser.exe"),
    ]
    payload.extend(_application_payload(repo_root))
    destination_paths = [relative for _, relative in payload]
    if len(destination_paths) != len(set(destination_paths)):
        raise ReleaseToolError("release payload contains duplicate paths")

    output_dir.mkdir()
    try:
        for source, relative in payload:
            destination = _safe_child(output_dir, relative)
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, destination)

        records = _file_records(output_dir, destination_paths)
        manifest: dict[str, object] = {
            "schema_version": MANIFEST_SCHEMA,
            "release_version": release_version,
            "git_commit": git_commit,
            "target": "x86_64-pc-windows-msvc",
            "python": {
                "implementation": "CPython",
                "version": PYTHON_VERSION,
                "wheel_tag": WHEEL_TAG,
            },
            "native": {
                "module": "ai_daily_scanner_native",
                "build_identity": native_build_identity,
            },
            "office_worker": hello,
            "cargo_lock_sha256": sha256_file(repo_root / "rust" / "Cargo.lock"),
            "files": records,
        }
        _write_json_atomic(output_dir / "manifest.json", manifest)
        verify_release_bundle(output_dir)
        return manifest
    except BaseException:
        shutil.rmtree(output_dir, ignore_errors=True)
        raise


def verify_release_bundle(bundle_dir: Path) -> dict[str, object]:
    """Verify exact file membership, sizes, hashes, and release identities."""
    root = bundle_dir.resolve(strict=True)
    manifest_path = root / "manifest.json"
    manifest = _read_json(manifest_path)
    if manifest.get("schema_version") != MANIFEST_SCHEMA:
        raise ReleaseToolError("unsupported release manifest schema")
    python_identity = manifest.get("python")
    if python_identity != {
        "implementation": "CPython",
        "version": PYTHON_VERSION,
        "wheel_tag": WHEEL_TAG,
    }:
        raise ReleaseToolError("release manifest Python identity is invalid")
    native = manifest.get("native")
    worker = manifest.get("office_worker")
    if not isinstance(native, dict) or not isinstance(worker, dict):
        raise ReleaseToolError("release manifest identities are missing")
    if native.get("build_identity") != worker.get("worker_build"):
        raise ReleaseToolError("release manifest build identities differ")

    records = manifest.get("files")
    if not isinstance(records, list) or not records:
        raise ReleaseToolError("release manifest file list is empty")
    expected: set[str] = set()
    last_path = ""
    for record in records:
        if not isinstance(record, dict):
            raise ReleaseToolError("release manifest file record is invalid")
        relative = str(record.get("path", ""))
        _validate_relative_path(relative)
        if relative <= last_path or relative in expected:
            raise ReleaseToolError("release manifest paths are not unique and sorted")
        last_path = relative
        expected.add(relative)
        path = _safe_child(root, relative)
        if not path.is_file() or path.is_symlink():
            raise ReleaseToolError(f"release payload file is missing: {relative}")
        if record.get("size") != path.stat().st_size:
            raise ReleaseToolError(f"release payload size mismatch: {relative}")
        digest = str(record.get("sha256", ""))
        if not SHA256_PATTERN.fullmatch(digest) or digest != sha256_file(path):
            raise ReleaseToolError(f"release payload hash mismatch: {relative}")

    actual = {
        path.relative_to(root).as_posix()
        for path in root.rglob("*")
        if path.is_file() and path != manifest_path
    }
    if actual != expected:
        raise ReleaseToolError("release payload does not match the manifest allowlist")
    wheel_names = [path for path in expected if path.startswith("wheels/")]
    if len(wheel_names) != 1:
        raise ReleaseToolError("release must contain exactly one native wheel")
    validate_wheel_contents(root / wheel_names[0])
    return manifest


def archive_scanner_database(
    source_path: Path,
    archive_dir: Path,
    *,
    timestamp: datetime | None = None,
) -> dict[str, object]:
    """Create a checked read-only SQLite backup without modifying the source."""
    source = source_path.resolve(strict=True)
    archive_root = archive_dir.resolve(strict=True)
    if not source.is_file() or not archive_root.is_dir():
        raise ReleaseToolError("source database and archive directory must exist")
    moment = (timestamp or datetime.now(UTC)).astimezone(UTC)
    stamp = moment.strftime("%Y%m%dT%H%M%SZ")
    target = archive_root / f"{source.stem}-{stamp}.sqlite3"
    manifest_path = archive_root / f"{target.name}.manifest.json"
    temporary = archive_root / f".{target.name}.{os.getpid()}.tmp"
    for path in (target, manifest_path, temporary):
        if path.exists():
            raise ReleaseToolError(f"archive target already exists: {path}")

    source_hash_before = sha256_file(source)
    source_uri = f"file:{quote(source.as_posix(), safe='/:')}?mode=ro"
    destination: sqlite3.Connection | None = None
    try:
        with sqlite3.connect(source_uri, uri=True) as original:
            _require_integrity(original, "source")
            user_version = int(original.execute("PRAGMA user_version").fetchone()[0])
            destination = sqlite3.connect(temporary)
            original.backup(destination)
            destination.commit()
            _require_integrity(destination, "archive")
            destination.close()
            destination = None
        os.replace(temporary, target)
        target.chmod(stat.S_IREAD)
        if sha256_file(source) != source_hash_before:
            raise ReleaseToolError("source database changed during archival")
        manifest: dict[str, object] = {
            "schema_version": "ai_daily_scanner_db_archive_v1",
            "created_at_utc": moment.isoformat().replace("+00:00", "Z"),
            "source_name": source.name,
            "source_sha256": source_hash_before,
            "source_user_version": user_version,
            "archive_name": target.name,
            "archive_size": target.stat().st_size,
            "archive_sha256": sha256_file(target),
            "sqlite_backup_api": True,
        }
        _write_json_atomic(manifest_path, manifest)
        return manifest
    except BaseException:
        if destination is not None:
            destination.close()
        for path in (temporary, manifest_path, target):
            if path.exists():
                path.chmod(stat.S_IWRITE)
                path.unlink()
        raise


def switch_release_pointer(
    pointer_path: Path,
    *,
    release_version: str,
    scanner_db_path: str,
) -> dict[str, object]:
    """Atomically select a release while preserving the previous DB pointer."""
    current = _release_reference(release_version, scanner_db_path)
    previous: dict[str, object] | None = None
    if pointer_path.exists():
        previous = _read_pointer(pointer_path)["current"]
    pointer = {
        "schema_version": POINTER_SCHEMA,
        "current": current,
        "previous": previous,
    }
    _write_json_atomic(pointer_path, pointer)
    return pointer


def rollback_release_pointer(pointer_path: Path) -> dict[str, object]:
    """Atomically swap current and previous references; no DB is deleted."""
    existing = _read_pointer(pointer_path)
    previous = existing["previous"]
    if previous is None:
        raise ReleaseToolError("release pointer has no previous release")
    pointer = {
        "schema_version": POINTER_SCHEMA,
        "current": previous,
        "previous": existing["current"],
    }
    _write_json_atomic(pointer_path, pointer)
    return pointer


def _application_payload(repo_root: Path) -> list[tuple[Path, str]]:
    required = [
        "main.py",
        ".python-version",
        "requirements.lock",
        "config/settings.example.yaml",
        "scripts/windows_release.py",
        "scripts/archive_scanner_database.ps1",
        "scripts/update_release_pointer.ps1",
    ]
    sources = [repo_root / relative for relative in required]
    sources.extend(sorted((repo_root / "src").rglob("*.py")))
    sources.extend(
        path for path in sorted((repo_root / "templates").rglob("*")) if path.is_file()
    )
    payload: list[tuple[Path, str]] = []
    for source in sources:
        if not source.is_file():
            raise ReleaseToolError(f"required application file is missing: {source}")
        relative = source.relative_to(repo_root).as_posix()
        payload.append((source, f"app/{relative}"))
    return payload


def _read_worker_hello(worker_path: Path) -> dict[str, object]:
    completed = subprocess.run(
        [str(worker_path), "hello"],
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    hello = json.loads(completed.stdout)
    required = {
        "contract": "ai_daily_worker",
        "protocol_version": 2,
        "frame": "hello",
        "worker_contract_version": "ai_daily_worker_v2",
        "worker_kind": "office",
        "supported_operations": ["office_parse"],
    }
    for key, value in required.items():
        if hello.get(key) != value:
            raise ReleaseToolError(f"Office worker hello mismatch: {key}")
    if not str(hello.get("worker_build", "")):
        raise ReleaseToolError("Office worker build identity is empty")
    return hello


def _file_records(root: Path, relative_paths: Iterable[str]) -> list[dict[str, object]]:
    records = []
    for relative in sorted(relative_paths):
        path = _safe_child(root, relative)
        records.append(
            {
                "path": relative,
                "size": path.stat().st_size,
                "sha256": sha256_file(path),
            }
        )
    return records


def _safe_child(root: Path, relative: str) -> Path:
    _validate_relative_path(relative)
    path = (root / Path(*PurePosixPath(relative).parts)).resolve()
    if not path.is_relative_to(root.resolve()):
        raise ReleaseToolError(f"path escapes release root: {relative}")
    return path


def _validate_relative_path(relative: str) -> None:
    pure = PurePosixPath(relative)
    if (
        not relative
        or "\\" in relative
        or ":" in relative
        or pure.is_absolute()
        or any(part in {"", ".", ".."} for part in pure.parts)
        or pure.as_posix() != relative
    ):
        raise ReleaseToolError(f"unsafe relative path: {relative}")


def _require_integrity(connection: sqlite3.Connection, label: str) -> None:
    rows = connection.execute("PRAGMA integrity_check").fetchall()
    foreign_keys = connection.execute("PRAGMA foreign_key_check").fetchall()
    if rows != [("ok",)] or foreign_keys:
        raise ReleaseToolError(f"{label} database integrity check failed")


def _release_reference(release_version: str, scanner_db_path: str) -> dict[str, object]:
    if not RELEASE_PATTERN.fullmatch(release_version):
        raise ReleaseToolError("release_version is invalid")
    _validate_relative_path(scanner_db_path)
    if not scanner_db_path.startswith("shared/data/db/") or not scanner_db_path.endswith(
        ".sqlite3"
    ):
        raise ReleaseToolError("scanner_db_path must stay under shared/data/db")
    return {
        "release_version": release_version,
        "release_path": f"releases/{release_version}",
        "scanner_db_path": scanner_db_path,
    }


def _read_pointer(pointer_path: Path) -> dict[str, Any]:
    pointer = _read_json(pointer_path)
    if pointer.get("schema_version") != POINTER_SCHEMA:
        raise ReleaseToolError("release pointer schema is invalid")
    for key in ("current", "previous"):
        value = pointer.get(key)
        if key == "previous" and value is None:
            continue
        if not isinstance(value, dict):
            raise ReleaseToolError(f"release pointer {key} is invalid")
        expected = _release_reference(
            str(value.get("release_version", "")),
            str(value.get("scanner_db_path", "")),
        )
        if value != expected:
            raise ReleaseToolError(f"release pointer {key} is not canonical")
    return pointer


def _read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise ReleaseToolError(f"invalid JSON file: {path}") from exc
    if not isinstance(value, dict):
        raise ReleaseToolError(f"JSON root must be an object: {path}")
    return value


def _write_json_atomic(path: Path, value: object) -> None:
    if not path.parent.is_dir():
        raise ReleaseToolError(f"output parent does not exist: {path.parent}")
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    text = json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    try:
        temporary.write_text(text, encoding="utf-8", newline="\n")
        os.replace(temporary, path)
    finally:
        if temporary.exists():
            temporary.unlink()


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    runtime = commands.add_parser("verify-runtime")
    runtime.add_argument("--python", type=Path)
    runtime.add_argument("--expected-version", default=PYTHON_VERSION)
    package = commands.add_parser("package")
    package.add_argument("--repo-root", type=Path, required=True)
    package.add_argument("--output-dir", type=Path, required=True)
    package.add_argument("--wheel", type=Path, required=True)
    package.add_argument("--office-worker", type=Path, required=True)
    package.add_argument("--release-version", required=True)
    package.add_argument("--git-commit", required=True)
    package.add_argument("--native-build-identity", required=True)
    verify = commands.add_parser("verify-bundle")
    verify.add_argument("--bundle-dir", type=Path, required=True)
    archive = commands.add_parser("archive-db")
    archive.add_argument("--source", type=Path, required=True)
    archive.add_argument("--archive-dir", type=Path, required=True)
    switch = commands.add_parser("pointer-switch")
    switch.add_argument("--pointer", type=Path, required=True)
    switch.add_argument("--release-version", required=True)
    switch.add_argument("--scanner-db-path", required=True)
    switch.add_argument("--apply", action="store_true")
    rollback = commands.add_parser("pointer-rollback")
    rollback.add_argument("--pointer", type=Path, required=True)
    rollback.add_argument("--apply", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.command == "verify-runtime":
            value = require_supported_python(
                args.python, expected_version=args.expected_version
            )
        elif args.command == "package":
            value = create_release_bundle(
                repo_root=args.repo_root,
                output_dir=args.output_dir,
                wheel_path=args.wheel,
                office_worker_path=args.office_worker,
                release_version=args.release_version,
                git_commit=args.git_commit,
                native_build_identity=args.native_build_identity,
            )
        elif args.command == "verify-bundle":
            value = verify_release_bundle(args.bundle_dir)
        elif args.command == "archive-db":
            value = archive_scanner_database(args.source, args.archive_dir)
        elif args.command == "pointer-switch":
            if not args.apply:
                raise ReleaseToolError("pointer switch requires --apply")
            value = switch_release_pointer(
                args.pointer,
                release_version=args.release_version,
                scanner_db_path=args.scanner_db_path,
            )
        else:
            if not args.apply:
                raise ReleaseToolError("pointer rollback requires --apply")
            value = rollback_release_pointer(args.pointer)
    except (ReleaseToolError, OSError, sqlite3.Error, subprocess.SubprocessError) as exc:
        print(json.dumps({"status": "error", "message": str(exc)}), file=sys.stderr)
        return 1
    print(json.dumps({"status": "ok", "result": value}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
