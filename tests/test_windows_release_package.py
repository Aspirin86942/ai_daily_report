"""Windows release archive, trusted verification, and pointer safety tests."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import subprocess
import sys
import zipfile
from datetime import date
from pathlib import Path
from uuid import uuid4

import pytest


ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"
ARCHIVE_ROOT = "ai-daily-report-windows-x64"
IS_WINDOWS = sys.platform == "win32" and shutil.which("pwsh") is not None


def _run_ps1(
    script: str,
    *arguments: str | Path,
    cwd: Path = ROOT,
    env: dict[str, str] | None = None,
    timeout: int = 180,
) -> subprocess.CompletedProcess[str]:
    command = [
        "pwsh",
        "-NoProfile",
        "-File",
        str(SCRIPTS / script),
        *(str(argument) for argument in arguments),
    ]
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        text=True,
        encoding="utf-8",
        errors="replace",
        capture_output=True,
        check=False,
        timeout=timeout,
    )


@pytest.fixture(scope="module")
def windows_package(tmp_path_factory: pytest.TempPathFactory) -> Path:
    if not IS_WINDOWS:
        pytest.skip("Windows PowerShell release contract")
    scanner = ROOT / "rust" / "target" / "release" / "ai-daily-scanner.exe"
    office = (
        ROOT
        / "rust"
        / "target"
        / "release"
        / "ai-daily-office-parser.exe"
    )
    if not scanner.is_file() or not office.is_file():
        pytest.skip("Rust release binaries are required")
    archive = tmp_path_factory.mktemp("windows-package") / "release.zip"
    result = _run_ps1(
        "package_windows.ps1",
        "-OutputPath",
        archive,
        "-ReleaseVersion",
        "pytest-a",
    )
    assert result.returncode == 0, result.stdout + result.stderr
    return archive


def _archive_entries(archive: Path) -> dict[str, bytes]:
    with zipfile.ZipFile(archive) as source:
        return {info.filename: source.read(info) for info in source.infolist()}


def _rewrite_archive(
    source: Path,
    destination: Path,
    *,
    replace: dict[str, bytes] | None = None,
    additions: list[tuple[zipfile.ZipInfo | str, bytes]] | None = None,
) -> None:
    replacements = replace or {}
    with zipfile.ZipFile(source) as reader, zipfile.ZipFile(
        destination,
        "w",
        compression=zipfile.ZIP_DEFLATED,
    ) as writer:
        for info in reader.infolist():
            payload = replacements.get(info.filename, reader.read(info))
            writer.writestr(info, payload)
        for name, payload in additions or []:
            writer.writestr(name, payload)


def _run_installed_scanner_smoke(
    install_root: Path,
    work_dir: Path,
    cwd: Path,
    env: dict[str, str],
) -> None:
    current = json.loads((install_root / "current.json").read_text("utf-8"))
    release = install_root / current["release_path"]
    request = json.loads(
        (ROOT / "tests" / "fixtures" / "scanner_contract" / "v1" / "request.json")
        .read_text(encoding="utf-8")
    )
    request["request_id"] = str(uuid4())
    request["work_dir"] = str(work_dir)
    request["start_date"] = date.today().isoformat()
    request["end_date"] = date.today().isoformat()
    request["scan_db_path"] = str(
        install_root / "shared" / "data" / "db" / "scan_index_v2.sqlite3"
    )
    request["adapters"] = {
        "office_worker_path": str(
            release
            / "rust"
            / "target"
            / "release"
            / "ai-daily-office-parser.exe"
        ),
        "python_executable": str(release / ".venv" / "Scripts" / "python.exe"),
        "python_module_root": str(release),
        "python_document_worker_module": "src.workers.document_parser_worker",
    }
    result = subprocess.run(
        [
            str(
                release
                / "rust"
                / "target"
                / "release"
                / "ai-daily-scanner.exe"
            ),
            "build-context",
        ],
        input=json.dumps(request, ensure_ascii=False),
        cwd=cwd,
        env=env,
        text=True,
        encoding="utf-8",
        errors="replace",
        capture_output=True,
        check=False,
        timeout=180,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    response = json.loads(result.stdout)
    assert response["status"] in {"ok", "partial"}


def test_package_manifest_and_archive_entry_set_are_exact(windows_package: Path):
    entries = _archive_entries(windows_package)
    manifest_name = f"{ARCHIVE_ROOT}/manifest.json"
    sums_name = f"{ARCHIVE_ROOT}/SHA256SUMS"
    manifest = json.loads(entries[manifest_name])
    payload_paths = [record["path"] for record in manifest["files"]]

    assert manifest["schema_version"] == "ai_daily_windows_package_v1"
    assert manifest["target_triple"] == "x86_64-pc-windows-msvc"
    assert manifest["contract_version"] == "ai_daily_context/v1"
    assert payload_paths == sorted(payload_paths)
    assert len(payload_paths) == len(set(payload_paths))
    assert "manifest.json" not in payload_paths
    assert "SHA256SUMS" not in payload_paths
    assert set(entries) == {
        manifest_name,
        sums_name,
        *(f"{ARCHIVE_ROOT}/{path}" for path in payload_paths),
    }
    assert "config/settings.yaml" not in payload_paths
    assert "config/.secrets.yaml" not in payload_paths
    assert not any(path.startswith("data/") for path in payload_paths)
    assert not any(path.startswith("logs/") for path in payload_paths)

    sums = entries[sums_name].decode("utf-8").splitlines()
    expected = [
        f"{hashlib.sha256(entries[manifest_name]).hexdigest()}  manifest.json"
    ]
    for record in manifest["files"]:
        entry = entries[f"{ARCHIVE_ROOT}/{record['path']}"]
        assert len(entry) == record["size"]
        assert hashlib.sha256(entry).hexdigest() == record["sha256"]
        expected.append(f"{record['sha256']}  {record['path']}")
    assert sums == expected


def test_trusted_verifier_accepts_the_untampered_archive(windows_package: Path):
    result = _run_ps1(
        "verify_windows_package.ps1",
        "-ArchivePath",
        windows_package,
    )

    assert result.returncode == 0, result.stdout + result.stderr
    assert "status" in result.stdout and "ok" in result.stdout


@pytest.mark.parametrize(
    "payload_path",
    [
        "main.py",
        "templates/system_prompt.md",
        "requirements.lock",
        "scripts/deploy_windows.ps1",
        "rust/target/release/ai-daily-scanner.exe",
    ],
)
def test_tampered_payload_fails_before_install_pointer_or_code_execution(
    windows_package: Path,
    tmp_path: Path,
    payload_path: str,
):
    entry_name = f"{ARCHIVE_ROOT}/{payload_path}"
    entries = _archive_entries(windows_package)
    payload = entries[entry_name] + b"\nTAMPERED"
    if payload_path.endswith(".ps1"):
        payload += (
            b"\n[IO.File]::WriteAllText((Join-Path "
            b"$env:DAILY_REPORT_INSTALL_ROOT 'UNTRUSTED_EXECUTED'), 'x')\n"
        )
    tampered = tmp_path / "tampered.zip"
    _rewrite_archive(
        windows_package,
        tampered,
        replace={entry_name: payload},
    )
    install_root = tmp_path / "安装 root"
    config_dir = install_root / "shared" / "config"
    config_dir.mkdir(parents=True)
    shutil.copyfile(
        ROOT / "config" / "settings.example.yaml",
        config_dir / "settings.windows.yaml",
    )
    work_dir = tmp_path / "synthetic work"
    work_dir.mkdir()
    env = os.environ.copy()
    env["DAILY_REPORT_PATHS__WORK_DIR"] = str(work_dir)
    env["DEEPSEEK_API_KEY"] = "pytest-placeholder"
    env["AI_DAILY_TEST_FORBID_LLM"] = "1"

    result = _run_ps1(
        "install_windows_release.ps1",
        "-ArchivePath",
        tampered,
        "-InstallRoot",
        install_root,
        "-Python",
        sys.executable,
        env=env,
    )

    assert result.returncode != 0
    assert not (install_root / "current.json").exists()
    assert not (install_root / "UNTRUSTED_EXECUTED").exists()


def test_manifest_traversal_and_extra_entry_fail_before_pointer_change(
    windows_package: Path,
    tmp_path: Path,
):
    entries = _archive_entries(windows_package)
    manifest_name = f"{ARCHIVE_ROOT}/manifest.json"
    manifest = json.loads(entries[manifest_name])
    manifest["files"][0]["path"] = "../escape.py"
    unsafe_manifest = tmp_path / "unsafe-manifest.zip"
    _rewrite_archive(
        windows_package,
        unsafe_manifest,
        replace={
            manifest_name: (
                json.dumps(manifest, ensure_ascii=False).encode("utf-8") + b"\n"
            )
        },
    )
    extra = tmp_path / "extra.zip"
    _rewrite_archive(
        windows_package,
        extra,
        additions=[(f"{ARCHIVE_ROOT}/extra.txt", b"extra")],
    )

    for archive in (unsafe_manifest, extra):
        result = _run_ps1(
            "verify_windows_package.ps1",
            "-ArchivePath",
            archive,
        )
        assert result.returncode != 0


@pytest.mark.parametrize(
    "entry_name",
    [
        "C:/absolute.txt",
        "//server/share/unc.txt",
        f"{ARCHIVE_ROOT}/../traversal.txt",
        f"{ARCHIVE_ROOT}/payload.txt:stream",
        f"{ARCHIVE_ROOT}/MAIN.py",
    ],
)
def test_verifier_rejects_unsafe_or_case_colliding_archive_names(
    windows_package: Path,
    tmp_path: Path,
    entry_name: str,
):
    tampered = tmp_path / "unsafe-entry.zip"
    _rewrite_archive(
        windows_package,
        tampered,
        additions=[(entry_name, b"unsafe")],
    )

    result = _run_ps1(
        "verify_windows_package.ps1",
        "-ArchivePath",
        tampered,
    )

    assert result.returncode != 0


def test_verifier_rejects_duplicate_and_symlink_entries(
    windows_package: Path,
    tmp_path: Path,
):
    duplicate = tmp_path / "duplicate.zip"
    with pytest.warns(UserWarning, match="Duplicate name"):
        _rewrite_archive(
            windows_package,
            duplicate,
            additions=[(f"{ARCHIVE_ROOT}/main.py", b"duplicate")],
        )
    symlink_info = zipfile.ZipInfo(f"{ARCHIVE_ROOT}/link")
    symlink_info.create_system = 3
    symlink_info.external_attr = 0o120777 << 16
    symlink = tmp_path / "symlink.zip"
    _rewrite_archive(
        windows_package,
        symlink,
        additions=[(symlink_info, b"main.py")],
    )

    for archive in (duplicate, symlink):
        result = _run_ps1(
            "verify_windows_package.ps1",
            "-ArchivePath",
            archive,
        )
        assert result.returncode != 0


def test_launcher_rejects_relative_root_pointer_escape_and_missing_shared_dirs(
    tmp_path: Path,
):
    relative = _run_ps1(
        "run_current_release.ps1",
        "-InstallRoot",
        "relative/install",
        "doctor",
        "--strict",
    )
    assert relative.returncode != 0

    install_root = tmp_path / "installed root"
    (install_root / "releases" / "v1").mkdir(parents=True)
    current = {
        "schema_version": "ai_daily_current_v1",
        "release_version": "v1",
        "release_path": "releases/../escape",
        "previous_release_version": None,
    }
    (install_root / "current.json").write_text(
        json.dumps(current),
        encoding="utf-8",
    )
    escaped = _run_ps1(
        "run_current_release.ps1",
        "-InstallRoot",
        install_root,
        "doctor",
        "--strict",
    )
    assert escaped.returncode != 0

    current["release_path"] = "releases/v1"
    (install_root / "current.json").write_text(
        json.dumps(current),
        encoding="utf-8",
    )
    missing_shared = _run_ps1(
        "run_current_release.ps1",
        "-InstallRoot",
        install_root,
        "doctor",
        "--strict",
    )
    assert missing_shared.returncode != 0


@pytest.mark.skipif(
    os.getenv("AI_DAILY_RUN_WINDOWS_RELEASE_E2E") != "1",
    reason="enabled only by the mandatory Windows release workflow",
)
def test_clean_install_switch_rollback_and_shared_state_e2e(
    windows_package: Path,
    tmp_path: Path,
):
    """The workflow enables this slow clean-venv installed-package proof."""
    second = tmp_path / "release-b.zip"
    packaged = _run_ps1(
        "package_windows.ps1",
        "-OutputPath",
        second,
        "-ReleaseVersion",
        "pytest-b",
    )
    assert packaged.returncode == 0, packaged.stdout + packaged.stderr

    install_root = tmp_path / f"安装 根 {uuid4().hex}"
    config_dir = install_root / "shared" / "config"
    data_dir = install_root / "shared" / "data"
    config_dir.mkdir(parents=True)
    data_dir.mkdir(parents=True)
    settings = config_dir / "settings.windows.yaml"
    shutil.copyfile(ROOT / "config" / "settings.example.yaml", settings)
    sentinel = data_dir / "shared-preserve.txt"
    sentinel.write_text("preserve shared state", encoding="utf-8")
    initial_hashes = {
        path: hashlib.sha256(path.read_bytes()).hexdigest()
        for path in (settings, sentinel)
    }
    work_dir = tmp_path / "业务合成目录"
    work_dir.mkdir()
    (work_dir / "sample.txt").write_text("synthetic only", encoding="utf-8")
    unrelated_cwd = tmp_path / "unrelated cwd"
    unrelated_cwd.mkdir()
    env = os.environ.copy()
    env["DAILY_REPORT_PATHS__WORK_DIR"] = str(work_dir)
    env["DEEPSEEK_API_KEY"] = "pytest-placeholder"
    env["AI_DAILY_TEST_FORBID_LLM"] = "1"

    for archive, version in (
        (windows_package, "pytest-a"),
        (second, "pytest-b"),
    ):
        installed = _run_ps1(
            "install_windows_release.ps1",
            "-ArchivePath",
            archive,
            "-InstallRoot",
            install_root,
            "-Python",
            sys.executable,
            env=env,
            timeout=600,
        )
        assert installed.returncode == 0, installed.stdout + installed.stderr
        current = json.loads((install_root / "current.json").read_text("utf-8"))
        assert current["release_version"] == version
        for path, digest in initial_hashes.items():
            assert hashlib.sha256(path.read_bytes()).hexdigest() == digest

    launcher = install_root / "run_current_release.ps1"
    for arguments in (("doctor", "--strict"), ("list",)):
        result = subprocess.run(
            [
                "pwsh",
                "-NoProfile",
                "-File",
                str(launcher),
                "-InstallRoot",
                str(install_root),
                *arguments,
            ],
            cwd=unrelated_cwd,
            env=env,
            text=True,
            encoding="utf-8",
            errors="replace",
            capture_output=True,
            check=False,
            timeout=180,
        )
        assert result.returncode == 0, result.stdout + result.stderr

    assert (install_root / "shared" / "data" / "db" / "reports.sqlite3").is_file()
    _run_installed_scanner_smoke(install_root, work_dir, unrelated_cwd, env)
    assert (
        install_root / "shared" / "data" / "db" / "scan_index_v2.sqlite3"
    ).is_file()
    assert any((install_root / "shared" / "logs").glob("*.log"))
    assert not any((install_root / "releases").glob("**/logs/*.log"))
    assert not any((install_root / "releases").glob("**/*.sqlite3"))
    for path, digest in initial_hashes.items():
        assert hashlib.sha256(path.read_bytes()).hexdigest() == digest

    rollback = subprocess.run(
        [
            "pwsh",
            "-NoProfile",
            "-File",
            str(install_root / "rollback_windows_release.ps1"),
            "-InstallRoot",
            str(install_root),
        ],
        cwd=unrelated_cwd,
        env=env,
        text=True,
        encoding="utf-8",
        errors="replace",
        capture_output=True,
        check=False,
        timeout=180,
    )
    assert rollback.returncode == 0, rollback.stdout + rollback.stderr
    current = json.loads((install_root / "current.json").read_text("utf-8"))
    assert current["release_version"] == "pytest-a"
    rerun = subprocess.run(
        [
            "pwsh",
            "-NoProfile",
            "-File",
            str(launcher),
            "-InstallRoot",
            str(install_root),
            "doctor",
            "--strict",
        ],
        cwd=unrelated_cwd,
        env=env,
        text=True,
        encoding="utf-8",
        errors="replace",
        capture_output=True,
        check=False,
        timeout=180,
    )
    assert rerun.returncode == 0, rerun.stdout + rerun.stderr
    _run_installed_scanner_smoke(install_root, work_dir, unrelated_cwd, env)
    for path, digest in initial_hashes.items():
        assert hashlib.sha256(path.read_bytes()).hexdigest() == digest
