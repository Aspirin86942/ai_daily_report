"""Opt-in, bounded local OCR protocol; no engine is installed or enabled here."""
from __future__ import annotations

import json
import os
import subprocess
import threading
import time
from hashlib import sha256
from pathlib import Path

from pydantic import BaseModel, ConfigDict, Field, model_validator


class OcrError(ValueError):
    pass


class OcrLimits(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True, frozen=True)
    enabled: bool = False
    max_files: int = Field(default=4, ge=1, le=1000)
    max_pages_per_file: int = Field(default=5, ge=1, le=1000)
    max_total_pages: int = Field(default=10, ge=1, le=10000)
    max_output_chars: int = Field(default=6000, ge=1, le=100000)
    max_runtime_seconds: float = Field(default=60.0, gt=0, le=3600)
    max_file_size_bytes: int = Field(default=52428800, ge=1, le=4294967296)


class OcrPage(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)
    page: int = Field(ge=1)
    text: str
    text_sha256: str = Field(pattern=r"^[a-f0-9]{64}$")

    @model_validator(mode="after")
    def validate_hash(self):
        if sha256(self.text.encode("utf-8")).hexdigest() != self.text_sha256:
            raise ValueError("OCR text identity mismatch")
        return self


class OcrResult(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)
    content_sha256: str = Field(pattern=r"^[a-f0-9]{64}$")
    pages: list[OcrPage] = Field(min_length=1, max_length=1000)
    text_unverified: bool = True

    @model_validator(mode="after")
    def validate_unverified(self):
        if not self.text_unverified or len({page.page for page in self.pages}) != len(self.pages):
            raise ValueError("OCR output must retain unverified text and distinct pages")
        return self


def pdf_identity(path: Path, max_bytes: int) -> tuple[str, int]:
    from src.services.report_runner.snapshots import is_redirect
    import pypdfium2

    path = path.absolute()
    if any(is_redirect(component) for component in (path, *path.parents)):
        raise OcrError("OCR_SOURCE_CHANGED")
    before = path.stat()
    if not 0 < before.st_size <= max_bytes:
        raise OcrError("OCR_SOURCE_SIZE")
    digest = sha256()
    with path.open("rb") as source:
        total = 0
        for data in iter(lambda: source.read(1 << 20), b""):
            total += len(data)
            if total > max_bytes:
                raise OcrError("OCR_SOURCE_SIZE")
            digest.update(data)
    try:
        with pypdfium2.PdfDocument(str(path)) as pdf:
            pages = len(pdf)
    except pypdfium2.PdfiumError:
        raise OcrError("OCR_SOURCE_INVALID") from None
    after = path.stat()
    identity = lambda stat: (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns)
    if identity(before) != identity(after) or pages < 1:
        raise OcrError("OCR_SOURCE_CHANGED")
    return digest.hexdigest(), pages


class ProcessOcrWorker:
    """A JSON stdin/stdout process with bounded output, deadline and cancellation."""

    def __init__(self, command: tuple[str, ...]):
        if not command:
            raise ValueError("OCR command is required")
        self.command = command

    def recognize(self, request: dict, *, deadline: float, cancel: threading.Event,
                  max_output_bytes: int) -> OcrResult:
        if cancel.is_set() or time.monotonic() >= deadline:
            raise OcrError("OCR_CANCELLED" if cancel.is_set() else "OCR_TIMEOUT")
        encoded = json.dumps(request, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        if len(encoded) > 256 * 1024:
            raise OcrError("OCR_REQUEST_LIMIT")
        process = subprocess.Popen(self.command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
        output = bytearray()
        exceeded = threading.Event()
        broken = threading.Event()

        def read():
            try:
                while data := process.stdout.read1(min(4096, max_output_bytes + 1 - len(output))):
                    output.extend(data)
                    if len(output) > max_output_bytes:
                        exceeded.set()
                        break
            except OSError:
                broken.set()

        def write():
            try:
                process.stdin.write(encoded)
                process.stdin.close()
            except OSError:
                broken.set()

        reader = threading.Thread(target=read, daemon=True)
        writer = threading.Thread(target=write, daemon=True)
        reader.start()
        writer.start()
        try:
            while process.poll() is None:
                if exceeded.is_set():
                    raise OcrError("OCR_OUTPUT_LIMIT")
                if cancel.is_set():
                    raise OcrError("OCR_CANCELLED")
                if time.monotonic() >= deadline:
                    raise OcrError("OCR_TIMEOUT")
                time.sleep(min(0.01, max(0, deadline - time.monotonic())))
            reader.join(timeout=0.5)
            writer.join(timeout=0.5)
            if exceeded.is_set():
                raise OcrError("OCR_OUTPUT_LIMIT")
            if process.returncode or broken.is_set() or reader.is_alive() or writer.is_alive():
                raise OcrError("OCR_WORKER_FAILED")
            try:
                return OcrResult.model_validate_json(bytes(output))
            except ValueError:
                raise OcrError("OCR_INVALID_PAYLOAD") from None
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            reader.join(timeout=0.5)
            writer.join(timeout=0.5)
            process.stdout.close()
            if not process.stdin.closed:
                process.stdin.close()


class OcrCoordinator:
    def __init__(self, limits: OcrLimits, worker: ProcessOcrWorker | None = None):
        self.limits, self.worker = limits, worker
        self.files_used = self.pages_used = 0
        self.deadline = time.monotonic() + limits.max_runtime_seconds
        self.lock = threading.Lock()

    def recognize(self, path: Path, pages: list[int], *, cancel: threading.Event | None = None,
                  run_deadline: float | None = None) -> OcrResult:
        if not self.limits.enabled:
            raise OcrError("OCR_DISABLED")
        if self.worker is None:
            raise OcrError("OCR_WORKER_UNAVAILABLE")
        if (not pages or len(pages) > self.limits.max_pages_per_file
            or len(set(pages)) != len(pages) or any(type(page) is not int or page < 1 for page in pages)):
            raise OcrError("OCR_PAGE_LIMIT")
        deadline = min(self.deadline, run_deadline if run_deadline is not None else self.deadline)
        cancel = cancel or threading.Event()
        if cancel.is_set() or time.monotonic() >= deadline:
            raise OcrError("OCR_CANCELLED" if cancel.is_set() else "OCR_TIMEOUT")
        digest, page_count = pdf_identity(path, self.limits.max_file_size_bytes)
        if max(pages) > page_count:
            raise OcrError("OCR_PAGE_OUT_OF_RANGE")
        with self.lock:
            if self.files_used >= self.limits.max_files or self.pages_used + len(pages) > self.limits.max_total_pages:
                raise OcrError("OCR_RUN_LIMIT")
            self.files_used += 1
            self.pages_used += len(pages)
        result = self.worker.recognize({"file_path": str(path.absolute()), "pages": pages,
            "content_sha256": digest, "max_output_chars": self.limits.max_output_chars,
            "text_unverified": True}, deadline=deadline, cancel=cancel,
            max_output_bytes=self.limits.max_output_chars * 12 + 8192 + len(pages) * 256)
        if result.content_sha256 != digest or [item.page for item in result.pages] != pages:
            raise OcrError("OCR_SOURCE_CHANGED")
        if sum(len(item.text) for item in result.pages) > self.limits.max_output_chars:
            raise OcrError("OCR_OUTPUT_LIMIT")
        if not any(item.text.strip() for item in result.pages):
            raise OcrError("OCR_EMPTY")
        if pdf_identity(path, self.limits.max_file_size_bytes) != (digest, page_count):
            raise OcrError("OCR_SOURCE_CHANGED")
        return result
