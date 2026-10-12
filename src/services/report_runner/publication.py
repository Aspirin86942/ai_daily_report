"""Publish exact staged Markdown while preserving external edits and crash recovery."""

from __future__ import annotations

import ctypes
from hashlib import sha256
import os
from pathlib import Path
from uuid import uuid4

from ..report_run_store import ReportRunStore
from .context import parse_report_date
from .errors import ReportRunError
from .snapshots import is_redirect


def _hash(path: Path) -> str | None:
    try:
        if is_redirect(path):
            raise ReportRunError("OUTPUT_PATH_REDIRECTED")
        digest = sha256()
        with path.open("rb") as stream:
            # Report files should be small; reject unknown large files without reading all data.
            remaining = 8 * 1024 * 1024
            while chunk := stream.read(min(remaining + 1, 65536)):
                remaining -= len(chunk)
                if remaining < 0:
                    raise ReportRunError("OUTPUT_FILE_TOO_LARGE")
                digest.update(chunk)
        return digest.hexdigest()
    except FileNotFoundError:
        return None


def _replace_with_backup(target: Path, candidate: Path, backup: Path) -> None:
    if os.name == "nt":
        from ctypes import wintypes
        replace = ctypes.WinDLL("kernel32", use_last_error=True).ReplaceFileW
        replace.argtypes = [wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.LPCWSTR,
                            wintypes.DWORD, wintypes.LPVOID, wintypes.LPVOID]
        replace.restype = wintypes.BOOL
        if not replace(str(target), str(candidate), str(backup), 0, None, None):
            raise ReportRunError("OUTPUT_REPLACE_FAILED")
    else:
        # Production is Windows. This fallback keeps a captured predecessor for Unix tests.
        os.link(target, backup)
        os.replace(candidate, target)


class ReportPublisher:
    def __init__(self, root: Path, runs: ReportRunStore):
        self.root = root.absolute()
        self.runs = runs

    def output_path(self, target_date: str) -> Path:
        parse_report_date(target_date)
        return self.root / target_date[:7] / f"{target_date}.md"

    def _check_path(self, path: Path) -> None:
        if not path.absolute().is_relative_to(self.root):
            raise ReportRunError("OUTPUT_PATH_INVALID")
        for component in (path, *path.parents):
            if component.exists() and is_redirect(component):
                raise ReportRunError("OUTPUT_PATH_REDIRECTED")

    @staticmethod
    def _write_exclusive(path: Path, body: bytes) -> None:
        with path.open("xb") as stream:
            stream.write(body)
            stream.flush()
            os.fsync(stream.fileno())

    def _conflict(self, run: dict, body: bytes) -> Path:
        conflict = self.root / "conflicts" / f"{run['target_date']}.{run['run_id']}.md"
        self._check_path(conflict)
        conflict.parent.mkdir(parents=True, exist_ok=True)
        try:
            self._write_exclusive(conflict, body)
        except FileExistsError:
            if _hash(conflict) != run["expected_sha256"]:
                raise ReportRunError("OUTPUT_CONFLICT_COPY_MODIFIED") from None
        return conflict

    def publish(self, run_id: str) -> Path:
        run = self.runs.get(run_id)
        if run["phase"] not in {"db_committed", "published"} or not run["markdown"]:
            raise ReportRunError("REPORT_NOT_STAGED")
        target = self.output_path(run["target_date"])
        self._check_path(target)
        if Path(run["output_path"]).absolute() != target:
            raise ReportRunError("OUTPUT_LOCATION_CHANGED")
        body = run["markdown"].encode("utf-8")
        if sha256(body).hexdigest() != run["expected_sha256"]:
            raise ReportRunError("STAGED_REPORT_HASH_MISMATCH")
        current = self.runs.current(run["target_date"])
        expected_current = run_id if run["phase"] == "published" else run["previous_run_id"]
        if (current["run_id"] if current else None) != expected_current:
            raise ReportRunError("PUBLICATION_VERSION_CHANGED")
        existing = _hash(target)
        if run["error_code"] == "OUTPUT_CONFLICT_CAPTURED" and existing == run["expected_sha256"]:
            raise ReportRunError("OUTPUT_CONFLICT_CAPTURED")
        if existing == run["expected_sha256"]:
            if run["phase"] == "db_committed":
                self.runs.mark_published(run_id, verified_sha256=existing)
            return target
        allowed_old = run["expected_sha256"] if run["phase"] == "published" else run["previous_sha256"]
        if existing is not None and existing != allowed_old:
            self._conflict(run, body)
            raise ReportRunError("OUTPUT_CONFLICT")
        target.parent.mkdir(parents=True, exist_ok=True)
        self._check_path(target)
        temporary = target.parent / f".{target.name}.{uuid4().hex}.tmp"
        self._write_exclusive(temporary, body)
        try:
            if _hash(target) != existing:
                self._conflict(run, body)
                raise ReportRunError("OUTPUT_CONFLICT")
            if existing is None:
                try:
                    if os.name == "nt":
                        os.rename(temporary, target)
                    else:
                        os.link(temporary, target)
                        temporary.unlink()
                except FileExistsError:
                    self._conflict(run, body)
                    raise ReportRunError("OUTPUT_CONFLICT") from None
            else:
                directory = self.root / "conflicts"
                self._check_path(directory)
                directory.mkdir(parents=True, exist_ok=True)
                backup = directory / f"{run['target_date']}.{run_id}.{uuid4().hex}.replaced.md"
                _replace_with_backup(target, temporary, backup)
                if _hash(backup) != existing:
                    self._conflict(run, body)
                    self.runs.fail(run_id, "OUTPUT_CONFLICT_CAPTURED")
                    raise ReportRunError("OUTPUT_CONFLICT_CAPTURED")
            actual = _hash(target)
            if actual != run["expected_sha256"]:
                self._conflict(run, body)
                raise ReportRunError("OUTPUT_CONFLICT")
            if run["phase"] == "db_committed":
                self.runs.mark_published(run_id, verified_sha256=actual)
            return target
        finally:
            temporary.unlink(missing_ok=True)
