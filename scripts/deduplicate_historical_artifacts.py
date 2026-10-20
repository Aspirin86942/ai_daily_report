"""Deduplicate only frozen historical inputs; retain bytes, audit and rollback."""

import argparse
import collections
import contextlib
import ctypes
from ctypes import wintypes
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import msvcrt
import os
from pathlib import Path
import shutil
import subprocess
import uuid


_spec = importlib.util.spec_from_file_location(
    "_historical_file_guards", Path(__file__).with_name("compress_historical_artifacts.py")
)
files = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(files)
ALLOWED_ROOTS = tuple(
    f".artifacts/rust-migration/archive-benchmark-final-20261017{suffix}/incremental/{part}"
    for suffix in ("", "-04")
    for part in ("work", "data/archive/objects")
)
EVIDENCE_ROOT = ".artifacts/architecture-storage-20261020/dedup"
FROZEN_MARKER = ".frozen-evidence.json"
READONLY = 1
K = files._kernel
K.SetFileAttributesW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD]
K.SetFileAttributesW.restype = wintypes.BOOL
K.SetFileInformationByHandle.argtypes = [
    wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD,
]
K.SetFileInformationByHandle.restype = wintypes.BOOL
K.SetFileTime.argtypes = [
    wintypes.HANDLE, ctypes.POINTER(wintypes.FILETIME),
    ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME),
]
K.SetFileTime.restype = wintypes.BOOL
A = ctypes.WinDLL("advapi32", use_last_error=True)
A.GetFileSecurityW.argtypes = [
    wintypes.LPCWSTR, wintypes.DWORD, wintypes.LPVOID, wintypes.DWORD,
    ctypes.POINTER(wintypes.DWORD),
]
A.GetFileSecurityW.restype = wintypes.BOOL


class StreamInfo(ctypes.Structure):
    _fields_ = [("size", ctypes.c_longlong), ("name", wintypes.WCHAR * 296)]


K.FindFirstStreamW.argtypes = [wintypes.LPCWSTR, ctypes.c_int, ctypes.POINTER(StreamInfo), wintypes.DWORD]
K.FindFirstStreamW.restype = wintypes.HANDLE
K.FindNextStreamW.argtypes = [wintypes.HANDLE, ctypes.POINTER(StreamInfo)]
K.FindNextStreamW.restype = wintypes.BOOL
K.FindClose.argtypes = [wintypes.HANDLE]


def check(ok):
    if not ok:
        raise ctypes.WinError(ctypes.get_last_error())


def attrs(path, value):
    check(K.SetFileAttributesW(str(path), value))


def replace(source, destination):
    # POSIX replacement keeps our read guard on the old file valid until verification.
    name = str(destination).encode("utf-16-le")
    class RenameInfo(ctypes.Structure):
        _fields_ = [
            ("flags", wintypes.DWORD), ("root", wintypes.HANDLE),
            ("length", wintypes.DWORD), ("name", ctypes.c_byte * len(name)),
        ]
    info = RenameInfo(3, None, len(name))
    ctypes.memmove(ctypes.addressof(info) + RenameInfo.name.offset, name, len(name))
    handle = K.CreateFileW(str(source), 0x10000, 7, None, 3, 0x00200000, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        check(K.SetFileInformationByHandle(handle, 22, ctypes.byref(info), ctypes.sizeof(info)))
    finally:
        K.CloseHandle(handle)


def restore_creation(path, ticks):
    handle = K.CreateFileW(str(path), 0x100, 7, None, 3, 0x00200000, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        value = wintypes.FILETIME(ticks & 0xFFFFFFFF, ticks >> 32)
        check(K.SetFileTime(handle, ctypes.byref(value), None, None))
    finally:
        K.CloseHandle(handle)


def security(path):
    needed = wintypes.DWORD()
    A.GetFileSecurityW(str(path), 7, None, 0, ctypes.byref(needed))
    if not needed.value:
        raise ctypes.WinError(ctypes.get_last_error())
    data = ctypes.create_string_buffer(needed.value)
    check(A.GetFileSecurityW(str(path), 7, data, len(data), ctypes.byref(needed)))
    return hashlib.sha256(data.raw[:needed.value]).hexdigest()


def streams(path):
    info = StreamInfo()
    handle = K.FindFirstStreamW(str(path), 0, ctypes.byref(info), 0)
    if handle == ctypes.c_void_p(-1).value:
        if ctypes.get_last_error() == 38:
            return []
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        names = [info.name]
        while K.FindNextStreamW(handle, ctypes.byref(info)):
            names.append(info.name)
        if ctypes.get_last_error() != 38:
            raise ctypes.WinError(ctypes.get_last_error())
        return names
    finally:
        K.FindClose(handle)


def source_path(root, relative):
    if (not isinstance(relative, str) or "\\" in relative or ":" in relative
            or any(p in ("", ".", "..") or p.endswith((".", " ")) for p in relative.split("/"))
            or not any(relative.startswith(p + "/") for p in ALLOWED_ROOTS)):
        raise ValueError("Path outside the frozen allowlist")
    path = root.joinpath(*relative.split("/"))
    files._no_reparse(path)
    return path


@contextlib.contextmanager
def opened(path):
    handle = K.CreateFileW(str(path), 0x80000000, 5, None, 3, 0x00200000, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    descriptor = msvcrt.open_osfhandle(handle, os.O_RDONLY | os.O_BINARY)
    with os.fdopen(descriptor, "rb") as stream:
        if os.path.normcase(files._final_path(stream)) != os.path.normcase(path):
            raise ValueError("Opened path changed")
        yield stream


def metadata(stream):
    value = files._file_metadata(stream)
    info = files._HandleInformation()
    check(K.GetFileInformationByHandle(msvcrt.get_osfhandle(stream.fileno()), ctypes.byref(info)))
    value["creation_ticks"] = (info.creation_time.dwHighDateTime << 32) | info.creation_time.dwLowDateTime
    return value


def digest(stream):
    stream.seek(0)
    return hashlib.file_digest(stream, "sha256").hexdigest()


def inspect(path):
    files._no_reparse(path)
    with opened(path) as stream:
        before = metadata(stream)
        value = dict(before, sha256=digest(stream), security=security(path), streams=streams(path))
        if before != metadata(stream):
            raise ValueError("File changed while hashing")
    return value


def key(entry):
    return (
        entry["sha256"], entry["logical_bytes"], entry["mtime_ns"],
        entry["attributes"], entry["security"], entry["file_id"]["volume_serial"],
    )


def write_new(path, value):
    with path.open("xb") as stream:
        raw = (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
        stream.write(raw)
        stream.flush()
        os.fsync(stream.fileno())
    return hashlib.sha256(raw).hexdigest()


def event(directory, **value):
    with (directory / "events.jsonl").open("a+b") as stream:
        if stream.tell():
            stream.seek(-1, os.SEEK_END)
            if stream.read(1) != b"\n":
                stream.write(b"\n")
        stream.write((json.dumps(value, ensure_ascii=False) + "\n").encode("utf-8"))
        stream.flush()
        os.fsync(stream.fileno())


def quiet_window():
    command = (
        "@(Get-ScheduledTask | Where-Object {$_.TaskName -like 'AI Daily Report*'} | "
        "ForEach-Object {$i=$_ | Get-ScheduledTaskInfo; [pscustomobject]@{"
        "name=$_.TaskName;state=[int]$_.State;seconds=($i.NextRunTime-(Get-Date)).TotalSeconds}})"
        " | ConvertTo-Json -Compress"
    )
    result = subprocess.run(
        ["powershell.exe", "-NoProfile", "-Command", command],
        capture_output=True, check=True, creationflags=0x08000000,
    )
    tasks = json.loads(result.stdout)
    expected = {"AI Daily Report", "AI Daily Report Collect", "AI Daily Report Health", "AI Daily Report Parse"}
    if {t["name"] for t in tasks} != expected or any(t["state"] != 3 or t["seconds"] < 300 for t in tasks):
        raise RuntimeError("Formal task active, unavailable or due within five minutes")


def plan(project_root, output):
    root = files._absolute_path(project_root)
    files._no_reparse(root)
    output = files._absolute_path(output)
    if not output.is_relative_to(root / EVIDENCE_ROOT) or output == root / EVIDENCE_ROOT:
        raise ValueError("Evidence must be below the fixed evidence root")
    files._no_reparse(output, missing_ok=True)
    if output.exists():
        raise FileExistsError(output)
    quiet_window()
    groups = collections.defaultdict(list)
    skipped = []
    for relative in ALLOWED_ROOTS:
        directory = root / relative
        files._no_reparse(directory)
        if not directory.is_dir():
            raise ValueError("Required historical root is missing")
        for base, dirs, names in os.walk(directory, followlinks=False):
            for name in dirs:
                files._no_reparse(Path(base) / name)
            for name in sorted(names):
                path = Path(base) / name
                relative_path = path.relative_to(root).as_posix()
                value = inspect(path)
                reason = None
                if ".dedup-" in name:
                    raise ValueError("Unfinished deduplication requires its original manifest")
                if value["nlink"] != 1:
                    reason = "existing_hardlink"
                elif value["attributes"] & ~(0x20 | 0x80 | READONLY):
                    reason = "special_attributes"
                elif value["streams"] not in ([], ["::$DATA"]):
                    reason = "additional_streams"
                elif value["logical_bytes"] == 0:
                    reason = "empty"
                if reason:
                    skipped.append({"path": relative_path, "reason": reason})
                else:
                    groups[key(value)].append(dict(path=relative_path, **value))
    selected = [sorted(v, key=lambda e: e["path"]) for v in groups.values() if len(v) > 1]
    selected.sort(key=lambda group: group[0]["path"])
    value = {
        "schema_version": 1, "plan_id": uuid.uuid4().hex,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "project_root": str(root), "allowed_roots": list(ALLOWED_ROOTS),
        "groups": selected, "skipped": skipped,
        "unmatched_files": sum(len(v) for v in groups.values() if len(v) == 1),
        "reclaim_file_data_bytes": sum(e["allocated_before_bytes"] for g in selected for e in g[1:]),
        "rollback_growth_bytes": sum(e["allocated_before_bytes"] for g in selected for e in g[1:]),
    }
    quiet_window()
    output.mkdir(parents=True)
    sha = write_new(output / "manifest.json", value)
    (output / "manifest.sha256").write_text(sha + "\n", encoding="ascii")
    return {"manifest": str(output / "manifest.json"), "sha256": sha,
            "groups": len(selected), "files": sum(map(len, selected)),
            "reclaim_file_data_bytes": value["reclaim_file_data_bytes"]}


def load(path, expected):
    path = files._absolute_path(path)
    files._no_reparse(path)
    raw = path.read_bytes()
    if len(expected) != 64 or hashlib.sha256(raw).hexdigest() != expected:
        raise ValueError("Manifest hash mismatch")
    value = json.loads(raw, object_pairs_hook=files._unique_object)
    root = files._absolute_path(value["project_root"])
    if (value["schema_version"] != 1 or value["allowed_roots"] != list(ALLOWED_ROOTS)
            or not path.is_relative_to(root / EVIDENCE_ROOT)
            or not isinstance(value["plan_id"], str)
            or len(value["plan_id"]) != 32
            or any(c not in "0123456789abcdef" for c in value["plan_id"])):
        raise ValueError("Manifest scope mismatch")
    seen = set()
    for group in value["groups"]:
        if len(group) < 2:
            raise ValueError("Group needs distinct copies")
        for entry in group:
            source_path(root, entry["path"])
            if entry["path"].casefold() in seen or key(entry) != key(group[0]) or entry["nlink"] != 1:
                raise ValueError("Duplicate path or incompatible group")
            if entry["attributes"] & ~(0x20 | 0x80 | READONLY) or entry["streams"] not in ([], ["::$DATA"]):
                raise ValueError("Ineligible metadata")
            seen.add(entry["path"].casefold())
    return value, root


def same(stream, entry, path, *, identity=True):
    actual = metadata(stream)
    for field in ("logical_bytes", "mtime_ns"):
        if actual[field] != entry[field]:
            raise ValueError(f"Changed {field}: {entry['path']}")
    if actual["attributes"] & ~READONLY != entry["attributes"] & ~READONLY:
        raise ValueError("File attributes changed")
    if identity and actual["file_id"] != entry["file_id"]:
        raise ValueError("File identity changed")
    if digest(stream) != entry["sha256"] or security(path) != entry["security"] or streams(path) != entry["streams"]:
        raise ValueError("File content, streams or security changed")
    return actual


def freeze(root, manifest, directory):
    for prefix in sorted({p.split("/incremental/")[0] for p in ALLOWED_ROOTS}):
        marker = root / prefix / FROZEN_MARKER
        value = {"plan_id": manifest["plan_id"], "manifest": str(directory / "manifest.json"),
                 "reason": "Immutable historical input; materialize copies in a new benchmark directory"}
        if marker.exists():
            if json.loads(marker.read_text(encoding="utf-8")) != value:
                raise ValueError("A different frozen plan owns this directory")
        else:
            write_new(marker, value)


def temp_paths(path, plan_id):
    return tuple(path.with_name(path.name + ".dedup-" + plan_id + suffix) for suffix in (".new", ".old"))


@contextlib.contextmanager
def operation_lock(directory):
    path = directory / "operation.lock"
    files._no_reparse(path, missing_ok=True)
    handle = K.CreateFileW(str(path), 0xC0000000, 0, None, 4, 0x00200000, None)
    if handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        yield
    finally:
        K.CloseHandle(handle)


def check_link_scope(root, group, plan_id):
    source = source_path(root, group[0]["path"])
    source_id = source.stat().st_ino
    expected = 0
    for entry in group:
        path = source_path(root, entry["path"])
        temporary, _ = temp_paths(path, plan_id)
        expected += path.stat().st_ino == source_id
        expected += temporary.exists() and temporary.stat().st_ino == source_id
    if source.stat().st_nlink != expected:
        raise ValueError("New external canonical hardlink")


@contextlib.contextmanager
def parents_locked(paths):
    with contextlib.ExitStack() as stack:
        parents = sorted({p for path in paths for p in path.parents}, key=lambda p: len(p.parts))
        for parent in parents:
            # Allow child creation, but keep each directory itself pinned against rename.
            handle = K.CreateFileW(str(parent), 0x81, 3, None, 3, 0x02200000, None)
            if handle == ctypes.c_void_p(-1).value:
                raise ctypes.WinError(ctypes.get_last_error())
            stack.callback(K.CloseHandle, handle)
            if os.path.normcase(files._final_handle_path(handle)) != os.path.normcase(parent):
                raise ValueError("Directory identity changed")
            files._no_reparse(parent)
        yield


def apply_one(source, target, original, canonical, manifest, directory, restored_ids):
    temporary, backup = temp_paths(target, manifest["plan_id"])
    with parents_locked([source, target]), opened(source) as source_stream:
        source_meta = same(source_stream, canonical, source)
        with opened(target) as target_stream:
            target_meta = same(target_stream, original, target, identity=False)
            if target_meta["file_id"] == source_meta["file_id"]:
                if backup.exists():
                    with opened(backup) as saved:
                        saved_meta = same(saved, original, backup, identity=False)
                        if saved_meta["file_id"] not in (original["file_id"], restored_ids.get(original["path"])):
                            raise ValueError("Unexpected rollback identity")
                    if backup.stat().st_file_attributes & READONLY:
                        attrs(backup, original["attributes"] & ~READONLY)
                    backup.unlink()
                return False
            if target_meta["file_id"] not in (original["file_id"], restored_ids.get(original["path"])):
                raise ValueError("Unexpected independent target identity")
            if target_meta["nlink"] != 1 + int(backup.exists()):
                raise ValueError("New external target hardlink")
            if backup.exists():
                if backup.stat().st_ino != target.stat().st_ino:
                    raise ValueError("Unexpected rollback link")
            else:
                os.link(target, backup)
            if temporary.exists():
                if temporary.stat().st_ino != source.stat().st_ino:
                    raise ValueError("Unexpected prepared link")
            else:
                os.link(source, temporary)
            event(directory, action="prepared", path=original["path"], source=canonical["path"])
            original_readonly = target_meta["attributes"] & READONLY
            if original_readonly:
                attrs(target, original["attributes"] & ~READONLY)
            try:
                replace(temporary, target)
            except BaseException:
                if original_readonly:
                    attrs(target, original["attributes"])
                raise
        with opened(target) as current:
            actual = same(current, original, target, identity=False)
            if actual["file_id"] != source_meta["file_id"]:
                raise ValueError("Replacement did not share the canonical identity")
        event(directory, action="verified", path=original["path"], file_id=source_meta["file_id"])
        if backup.stat().st_file_attributes & READONLY:
            attrs(backup, original["attributes"] & ~READONLY)
        backup.unlink()
        return True


def restored_identities(directory):
    result = {}
    journal = directory / "events.jsonl"
    if journal.exists():
        for raw in journal.read_bytes().splitlines():
            try:
                entry = json.loads(raw)
            except ValueError:
                continue  # A torn journal line never authorizes a new identity.
            if entry.get("action") == "rollback_prepared":
                result[entry["path"]] = entry["file_id"]
    return result


def rollback_one(source, target, entry, canonical, manifest, directory):
    temporary, backup = temp_paths(target, manifest["plan_id"])
    restored_path = temporary.with_suffix(".restore")
    with parents_locked([source, target]), opened(source) as stream:
        source_meta = same(stream, canonical, source)
        with opened(target) as current:
            target_meta = same(current, entry, target, identity=False)
            if target_meta["file_id"] != source_meta["file_id"]:
                restore_creation(target, entry["creation_ticks"])
                attrs(target, entry["attributes"])
                return False
            if temporary.exists() or backup.exists():
                raise ValueError("Pending apply requires apply recovery first")
            if restored_path.exists():
                files._no_reparse(restored_path)
                if restored_path.stat().st_nlink != 1:
                    raise ValueError("Unexpected restore hardlink")
                # A failed copy is disposable only while the complete shared target remains.
                attrs(restored_path, restored_path.stat().st_file_attributes & ~READONLY)
                restored_path.unlink()
            with restored_path.open("xb") as output:
                stream.seek(0)
                shutil.copyfileobj(stream, output, 1024 * 1024)
                output.flush()
                os.fsync(output.fileno())
            os.utime(restored_path, ns=(target.stat().st_atime_ns, entry["mtime_ns"]))
            restore_creation(restored_path, entry["creation_ticks"])
            with opened(restored_path) as restored:
                value = same(restored, entry, restored_path, identity=False)
            event(directory, action="rollback_prepared", path=entry["path"], file_id=value["file_id"])
            attrs(source, canonical["attributes"] & ~READONLY)
            try:
                replace(restored_path, target)
            finally:
                attrs(source, canonical["attributes"] | READONLY)
            # NTFS tunneling may restore the replaced path's creation time during rename.
            restore_creation(target, entry["creation_ticks"])
            attrs(target, entry["attributes"])
        event(directory, action="restored", path=entry["path"])
    return True


def operate(path, expected, operation):
    path = files._absolute_path(path)
    files._no_reparse(path)
    load(path, expected)
    with operation_lock(path.parent):
        return operate_locked(path, expected, operation)


def operate_locked(path, expected, operation):
    if operation not in ("apply", "rollback", "verify"):
        raise ValueError("Unknown operation")
    manifest, root = load(path, expected)
    directory = Path(path).parent
    quiet_window()
    if operation == "rollback" and shutil.disk_usage(root).free < manifest["rollback_growth_bytes"] + (1 << 30):
        raise RuntimeError("Rollback requires estimated growth plus 1 GiB")
    restored_ids = restored_identities(directory)
    for group in manifest["groups"]:
        check_link_scope(root, group, manifest["plan_id"])
        canonical_id = group[0]["file_id"]
        for entry in group:
            current_path = source_path(root, entry["path"])
            with opened(current_path) as stream:
                actual = same(stream, entry, current_path, identity=False)
                if actual["file_id"] not in (entry["file_id"], canonical_id, restored_ids.get(entry["path"])):
                    raise ValueError("File identity changed since the plan")
                _, backup = temp_paths(current_path, manifest["plan_id"])
                if actual["file_id"] != canonical_id and actual["nlink"] != 1 + int(backup.exists()):
                    raise ValueError("New external target hardlink")
    if operation == "apply":
        freeze(root, manifest, directory)
    changed = 0
    for number, group in enumerate(manifest["groups"]):
        if number % 25 == 0:
            quiet_window()
        source = source_path(root, group[0]["path"])
        if operation == "apply":
            for entry in group[1:]:
                target = source_path(root, entry["path"])
                changed += apply_one(source, target, entry, group[0], manifest, directory, restored_ids)
            restore_creation(source, group[0]["creation_ticks"])
            attrs(source, group[0]["attributes"] | READONLY)
        elif operation == "rollback":
            for entry in group[1:]:
                target = source_path(root, entry["path"])
                changed += rollback_one(source, target, entry, group[0], manifest, directory)
            restore_creation(source, group[0]["creation_ticks"])
            attrs(source, group[0]["attributes"])
        else:
            with opened(source) as stream:
                canonical = same(stream, group[0], source)
            for entry in group[1:]:
                with opened(source_path(root, entry["path"])) as stream:
                    actual = same(stream, entry, source_path(root, entry["path"]), identity=False)
                if actual["file_id"] != canonical["file_id"]:
                    raise ValueError("Group is not fully deduplicated")
                if not actual["attributes"] & READONLY:
                    raise ValueError("Shared content must be read-only")
                if actual["nlink"] != len(group):
                    raise ValueError("Unexpected external hardlink")
    result = {"operation": operation, "changed": changed, "groups": len(manifest["groups"]),
              "plan_sha256": expected, "reclaim_file_data_bytes": manifest["reclaim_file_data_bytes"]}
    event(directory, action="completed", **result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    create = commands.add_parser("plan")
    create.add_argument("--project-root", type=Path, required=True)
    create.add_argument("--output", type=Path, required=True)
    for name in ("apply", "verify", "rollback"):
        command = commands.add_parser(name)
        command.add_argument("--manifest", type=Path, required=True)
        command.add_argument("--sha256", required=True)
    args = parser.parse_args()
    if args.command == "plan":
        result = plan(args.project_root, args.output)
    else:
        result = operate(args.manifest, args.sha256, args.command)
    print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    main()
