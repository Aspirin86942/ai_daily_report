"""Immutable, bounded local snapshots used independently of the scanner cache."""

from __future__ import annotations

from dataclasses import dataclass
from hashlib import sha256
import json
import os
from pathlib import Path
import re
import stat
from threading import RLock
from typing import Any
from uuid import uuid4

from .errors import ReportRunError


def canonical_json(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"),
                      allow_nan=False).encode("utf-8")


def fingerprint(value: Any) -> str:
    return sha256(canonical_json(value)).hexdigest()


@dataclass(frozen=True, slots=True)
class SnapshotRef:
    path: str
    sha256: str


def is_redirect(path: Path) -> bool:
    info = path.lstat()
    return stat.S_ISLNK(info.st_mode) or bool(getattr(info, "st_file_attributes", 0) & 0x400)


class SnapshotStore:
    def __init__(self, root: Path, *, max_bytes: int = 512 * 1024 * 1024,
                 max_file_bytes: int = 8 * 1024 * 1024):
        self.root = Path(root).absolute()
        self.max_bytes = max_bytes
        self.max_file_bytes = max_file_bytes
        self._write_lock = RLock()
        # Reject pre-existing redirections before mkdir or any write.
        for component in (self.root, *self.root.parents):
            if component.exists() and is_redirect(component):
                raise ReportRunError("SNAPSHOT_PATH_REDIRECTED")
        self.root.mkdir(parents=True, exist_ok=True)
        self.root = self.root.resolve()

    def _resolve(self, relative: str) -> Path:
        parts = relative.split("/")
        if (len(parts) != 2 or not re.fullmatch(r"[0-9a-f]{32}", parts[0])
                or not re.fullmatch(r"[a-z][a-z0-9_-]*-[0-9a-f]{64}\.json", parts[1])):
            raise ReportRunError("SNAPSHOT_PATH_INVALID")
        path = self.root.joinpath(*parts)
        for component in (self.root, path.parent, path):
            if component.exists() and is_redirect(component):
                raise ReportRunError("SNAPSHOT_PATH_REDIRECTED")
        if not path.resolve().is_relative_to(self.root):
            raise ReportRunError("SNAPSHOT_PATH_INVALID")
        return path

    def total_bytes(self) -> int:
        with self._write_lock:
            return self._total_bytes()

    def _total_bytes(self) -> int:
        total = 0
        for directory, children, files in os.walk(self.root, followlinks=False):
            for name in (*children, *files):
                path = Path(directory) / name
                if is_redirect(path):
                    raise ReportRunError("SNAPSHOT_PATH_REDIRECTED")
            total += sum((Path(directory) / name).stat().st_size for name in files)
        return total

    def put(self, run_id: str, kind: str, value: Any) -> SnapshotRef:
        with self._write_lock:
            return self._put(run_id, kind, value)

    def _put(self, run_id: str, kind: str, value: Any) -> SnapshotRef:
        data = canonical_json(value)
        digest = sha256(data).hexdigest()
        relative = f"{run_id}/{kind}-{digest}.json"
        path = self._resolve(relative)
        if path.exists():
            self.read(SnapshotRef(relative, digest))
            return SnapshotRef(relative, digest)
        if len(data) > self.max_file_bytes or self.total_bytes() + len(data) > self.max_bytes:
            raise ReportRunError("SNAPSHOT_CAPACITY_EXCEEDED")
        path.parent.mkdir(parents=True, exist_ok=True)
        self._resolve(relative)
        temporary = path.parent / f".pending-{uuid4().hex}"
        with temporary.open("xb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        try:
            # Hard-link installation fails if another writer already installed a file.
            # Neither Windows nor POSIX silently overwrites an immutable snapshot.
            os.link(temporary, path)
        except FileExistsError:
            self.read(SnapshotRef(relative, digest))
        finally:
            temporary.unlink(missing_ok=True)
        return SnapshotRef(relative, digest)

    def read(self, reference: SnapshotRef) -> Any:
        path = self._resolve(reference.path)
        try:
            with path.open("rb") as stream:
                data = stream.read(self.max_file_bytes + 1)
        except OSError:
            raise ReportRunError("SNAPSHOT_MISSING") from None
        if len(data) > self.max_file_bytes or sha256(data).hexdigest() != reference.sha256:
            raise ReportRunError("SNAPSHOT_HASH_MISMATCH")
        try:
            return json.loads(data)
        except (ValueError, UnicodeError):
            raise ReportRunError("SNAPSHOT_JSON_INVALID") from None
