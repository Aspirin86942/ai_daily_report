"""Offline Windows historical artifact maintenance: frozen plans, compression, verification and rollback."""

import argparse
import contextlib
import ctypes
from ctypes import wintypes
from datetime import datetime, timedelta, timezone
import hashlib
import json
import msvcrt
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
from typing import BinaryIO
import uuid


ALLOWED_ROOTS = (
    ".artifacts/rust-migration/archive-benchmark-final-20261017-04/incremental/data/archive/extractions",
    ".artifacts/rust-migration/archive-benchmark-final-20261017/incremental/data/archive/extractions",
)
EVIDENCE_ROOT = ".artifacts/rust-migration/historical-artifact-compression"
TARGET_FILENAMES = frozenset(("body.md", "units.jsonl", "units.idx"))
HASH_CHUNK_BYTES = 1_048_576
BATCH_BYTES = 268_435_456
BATCH_FILES = 100
PAUSE_SECONDS = 300
TASK_NAMES = ("AI Daily Report", "AI Daily Report Health")
REPARSE = 0x400
COMPRESSED = 0x800
ENCRYPTED = 0x4000
SKIP_REASONS = frozenset(("hardlink", "encrypted", "already_compressed", "reparse", "unstable"))
MANIFEST_FIELDS = frozenset(("schema_version", "plan_id", "created_at_utc", "project_root",
                             "allowed_roots", "eligible", "skipped", "totals"))
ENTRY_FIELDS = frozenset(("relative_path", "file_id", "nlink", "logical_bytes", "mtime_ns",
                          "attributes", "was_compressed", "sha256", "allocated_before_bytes"))


class ToolError(Exception):
    def __init__(self, code, message, *, status="failed"):
        super().__init__(message)
        self.code = code
        self.status = status


class _HandleInformation(ctypes.Structure):
    _fields_ = [
        ("attributes", wintypes.DWORD), ("creation_time", wintypes.FILETIME),
        ("access_time", wintypes.FILETIME), ("write_time", wintypes.FILETIME),
        ("volume_serial", wintypes.DWORD), ("size_high", wintypes.DWORD),
        ("size_low", wintypes.DWORD), ("nlink", wintypes.DWORD),
        ("index_high", wintypes.DWORD), ("index_low", wintypes.DWORD),
    ]


class _CompressionInformation(ctypes.Structure):
    _fields_ = [
        ("allocated_bytes", ctypes.c_longlong), ("format", wintypes.WORD),
        ("unit_shift", wintypes.BYTE), ("chunk_shift", wintypes.BYTE),
        ("cluster_shift", wintypes.BYTE), ("reserved", wintypes.BYTE * 3),
    ]


_kernel = ctypes.WinDLL("kernel32", use_last_error=True)
_kernel.CreateFileW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.LPVOID,
                                wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
_kernel.CreateFileW.restype = wintypes.HANDLE
_kernel.CloseHandle.argtypes = [wintypes.HANDLE]
_kernel.CloseHandle.restype = wintypes.BOOL
_kernel.CreateMutexW.argtypes = [wintypes.LPVOID, wintypes.BOOL, wintypes.LPCWSTR]
_kernel.CreateMutexW.restype = wintypes.HANDLE
_kernel.GetFileInformationByHandle.argtypes = [wintypes.HANDLE, ctypes.POINTER(_HandleInformation)]
_kernel.GetFileInformationByHandle.restype = wintypes.BOOL
_kernel.GetFileInformationByHandleEx.argtypes = [wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD]
_kernel.GetFileInformationByHandleEx.restype = wintypes.BOOL
_kernel.GetFinalPathNameByHandleW.argtypes = [wintypes.HANDLE, wintypes.LPWSTR, wintypes.DWORD, wintypes.DWORD]
_kernel.GetFinalPathNameByHandleW.restype = wintypes.DWORD
_kernel.DeviceIoControl.argtypes = [
    wintypes.HANDLE, wintypes.DWORD, wintypes.LPVOID, wintypes.DWORD,
    wintypes.LPVOID, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID,
]
_kernel.DeviceIoControl.restype = wintypes.BOOL
_kernel.SetFileTime.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.FILETIME),
                               ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME)]
_kernel.SetFileTime.restype = wintypes.BOOL
_kernel.GetDiskFreeSpaceW.argtypes = [wintypes.LPCWSTR] + [ctypes.POINTER(wintypes.DWORD)] * 4
_kernel.GetDiskFreeSpaceW.restype = wintypes.BOOL


def set_compression(file: BinaryIO, enabled: bool) -> None:
    """Set NTFS LZNT1 (or NONE) on the caller's already protected file handle."""
    file.flush()
    compression = wintypes.USHORT(2 if enabled else 0)
    returned = wintypes.DWORD()
    if not _kernel.DeviceIoControl(
        msvcrt.get_osfhandle(file.fileno()), 0x9C040,
        ctypes.byref(compression), ctypes.sizeof(compression), None, 0,
        ctypes.byref(returned), None,
    ):
        raise ctypes.WinError(ctypes.get_last_error())


def _absolute_path(value):
    path = Path(value)
    if (not path.is_absolute() or len(path.drive) != 2 or path.drive[1] != ":"
            or ".." in path.parts or any(":" in part for part in path.parts[1:])):
        raise ToolError("SCOPE_REJECTED", "An explicit absolute local-volume path is required")
    if any(part.endswith((".", " ")) for part in path.parts[1:]):
        raise ToolError("SCOPE_REJECTED", "Windows path aliases are not allowed")
    return path


def _no_reparse(path, *, missing_ok=False):
    for ancestor in (*reversed(path.parents), path):
        try:
            info = os.lstat(ancestor)
        except FileNotFoundError:
            if missing_ok:
                continue
            raise ToolError("SCOPE_REJECTED", f"Missing scope component: {ancestor}") from None
        if info.st_file_attributes & REPARSE:
            raise ToolError("SCOPE_REJECTED", f"Reparse scope component: {ancestor}")


def _project_root(value):
    root = _absolute_path(value)
    _no_reparse(root)
    if not root.is_dir():
        raise ToolError("SCOPE_REJECTED", "project_root must be a directory")
    for relative in ALLOWED_ROOTS:
        allowed = root / relative
        _no_reparse(allowed)
        if not allowed.is_dir():
            raise ToolError("SCOPE_REJECTED", f"Missing allowed directory: {relative}")
    return root


def _relative_path(value):
    if not isinstance(value, str) or not value or "\\" in value or ":" in value:
        raise ToolError("SCOPE_REJECTED", "Invalid relative path")
    parts = value.split("/")
    for part in parts:
        if (part in ("", ".", "..") or part.endswith((".", " "))
                or any(ord(char) < 32 or char in '<>"|?*' for char in part)
                or re.fullmatch(r"(?:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\..*)?", part, re.I)):
            raise ToolError("SCOPE_REJECTED", "Unsafe relative path component")
    if not any(value.startswith(allowed + "/") for allowed in ALLOWED_ROOTS):
        raise ToolError("SCOPE_REJECTED", "Path is outside the two allowed directories")
    return parts


def _scoped_path(root, relative, *, eligible):
    parts = _relative_path(relative)
    if eligible and parts[-1] not in TARGET_FILENAMES:
        raise ToolError("SCOPE_REJECTED", "Filename is outside the exact allowlist")
    path = root.joinpath(*parts)
    _no_reparse(path if eligible else path.parent, missing_ok=True)
    return path


@contextlib.contextmanager
def _open_source(path, *, writable=False):
    # The same handle denies writers/deletion and does not follow a leaf reparse point.
    access = 0xC0000000 if writable else 0x80000000
    handle = _kernel.CreateFileW(str(path), access, 0x1, None, 3, 0x00200000, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        descriptor = msvcrt.open_osfhandle(handle, (os.O_RDWR if writable else os.O_RDONLY) | os.O_BINARY)
    except BaseException:
        _kernel.CloseHandle(handle)
        raise
    try:
        stream = open(path, "r+b" if writable else "rb", buffering=0,
                      opener=lambda name, flags: descriptor)
    except BaseException:
        os.close(descriptor)
        raise
    with stream:
        yield stream


def _file_metadata(stream):
    handle = msvcrt.get_osfhandle(stream.fileno())
    info = _HandleInformation()
    compression = _CompressionInformation()
    if not _kernel.GetFileInformationByHandle(handle, ctypes.byref(info)):
        raise ctypes.WinError(ctypes.get_last_error())
    if not _kernel.GetFileInformationByHandleEx(handle, 8, ctypes.byref(compression), ctypes.sizeof(compression)):
        raise ctypes.WinError(ctypes.get_last_error())
    # Use this same Win32 source in later locked-handle checks; os.stat.st_dev is a different format.
    return {
        "file_id": {"volume_serial": info.volume_serial,
                    "file_index": (info.index_high << 32) | info.index_low},
        "nlink": info.nlink,
        "logical_bytes": (info.size_high << 32) | info.size_low,
        "mtime_ns": (((info.write_time.dwHighDateTime << 32) | info.write_time.dwLowDateTime)
                     - 116_444_736_000_000_000) * 100,
        "attributes": info.attributes,
        "was_compressed": bool(info.attributes & COMPRESSED),
        "allocated_before_bytes": compression.allocated_bytes,
    }


def _final_handle_path(handle):
    length = _kernel.GetFinalPathNameByHandleW(handle, None, 0, 0)
    if not length:
        raise ctypes.WinError(ctypes.get_last_error())
    buffer = ctypes.create_unicode_buffer(length + 1)
    actual = _kernel.GetFinalPathNameByHandleW(handle, buffer, len(buffer), 0)
    if not actual or actual >= len(buffer):
        raise ctypes.WinError(ctypes.get_last_error())
    result = buffer.value
    if result.startswith("\\\\?\\UNC\\"):
        result = "\\\\" + result[8:]
    elif result.startswith("\\\\?\\"):
        result = result[4:]
    return Path(result)


def _final_path(stream):
    return _final_handle_path(msvcrt.get_osfhandle(stream.fileno()))


def _read_candidate(root, relative):
    path = _scoped_path(root, relative, eligible=True)
    with _open_source(path) as stream:
        if os.path.normcase(_final_path(stream)) != os.path.normcase(path):
            raise ToolError("SCOPE_REJECTED", "Opened source has a different final path")
        before = _file_metadata(stream)
        if before["attributes"] & REPARSE:
            return None, "reparse", before["logical_bytes"]
        if before["nlink"] != 1:
            return None, "hardlink", before["logical_bytes"]
        if before["attributes"] & ENCRYPTED:
            return None, "encrypted", before["logical_bytes"]
        if before["was_compressed"]:
            return None, "already_compressed", before["logical_bytes"]
        if not before["file_id"]["file_index"]:
            return None, "unstable", before["logical_bytes"]
        digest = hashlib.sha256()
        length = 0
        while chunk := stream.read(HASH_CHUNK_BYTES):
            digest.update(chunk)
            length += len(chunk)
        after = _file_metadata(stream)
        _scoped_path(root, relative, eligible=True)
        if os.path.normcase(_final_path(stream)) != os.path.normcase(path):
            raise ToolError("SCOPE_REJECTED", "Source final path changed while hashing")
        if before != after or length != before["logical_bytes"]:
            return None, "unstable", before["logical_bytes"]
        return dict(relative_path=relative, **before, sha256=digest.hexdigest()), None, length


_TASK_QUERY = r"""
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$names = @('AI Daily Report', 'AI Daily Report Health')
$rows = @(foreach ($name in $names) {
    $task = Get-ScheduledTask -TaskName $name -TaskPath '\' -ErrorAction Stop
    $info = Get-ScheduledTaskInfo -InputObject $task -ErrorAction Stop
    if ($info.NextRunTime.Year -lt 2000) { throw 'No verifiable next trigger' }
    [pscustomobject]@{
        task_name = $task.TaskName
        state = [int]$task.State
        next_run_time_utc = $info.NextRunTime.ToUniversalTime().ToString('o')
    }
})
ConvertTo-Json -InputObject $rows -Compress
"""


def _utc_time(value):
    if not isinstance(value, str):
        raise ValueError("UTC time must be a string")
    result = datetime.fromisoformat(value)
    if result.tzinfo is None or result.utcoffset() is None:
        raise ValueError("A time-zone offset is required")
    return result.astimezone(timezone.utc)


def read_task_window(now_utc: datetime) -> tuple[bool, datetime]:
    """Read exactly the three existing tasks; never start, register or edit a task."""
    try:
        if now_utc.tzinfo is None or now_utc.utcoffset() != timedelta(0):
            raise ValueError("now_utc must be aware UTC")
        powershell = Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe"
        with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            result = subprocess.run(
                [str(powershell), "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", _TASK_QUERY],
                stdout=stdout, stderr=stderr, check=False, timeout=30,
            )
            if result.returncode != 0:
                raise ValueError("Scheduled-task query failed")
            stdout.seek(0)
            stderr.seek(0)
            if stderr.read().strip():
                raise ValueError("Scheduled-task query produced errors")
            rows = json.loads(stdout.read().decode("utf-8-sig"))
        if not isinstance(rows, list) or len(rows) != len(TASK_NAMES):
            raise ValueError("All current task records are required")
        names = []
        triggers = []
        running = False
        for row in rows:
            if not isinstance(row, dict) or set(row) != {"task_name", "state", "next_run_time_utc"}:
                raise ValueError("Malformed scheduled-task record")
            names.append(row["task_name"])
            if type(row["state"]) is not int or row["state"] not in (2, 3, 4):
                raise ValueError("Unverifiable task state")
            running = running or row["state"] in (2, 4)
            triggers.append(_utc_time(row["next_run_time_utc"]))
        if set(names) != set(TASK_NAMES):
            raise ValueError("Missing or duplicate scheduled task")
        return running, min(triggers)
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as exc:
        raise ToolError("TASK_QUERY_FAILED", "Scheduled-task context cannot be verified", status="paused") from exc


def _check_task_window(*, record=None):
    now = datetime.now(timezone.utc)
    try:
        running, upcoming = read_task_window(now)
        if type(running) is not bool or not isinstance(upcoming, datetime) or upcoming.utcoffset() != timedelta(0):
            raise ValueError("Task window is not UTC")
    except Exception as exc:
        if record:
            record({"checked_at_utc": now.isoformat(), "error_code": "TASK_QUERY_FAILED"})
        raise ToolError("TASK_QUERY_FAILED", "Scheduled-task context cannot be verified", status="paused") from exc
    if record:
        record({"checked_at_utc": now.isoformat(), "task_names": list(TASK_NAMES),
                "running_or_queued": running, "next_trigger_utc": upcoming.isoformat()})
    if running:
        raise ToolError("TASK_RUNNING", "A formal task is running or queued", status="paused")
    _check_cutoff(upcoming)
    return upcoming


def _check_cutoff(upcoming):
    if datetime.now(timezone.utc) >= upcoming - timedelta(seconds=PAUSE_SECONDS):
        raise ToolError("TASK_IMMINENT", "A formal trigger is within 300 seconds", status="paused")


def _walk(root, directory):
    _no_reparse(directory)
    with os.scandir(directory) as iterator:
        entries = sorted(iterator, key=lambda entry: entry.name)
    for entry in entries:
        path = Path(entry.path)
        relative = path.relative_to(root).as_posix()
        try:
            info = os.lstat(path)
        except FileNotFoundError:
            if entry.name in TARGET_FILENAMES:
                yield relative, "unstable", 0
            continue
        if info.st_file_attributes & REPARSE:
            yield relative, "reparse", 0
        elif stat.S_ISDIR(info.st_mode):
            yield from _walk(root, path)
        elif stat.S_ISREG(info.st_mode) and entry.name in TARGET_FILENAMES:
            yield relative, None, info.st_size


def _totals(eligible, skipped):
    return {"eligible_files": len(eligible), "skipped_files": len(skipped),
            "logical_bytes": sum(entry["logical_bytes"] for entry in eligible),
            "allocated_before_bytes": sum(entry["allocated_before_bytes"] for entry in eligible)}


def scan_manifest(project_root: Path) -> dict:
    root = _project_root(project_root)
    _check_task_window()
    eligible, skipped = [], []
    batch_files = batch_bytes = 0
    for allowed in ALLOWED_ROOTS:
        for relative, reason, logical_bytes in _walk(root, root / allowed):
            if reason is None:
                try:
                    entry, reason, logical_bytes = _read_candidate(root, relative)
                except OSError:
                    entry, reason = None, "unstable"
                if entry is not None:
                    eligible.append(entry)
            if reason is not None:
                skipped.append({"relative_path": relative, "reason": reason})
            batch_files += 1
            batch_bytes += logical_bytes
            byte_boundary = batch_bytes >= BATCH_BYTES
            if batch_files % BATCH_FILES == 0 or byte_boundary:
                _check_task_window()
            if byte_boundary:
                batch_bytes = 0
    _check_task_window()
    eligible.sort(key=lambda entry: entry["relative_path"])
    skipped.sort(key=lambda entry: entry["relative_path"])
    return {
        "schema_version": 1, "plan_id": uuid.uuid4().hex,
        "created_at_utc": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        "project_root": str(root), "allowed_roots": list(ALLOWED_ROOTS),
        "eligible": eligible, "skipped": skipped, "totals": _totals(eligible, skipped),
    }


def _changed(message):
    raise ToolError("MANIFEST_CHANGED", message)


def _integer(value, *, minimum=0, maximum=None):
    return type(value) is int and value >= minimum and (maximum is None or value <= maximum)


def _validate_manifest(manifest):
    if not isinstance(manifest, dict) or set(manifest) != MANIFEST_FIELDS:
        _changed("Unexpected manifest fields")
    if type(manifest["schema_version"]) is not int or manifest["schema_version"] != 1:
        _changed("Unsupported manifest version")
    if not isinstance(manifest["plan_id"], str) or not re.fullmatch(r"[0-9a-f]{32}", manifest["plan_id"]):
        _changed("Invalid plan ID")
    try:
        _utc_time(manifest["created_at_utc"])
        if not manifest["created_at_utc"].endswith("Z"):
            raise ValueError("Manifest time must be UTC Z")
    except ValueError:
        _changed("Invalid manifest UTC time")
    if not isinstance(manifest["project_root"], str):
        _changed("Invalid project_root type")
    root = _project_root(manifest["project_root"])
    if str(root) != manifest["project_root"]:
        raise ToolError("SCOPE_REJECTED", "Noncanonical project_root")
    if manifest["allowed_roots"] != list(ALLOWED_ROOTS):
        raise ToolError("SCOPE_REJECTED", "The two allowed directories cannot be changed")
    if not isinstance(manifest["eligible"], list) or not isinstance(manifest["skipped"], list):
        _changed("Invalid manifest entries")
    seen = set()
    for entry in manifest["eligible"]:
        if not isinstance(entry, dict) or set(entry) != ENTRY_FIELDS:
            _changed("Unexpected eligible-entry fields")
        _scoped_path(root, entry["relative_path"], eligible=True)
        key = entry["relative_path"].casefold()
        if key in seen:
            _changed("Duplicate manifest path")
        seen.add(key)
        identity = entry["file_id"]
        if (not isinstance(identity, dict) or set(identity) != {"volume_serial", "file_index"}
                or not _integer(identity["volume_serial"], maximum=0xFFFFFFFF)
                or not _integer(identity["file_index"], minimum=1, maximum=0xFFFFFFFFFFFFFFFF)):
            _changed("Invalid Win32 file identity")
        if type(entry["nlink"]) is not int or entry["nlink"] != 1 or entry["was_compressed"] is not False:
            _changed("Ineligible original file state")
        if (not _integer(entry["logical_bytes"]) or not _integer(entry["allocated_before_bytes"])
                or type(entry["mtime_ns"]) is not int
                or not _integer(entry["attributes"], maximum=0xFFFFFFFF)
                or entry["attributes"] & (REPARSE | COMPRESSED | ENCRYPTED)):
            _changed("Invalid original file metadata")
        if not isinstance(entry["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]):
            _changed("Invalid file SHA-256")
    for entry in manifest["skipped"]:
        if not isinstance(entry, dict) or set(entry) != {"relative_path", "reason"}:
            _changed("Unexpected skipped-entry fields")
        _scoped_path(root, entry["relative_path"], eligible=False)
        key = entry["relative_path"].casefold()
        if key in seen:
            _changed("Duplicate manifest path")
        seen.add(key)
        if not isinstance(entry["reason"], str) or entry["reason"] not in SKIP_REASONS:
            _changed("Unknown skip reason")
    totals = manifest["totals"]
    if (not isinstance(totals, dict) or totals != _totals(manifest["eligible"], manifest["skipped"])
            or any(not _integer(value) for value in totals.values())):
        _changed("Manifest totals are inconsistent")
    return root


def _evidence_directory(root, value):
    evidence = _absolute_path(value)
    base = root / EVIDENCE_ROOT
    if evidence == base or not evidence.is_relative_to(base):
        raise ToolError("SCOPE_REJECTED", "Evidence must be a new directory below the fixed evidence root")
    _no_reparse(evidence, missing_ok=True)
    return evidence


@contextlib.contextmanager
def _lock_directory(path):
    # LIST_DIRECTORY participates in sharing checks; attribute-only handles do not pin against rename.
    handle = _kernel.CreateFileW(str(path), 0x81, 0x1, None, 3, 0x02200000, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        info = _HandleInformation()
        if not _kernel.GetFileInformationByHandle(handle, ctypes.byref(info)):
            raise ctypes.WinError(ctypes.get_last_error())
        if (not info.attributes & 0x10 or info.attributes & REPARSE
                or not ((info.index_high << 32) | info.index_low)
                or os.path.normcase(_final_handle_path(handle)) != os.path.normcase(path)):
            raise ToolError("SCOPE_REJECTED", "Evidence directory identity/path cannot be locked")
        yield handle
    finally:
        _kernel.CloseHandle(handle)


@contextlib.contextmanager
def _locked_evidence_parents(root, evidence):
    # Lock top-down before creating the next component. No parent can be renamed or made a junction.
    base = root / EVIDENCE_ROOT
    with contextlib.ExitStack() as locks:
        for directory in reversed(evidence.parents):
            try:
                locks.enter_context(_lock_directory(directory))
            except FileNotFoundError:
                if not directory.is_relative_to(base):
                    raise ToolError("SCOPE_REJECTED", "An existing evidence ancestor disappeared") from None
                try:
                    directory.mkdir()
                except FileExistsError:
                    pass
                locks.enter_context(_lock_directory(directory))
        yield


def _write_new(path, data):
    with path.open("xb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())


def write_manifest(manifest: dict, evidence_dir: Path) -> Path:
    root = _validate_manifest(manifest)
    evidence = _evidence_directory(root, evidence_dir)
    if evidence.exists():
        raise ToolError("EVIDENCE_EXISTS", "Evidence directory already exists")
    raw = (json.dumps(manifest, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    with _locked_evidence_parents(root, evidence):
        _check_task_window()
        try:
            evidence.mkdir()
        except FileExistsError:
            raise ToolError("EVIDENCE_EXISTS", "Evidence directory already exists") from None
        with _lock_directory(evidence):
            # The entire checked directory chain stays locked across both writes and the task query.
            _write_new(evidence / "manifest.sha256", (hashlib.sha256(raw).hexdigest() + "\n").encode("ascii"))
            _check_task_window()
            _write_new(evidence / "manifest.json", raw)
    return evidence / "manifest.json"


def _unique_object(pairs):
    result = {}
    for name, value in pairs:
        if name in result:
            _changed("Duplicate JSON object field")
        result[name] = value
    return result


def load_manifest(manifest_path: Path) -> dict:
    path = _absolute_path(manifest_path)
    if path.name != "manifest.json":
        raise ToolError("SCOPE_REJECTED", "The frozen manifest must be named manifest.json")
    _no_reparse(path)
    _no_reparse(path.with_suffix(".sha256"))
    try:
        raw = path.read_bytes()
        expected = path.with_suffix(".sha256").read_bytes()
        if not re.fullmatch(rb"[0-9a-f]{64}\n", expected) or hashlib.sha256(raw).hexdigest().encode("ascii") + b"\n" != expected:
            _changed("Manifest bytes no longer match manifest.sha256")
        manifest = json.loads(raw.decode("utf-8"), object_pairs_hook=_unique_object)
    except (OSError, ValueError, UnicodeError) as exc:
        raise ToolError("MANIFEST_CHANGED", "Frozen manifest cannot be read or decoded") from exc
    root = _validate_manifest(manifest)
    _evidence_directory(root, path.parent)
    return manifest


def _summary(status, *, manifest=None, path=None, error=None):
    totals = manifest["totals"] if manifest else {}
    return {
        "status": status, "plan_sha256": hashlib.sha256(path.read_bytes()).hexdigest() if path else None,
        "run_id": manifest["plan_id"] if manifest else None,
        "files_verified": totals.get("eligible_files", 0), "files_changed": 0, "files_already": 0,
        "logical_bytes": totals.get("logical_bytes", 0),
        "allocated_before_bytes": totals.get("allocated_before_bytes", 0),
        "allocated_after_bytes": totals.get("allocated_before_bytes", 0), "reclaimed_bytes": 0,
        "pause_reason": error.code if error and status == "paused" else None,
        "error_code": error.code if error else None,
        "message": str(error) if error else None, "manifest": str(path) if path else None,
    }


def _match_source(stream, path, entry):
    metadata = _file_metadata(stream)
    if (os.path.normcase(_final_path(stream)) != os.path.normcase(path)
            or any(metadata[key] != entry[key] for key in ("file_id", "nlink", "logical_bytes", "mtime_ns"))
            or (metadata["attributes"] & ~COMPRESSED) != (entry["attributes"] & ~COMPRESSED)):
        raise ToolError("SOURCE_CHANGED", f"Source identity or metadata changed: {entry['relative_path']}")
    return metadata


def _hash_source(stream):
    stream.seek(0)
    digest = hashlib.sha256()
    length = 0
    while chunk := stream.read(HASH_CHUNK_BYTES):
        digest.update(chunk)
        length += len(chunk)
    return digest.hexdigest(), length


def _read_blocks(stream, length):
    blocks = []
    for offset in (0, length // 3, max(0, length - 4096)):
        stream.seek(offset)
        blocks.append(stream.read(min(4096, length - offset)))
    return blocks


def _restore_mtime(stream, mtime_ns):
    ticks = mtime_ns // 100 + 116_444_736_000_000_000
    stamp = wintypes.FILETIME(ticks & 0xFFFFFFFF, ticks >> 32)
    if not _kernel.SetFileTime(msvcrt.get_osfhandle(stream.fileno()), None, None, ctypes.byref(stamp)):
        raise ctypes.WinError(ctypes.get_last_error())


def _allocation_unit(root):
    sectors, sector_bytes, free, total = (wintypes.DWORD() for _ in range(4))
    if not _kernel.GetDiskFreeSpaceW(root.anchor, ctypes.byref(sectors), ctypes.byref(sector_bytes),
                                     ctypes.byref(free), ctypes.byref(total)):
        raise ctypes.WinError(ctypes.get_last_error())
    unit = sectors.value * sector_bytes.value
    if not unit:
        raise ToolError("IO_FAILED", "Cannot determine the volume allocation unit")
    return unit


def _decompression_growth(entry, current, unit):
    if not current["was_compressed"]:
        return 0
    # Round up to real NTFS clusters; retain original preallocation if it was larger.
    uncompressed = max(entry["allocated_before_bytes"], ((entry["logical_bytes"] + unit - 1) // unit) * unit)
    return max(0, uncompressed - current["allocated_before_bytes"])


def _check_rollback_space(root, growth):
    if shutil.disk_usage(root).free < growth + (1 << 30):
        raise ToolError("INSUFFICIENT_SPACE", "Rollback requires estimated growth plus 1 GiB free", status="paused")


def _process_source(root, entry, result, *, compression, upcoming, allocation_unit=None, receipt=None):
    path = _scoped_path(root, entry["relative_path"], eligible=True)
    try:
        with contextlib.ExitStack() as locks:
            for directory in reversed(path.parents):
                locks.enter_context(_lock_directory(directory))
            with _open_source(path, writable=compression is not None) as stream:
                before = _match_source(stream, path, entry)
                if _hash_source(stream) != (entry["sha256"], entry["logical_bytes"]):
                    raise ToolError("SOURCE_CHANGED", f"Source content changed: {entry['relative_path']}")
                blocks = _read_blocks(stream, entry["logical_bytes"])
                if _match_source(stream, path, entry) != before:
                    raise ToolError("SOURCE_CHANGED", "Source changed during preflight reads")
                result["allocated_before_bytes"] += before["allocated_before_bytes"]
                result["allocated_after_bytes"] += before["allocated_before_bytes"]
                after = before
                if compression is not None and before["was_compressed"] != compression:
                    if compression is False:
                        _check_rollback_space(root, _decompression_growth(entry, before, allocation_unit))
                    _check_cutoff(upcoming)
                    set_compression(stream, compression)
                    result["files_changed"] += 1
                    if _file_metadata(stream)["mtime_ns"] != entry["mtime_ns"]:
                        _restore_mtime(stream, entry["mtime_ns"])
                    if (_hash_source(stream) != (entry["sha256"], entry["logical_bytes"])
                            or _read_blocks(stream, entry["logical_bytes"]) != blocks):
                        raise ToolError("HASH_MISMATCH", f"Post-compression-state reads changed: {entry['relative_path']}")
                    after = _match_source(stream, path, entry)
                    if after["was_compressed"] != compression:
                        raise ToolError("IO_FAILED", "Native operation did not set the requested compression state")
                elif compression is not None:
                    result["files_already"] += 1
                result["allocated_after_bytes"] += after["allocated_before_bytes"] - before["allocated_before_bytes"]
                result["files_verified"] += 1
                result["logical_bytes"] += entry["logical_bytes"]
                result["files_compressed" if after["was_compressed"] else "files_uncompressed"] += 1
                if receipt is not None:
                    receipt()
                return after
    except OSError as exc:
        if getattr(exc, "winerror", None) in (32, 33):
            raise ToolError("SOURCE_LOCKED", f"Source is in use: {entry['relative_path']}") from exc
        if isinstance(exc, FileNotFoundError):
            raise ToolError("SOURCE_CHANGED", f"Source disappeared: {entry['relative_path']}") from exc
        raise


def _append_json(stream, record):
    stream.write((json.dumps(record, ensure_ascii=False) + "\n").encode("utf-8"))
    stream.flush()
    os.fsync(stream.fileno())


def _run_error(result, exc):
    error = exc if isinstance(exc, ToolError) else ToolError("IO_FAILED", str(exc))
    result.update(status=error.status, error_code=error.code, message=str(error),
                  pause_reason=error.code if error.status == "paused" else None)


def _run_files(root, manifest, result, events, windows, *, command):
    def event(name, **fields):
        _append_json(events, {"event": name, "at_utc": datetime.now(timezone.utc).isoformat(), **fields})

    def refresh():
        return _check_task_window(record=lambda row: _append_json(windows, row))

    batch_bytes = 0
    trial_done = False
    compression = {"apply": True, "verify": None, "rollback": False}[command]
    event("run_started", command=command, plan_sha256=result["plan_sha256"])
    try:
        upcoming = refresh()
        unit = None
        if command == "rollback":
            unit = _allocation_unit(root)
            growth = preflight_bytes = compressed_files = 0
            for index, entry in enumerate(manifest["eligible"], 1):
                _check_cutoff(upcoming)
                current = _process_source(root, entry, result.copy(), compression=None, upcoming=upcoming)
                growth += _decompression_growth(entry, current, unit)
                compressed_files += current["was_compressed"]
                preflight_bytes += entry["logical_bytes"]
                if preflight_bytes >= BATCH_BYTES or index % BATCH_FILES == 0:
                    upcoming = refresh()
                if preflight_bytes >= BATCH_BYTES:
                    preflight_bytes = 0
            event("rollback_preflight_completed", estimated_growth_bytes=growth, files_to_restore=compressed_files)
            if compressed_files:
                _check_rollback_space(root, growth)
            upcoming = refresh()
        for index, entry in enumerate(manifest["eligible"], 1):
            _check_cutoff(upcoming)
            changed_before = result["files_changed"]
            _process_source(
                root, entry, result, compression=compression, upcoming=upcoming, allocation_unit=unit,
                receipt=lambda: event("file_verified", relative_path=entry["relative_path"], sha256=entry["sha256"],
                                      changed=result["files_changed"] != changed_before),
            )
            batch_bytes += entry["logical_bytes"]
            byte_boundary = batch_bytes >= BATCH_BYTES
            if byte_boundary and not trial_done:
                event("trial_completed", **{key: result[key] for key in ("files_verified", "files_changed",
                      "files_already", "logical_bytes", "allocated_before_bytes", "allocated_after_bytes")})
                trial_done = True
            if byte_boundary or index % BATCH_FILES == 0:
                event("batch_completed", files_verified=result["files_verified"], logical_bytes=result["logical_bytes"])
                upcoming = refresh()
            if byte_boundary:
                batch_bytes = 0
        if not trial_done:
            event("trial_completed", **{key: result[key] for key in ("files_verified", "files_changed",
                  "files_already", "logical_bytes", "allocated_before_bytes", "allocated_after_bytes")})
    except (ToolError, OSError) as exc:
        _run_error(result, exc)
    result["reclaimed_bytes"] = result["allocated_before_bytes"] - result["allocated_after_bytes"]
    event("run_finished", status=result["status"], error_code=result["error_code"])


@contextlib.contextmanager
def _compression_run(root):
    # The named object's lifetime is the guard; no wait, ownership transfer or stale lock file.
    identity = hashlib.sha256(os.path.normcase(str(root)).encode("utf-8")).hexdigest()
    handle = _kernel.CreateMutexW(None, False, "Global\\HistoricalArtifactCompression-" + identity)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        if ctypes.get_last_error() == 183:  # ERROR_ALREADY_EXISTS
            raise ToolError("SOURCE_LOCKED", "A compression run is already active for this project_root")
        yield
    finally:
        _kernel.CloseHandle(handle)


def _run_manifest(manifest_path, *, command):
    result = _summary("completed")
    result.update(files_compressed=0, files_uncompressed=0)
    try:
        path = _absolute_path(manifest_path)
        manifest = load_manifest(path)
        root = Path(manifest["project_root"])
        result.update(run_id=uuid.uuid4().hex, manifest=str(path))
        run_dir = _evidence_directory(root, path.parent / "runs" / result["run_id"])
        guard = _compression_run(root) if command != "verify" else contextlib.nullcontext()
        with guard, _locked_evidence_parents(root, run_dir):
            # Pin the frozen files as well as every ancestor before rereading their exact bytes.
            with _open_source(path) as frozen, _open_source(path.with_suffix(".sha256")):
                manifest = load_manifest(path)
                result["plan_sha256"] = _hash_source(frozen)[0]
                run_dir.mkdir()
                with _lock_directory(run_dir), (run_dir / "events.jsonl").open("xb") as events, \
                     (run_dir / "task-windows.jsonl").open("xb") as windows:
                    _run_files(root, manifest, result, events, windows, command=command)
                    _write_new(run_dir / "summary.json", (json.dumps(result, ensure_ascii=False, indent=2) + "\n").encode("utf-8"))
    except (ToolError, OSError) as exc:
        _run_error(result, exc)
    result["reclaimed_bytes"] = result["allocated_before_bytes"] - result["allocated_after_bytes"]
    return result


def apply_manifest(manifest_path: Path) -> dict:
    return _run_manifest(manifest_path, command="apply")


def verify_manifest(manifest_path: Path) -> dict:
    return _run_manifest(manifest_path, command="verify")


def rollback_manifest(manifest_path: Path) -> dict:
    return _run_manifest(manifest_path, command="rollback")


class _ArgumentParser(argparse.ArgumentParser):
    def error(self, message):
        raise ToolError("INVALID_ARGUMENT", message)


def main(argv: list[str] | None = None) -> int:
    parser = _ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("plan", help="Create a frozen read-only manifest of the exact allowlist")
    plan.add_argument("--project-root", required=True, type=Path)
    plan.add_argument("--evidence-dir", required=True, type=Path)
    for command in ("apply", "verify", "rollback"):
        subcommand = commands.add_parser(command, help=f"{command.capitalize()} one frozen manifest")
        subcommand.add_argument("--manifest", required=True, type=Path)
    try:
        args = parser.parse_args(argv)
        if args.command == "plan":
            # Reject an invalid output path before any body reads or scheduler query.
            root = _project_root(args.project_root)
            _evidence_directory(root, args.evidence_dir)
            manifest = scan_manifest(root)
            path = write_manifest(manifest, args.evidence_dir)
            result = _summary("completed", manifest=manifest, path=path)
        else:
            operation = {"apply": apply_manifest, "verify": verify_manifest, "rollback": rollback_manifest}[args.command]
            result = operation(args.manifest)
        exitcode = {"completed": 0, "paused": 1, "failed": 2}[result["status"]]
    except ToolError as exc:
        result = _summary(exc.status, error=exc)
        exitcode = 1 if exc.status == "paused" else 2
    except OSError as exc:
        result = _summary("failed", error=ToolError("IO_FAILED", str(exc)))
        exitcode = 2
    print(json.dumps(result, ensure_ascii=False))
    return exitcode


if __name__ == "__main__":
    raise SystemExit(main())
