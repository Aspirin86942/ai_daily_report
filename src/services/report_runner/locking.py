"""OS-owned process lock; an existing lock file never means an active owner."""

from __future__ import annotations

from hashlib import sha256
import os
from pathlib import Path
from time import monotonic, sleep

from .errors import ReportRunError


class ReportLock:
    def __init__(self, db_path: Path, *, scope_id: str = "personal_work", wait_seconds: float = 5):
        identity = os.path.normcase(str(db_path.resolve())) + "\0" + scope_id
        self.path = db_path.resolve().parent / f".report-{sha256(identity.encode()).hexdigest()[:24]}.lock"
        self.wait_seconds = wait_seconds
        self.handle = None

    def __enter__(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.handle = self.path.open("a+b")
        if self.handle.seek(0, os.SEEK_END) == 0:
            self.handle.write(b"\0")
            self.handle.flush()
        ends_at = monotonic() + self.wait_seconds
        while True:
            self.handle.seek(0)
            try:
                if os.name == "nt":
                    import msvcrt
                    msvcrt.locking(self.handle.fileno(), msvcrt.LK_NBLCK, 1)
                else:
                    import fcntl
                    fcntl.flock(self.handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                return self
            except OSError:
                if monotonic() >= ends_at:
                    self.handle.close()
                    self.handle = None
                    raise ReportRunError("REPORT_ALREADY_RUNNING") from None
                sleep(min(0.1, max(0.0, ends_at - monotonic())))

    def __exit__(self, *_args):
        if self.handle is not None:
            try:
                self.handle.seek(0)
                if os.name == "nt":
                    import msvcrt
                    msvcrt.locking(self.handle.fileno(), msvcrt.LK_UNLCK, 1)
                else:
                    import fcntl
                    fcntl.flock(self.handle.fileno(), fcntl.LOCK_UN)
            finally:
                self.handle.close()
                self.handle = None
