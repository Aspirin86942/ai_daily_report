"""Local-only preview sharing daily coverage rules without constructing a model or report store."""

from __future__ import annotations

import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ..models.report_settings import RecoverySettings
from .native_scanner import NativeScanner, ScanResult
from .report_runner.context import RunContext
from .report_runner.model_calls import Deadline
from .report_runner.scan_evidence import coverage_gaps, scan_with_retry


@dataclass(frozen=True)
class ScanPreview:
    quality: str
    source_count: int
    usable_count: int
    manifest_path: Path


class ReportScannerConfig:
    """Exclude generated application data even when it is below the scan root."""

    def __init__(self, runtime_config: Any):
        self._config = runtime_config

    def __getattr__(self, name: str) -> Any:
        return getattr(self._config, name)

    def scanner_settings(self) -> dict[str, Any]:
        values = dict(self._config.scanner_settings())
        excluded = set(values.get("excluded_dirs", []))
        excluded.update([".git", ".venv", ".uv", "node_modules", "target", "__pycache__"])
        excluded.update(str(Path(path).resolve()) for path in (
            self._config.data_dir, self._config.reports_dir,
            self._config.db_dir, self._config.log_dir,
        ))
        values["excluded_dirs"] = sorted(excluded)
        values.setdefault("follow_symlinks", False)
        values.setdefault("cloud_files_policy", "local_only")
        return values


def scan_quality(result: ScanResult) -> str:
    envelope = result.envelope
    usable = [s for s in result.sources if s.parse_status == "success" and s.excerpt]
    gaps = coverage_gaps(result)
    if envelope.status == "error":
        return "unavailable"
    if not result.sources:
        return "no_changes" if not gaps else "unavailable"
    if not usable:
        return "unavailable"
    return "partial" if gaps else "complete"


def run_preview(context: RunContext, runtime_config: Any, *, scanner: Any = None) -> ScanPreview:
    if scanner is None:
        index_path = Path(runtime_config.index_db_path)
        if not index_path.is_absolute():
            index_path = Path(__file__).resolve().parents[2] / index_path
        index_path.parent.mkdir(parents=True, exist_ok=True)
        scanner = NativeScanner(ReportScannerConfig(runtime_config))
    limits = getattr(runtime_config, "recovery_settings", RecoverySettings())
    result = scan_with_retry(scanner, context, Deadline(limits.run_budget_seconds))
    quality = scan_quality(result)
    manifest = {
        "schema_version": 1, "kind": "scan_preview", "context": context.to_dict(),
        "quality": quality, "model_calls": 0,
        "summary": result.envelope.summary.model_dump(mode="json"),
        "error_code": result.envelope.error.error_code if result.envelope.error else None,
        "coverage_gaps": [gap.model_dump(mode="json") for gap in coverage_gaps(
            result, root=Path(runtime_config.work_dir))],
        # The checklist retains metadata and coverage only. Business excerpts
        # stay in the native parser cache; they are not copied into diagnostics.
        "sources": [s.model_dump(mode="json", exclude={"excerpt"}) for s in result.sources],
        "warnings": [{"error_code": w.error_code, "stage": w.stage, "file_path": w.file_path}
                     for w in result.envelope.warnings],
    }
    directory = Path(runtime_config.data_dir) / "diagnostics"
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / f"scan-preview-{context.target_date}-{context.run_id}.json"
    temporary = path.with_suffix(".json.tmp")
    with temporary.open("xb") as stream:
        stream.write((json.dumps(manifest, ensure_ascii=False, indent=2) + "\n").encode("utf-8"))
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)
    return ScanPreview(quality, len(result.sources),
                       sum(s.parse_status == "success" and bool(s.excerpt) for s in result.sources), path)
