"""Run explicit, metadata-only legacy/Rust context shadow comparison."""

from __future__ import annotations

import argparse
from collections.abc import Callable
from datetime import date
from hashlib import sha256
import json
import logging
from pathlib import Path
import re
import sys
from time import perf_counter
from types import SimpleNamespace
from typing import Any

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

from src.core.config import config  # noqa: E402
from src.models.schemas import ScanResult  # noqa: E402
from src.services.context_scheduler import ContextScheduleRequest  # noqa: E402
import src.services.file_scanner as file_scanner_module  # noqa: E402
from src.services.file_scanner import FileScanner  # noqa: E402
from src.services.python_legacy_context_engine import (  # noqa: E402
    PythonLegacyContextEngine,
)
from src.services.rust_context_client import RustContextClient  # noqa: E402


Observation = dict[str, Any]
EngineRunner = Callable[[ContextScheduleRequest, Path, Path], Observation]
_DECISION_FIELDS = (
    "action",
    "reason",
    "priority",
    "input_chars",
    "output_chars",
    "truncated",
    "error_code",
)
_FORBIDDEN_OUTPUT_KEYS = {
    "cache_contents",
    "cell_values",
    "content",
    "excerpt",
    "file_context",
}
_SAFE_CODE = re.compile(r"^[A-Z][A-Z0-9_]{1,127}$")


class _CapturingFileScanner(FileScanner):
    """Capture the public legacy scan result without changing frozen core code."""

    last_scan_result: ScanResult | None = None

    def scan_files(self, *args: Any, **kwargs: Any) -> ScanResult:
        result = super().scan_files(*args, **kwargs)
        self.last_scan_result = result
        return result


def prepare_ephemeral_db_paths(root: str | Path) -> tuple[Path, Path]:
    """Return two verified, distinct DB paths under a caller-owned root."""
    root_path = Path(root)
    if not root_path.is_absolute():
        raise ValueError("ephemeral DB root must be absolute")
    try:
        root_path = root_path.resolve(strict=True)
    except OSError as exc:
        raise ValueError("ephemeral DB root must be an existing directory") from exc
    if not root_path.is_dir():
        raise ValueError("ephemeral DB root must be an existing directory")

    legacy_parent = root_path / "python_legacy"
    rust_parent = root_path / "rust_v2"
    legacy_parent.mkdir()
    rust_parent.mkdir()
    legacy_db = _verified_child(legacy_parent / "scan_index.sqlite3", root_path)
    rust_db = _verified_child(rust_parent / "scan_index_v2.sqlite3", root_path)
    if legacy_db == rust_db:
        raise ValueError("shadow engines must use distinct database paths")
    return legacy_db, rust_db


def build_comparison_payload(
    *,
    legacy: Observation,
    rust: Observation,
    start_date: date,
    end_date: date,
    report_mode: str,
    redact_content: bool,
) -> dict[str, Any]:
    """Compare two sanitized engine observations at the complete context seam."""
    legacy_files = _items_by_path(legacy.get("files", []), "files")
    rust_files = _items_by_path(rust.get("files", []), "files")
    inventory_differences: list[dict[str, str]] = []
    for key in sorted(set(legacy_files) | set(rust_files)):
        legacy_file = legacy_files.get(key)
        rust_file = rust_files.get(key)
        if legacy_file is None or rust_file is None:
            item = legacy_file or rust_file
            inventory_differences.append(
                {
                    "relative_path": str(item["relative_path"]),
                    "present_in": "rust_v2" if legacy_file is None else "python_legacy",
                }
            )

    content_differences: list[dict[str, str]] = []
    for key in sorted(set(legacy_files) & set(rust_files)):
        legacy_file = legacy_files[key]
        rust_file = rust_files[key]
        if legacy_file.get("content_sha256") != rust_file.get("content_sha256"):
            content_differences.append(
                {
                    "relative_path": str(legacy_file["relative_path"]),
                    "python_legacy_sha256": str(legacy_file.get("content_sha256", "")),
                    "rust_v2_sha256": str(rust_file.get("content_sha256", "")),
                    "python_legacy_backend": str(
                        legacy_file.get("parser_backend", "")
                    ),
                    "rust_v2_backend": str(rust_file.get("parser_backend", "")),
                }
            )

    legacy_decisions = _items_by_path(legacy.get("decisions", []), "decisions")
    rust_decisions = _items_by_path(rust.get("decisions", []), "decisions")
    decision_differences: list[dict[str, Any]] = []
    for key in sorted(set(legacy_decisions) | set(rust_decisions)):
        legacy_decision = legacy_decisions.get(key)
        rust_decision = rust_decisions.get(key)
        if legacy_decision is None or rust_decision is None:
            item = legacy_decision or rust_decision
            decision_differences.append(
                {
                    "relative_path": str(item["relative_path"]),
                    "present_in": (
                        "rust_v2" if legacy_decision is None else "python_legacy"
                    ),
                }
            )
            continue
        differing_fields = [
            field
            for field in _DECISION_FIELDS
            if legacy_decision.get(field) != rust_decision.get(field)
        ]
        if differing_fields:
            decision_differences.append(
                {
                    "relative_path": str(legacy_decision["relative_path"]),
                    "fields": differing_fields,
                    "python_legacy": {
                        field: legacy_decision.get(field)
                        for field in differing_fields
                    },
                    "rust_v2": {
                        field: rust_decision.get(field)
                        for field in differing_fields
                    },
                }
            )

    legacy_order = _ordered_paths(legacy.get("decisions", []))
    rust_order = _ordered_paths(rust.get("decisions", []))
    order_differs = legacy_order != rust_order
    payload: dict[str, Any] = {
        "contract": "ai_daily_context_comparison",
        "protocol_version": 1,
        "parameters": {
            "start_date": start_date.isoformat(),
            "end_date": end_date.isoformat(),
            "report_mode": report_mode,
            "redact_content": bool(redact_content),
            "content_policy": "hashes_only",
        },
        "engines": {
            "python_legacy": legacy,
            "rust_v2": rust,
        },
        "inventory_difference_count": len(inventory_differences),
        "inventory_differences": inventory_differences,
        "content_hash_difference_count": len(content_differences),
        "content_hash_differences": content_differences,
        "decision_difference_count": len(decision_differences)
        + int(order_differs),
        "decision_order_equal": not order_differs,
        "decision_differences": decision_differences,
        "final_context_hash_equal": (
            legacy.get("context_sha256") == rust.get("context_sha256")
        ),
        "fallback_count": int(legacy.get("fallback_count", 0))
        + int(rust.get("fallback_count", 0)),
    }
    _assert_metadata_only(payload)
    return payload


def compare_context_engines(
    *,
    work_dir: str | Path,
    start_date: date,
    end_date: date,
    report_mode: str,
    redact_content: bool,
    ephemeral_db_root: str | Path,
    output: str | Path,
    legacy_runner: EngineRunner | None = None,
    rust_runner: EngineRunner | None = None,
) -> dict[str, Any]:
    """Run both explicit adapters once and persist only sanitized comparison data."""
    work_path = Path(work_dir)
    try:
        work_path = work_path.resolve(strict=True)
    except OSError as exc:
        raise ValueError("work directory must be an existing directory") from exc
    if not work_path.is_dir():
        raise ValueError("work directory must be an existing directory")
    if start_date > end_date:
        raise ValueError("start_date must be earlier than or equal to end_date")
    if report_mode not in {"daily", "weekly", "monthly"}:
        raise ValueError(f"unsupported report_mode: {report_mode!r}")

    root_path = Path(ephemeral_db_root).resolve(strict=True)
    output_path = _verified_child(Path(output), root_path)
    legacy_db, rust_db = prepare_ephemeral_db_paths(root_path)
    request = ContextScheduleRequest(
        report_mode=report_mode,
        source="scan",
        start_date=start_date,
        end_date=end_date,
    )
    legacy_observation = (legacy_runner or run_legacy_shadow)(
        request,
        work_path,
        legacy_db,
    )
    rust_observation = (rust_runner or run_rust_shadow)(
        request,
        work_path,
        rust_db,
    )
    payload = build_comparison_payload(
        legacy=legacy_observation,
        rust=rust_observation,
        start_date=start_date,
        end_date=end_date,
        report_mode=report_mode,
        redact_content=redact_content,
    )
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    return payload


def run_legacy_shadow(
    request: ContextScheduleRequest,
    work_dir: Path,
    db_path: Path,
) -> Observation:
    scanner_config = dict(config.scanner_config)
    scanner_config["index_db_path"] = str(db_path)
    comparison_config = SimpleNamespace(
        scanner_config=scanner_config,
        work_dir=work_dir,
    )
    scanner_holder: dict[str, _CapturingFileScanner] = {}

    def scanner_factory() -> _CapturingFileScanner:
        original_config = file_scanner_module.config
        try:
            file_scanner_module.config = comparison_config
            scanner = _CapturingFileScanner()
            scanner_holder["scanner"] = scanner
            return scanner
        finally:
            file_scanner_module.config = original_config

    engine = PythonLegacyContextEngine(
        scanner_factory=scanner_factory,
    )
    started_at = perf_counter()
    envelope = engine.build_context(request)
    build_wall_duration_ms = max(0.0, (perf_counter() - started_at) * 1000)
    scanner = scanner_holder.get("scanner")
    scan_result = None if scanner is None else scanner.last_scan_result
    details_by_path = (
        {}
        if scanner is None
        else {
            _absolute_key(item.path): item
            for item in scanner.last_reparse_details
        }
    )
    files: list[dict[str, Any]] = []
    if scan_result is not None:
        for context in scan_result.contexts:
            detail = details_by_path.get(_absolute_key(context.file_path))
            files.append(
                {
                    "relative_path": _relative_identifier(
                        context.file_path,
                        work_dir,
                    ),
                    "extension": context.file_type.lower(),
                    "parse_status": (
                        ("error" if context.error else "success")
                        if detail is None
                        else detail.parse_status
                    ),
                    "parser_backend": context.parser_backend or "not_parsed",
                    "worker_lane": (
                        "cache"
                        if detail is None
                        else (detail.worker_lane or "unknown")
                    ),
                    "cache_status": "fresh" if detail is None else "miss",
                    "cache_miss_reason": (
                        "" if detail is None else detail.cache_miss_reason
                    ),
                    "truncated": bool(context.truncated),
                    "content_sha256": _content_sha256(context.content),
                    "parse_duration_ms": (
                        0 if detail is None else detail.parse_duration_ms
                    ),
                    "failure_class": (
                        "" if detail is None else detail.failure_class
                    ),
                    "fallback_backend": (
                        "" if detail is None else detail.fallback_backend
                    ),
                    "fallback_reason_code": (
                        ""
                        if detail is None
                        else _reason_code(detail.fallback_reason)
                    ),
                }
            )

    decisions: list[dict[str, Any]] = []
    if scanner is not None and envelope.context_run_id is not None:
        for item in scanner.scan_index_store.list_context_decisions(
            envelope.context_run_id
        ):
            decisions.append(
                {
                    "relative_path": _relative_identifier(
                        str(item["path"]),
                        work_dir,
                    ),
                    "action": item["action"],
                    "reason": item["reason"],
                    "priority": item["priority"],
                    "input_chars": item["input_chars"],
                    "output_chars": item["output_chars"],
                    "truncated": item["truncated"],
                    "error_code": _legacy_error_code(str(item["error"])),
                }
            )

    return {
        "engine": "python_legacy",
        "engine_version": envelope.engine_version,
        "engine_build": envelope.engine_build,
        "status": envelope.status,
        "scan_run_id": envelope.scan_run_id,
        "context_run_id": envelope.context_run_id,
        "summary": envelope.summary.model_dump(mode="json"),
        "context_sha256": _content_sha256(envelope.file_context),
        "normalized_context_sha256": _normalized_context_sha256(
            envelope.file_context,
            work_dir,
        ),
        "build_wall_duration_ms": round(build_wall_duration_ms, 3),
        "files": files,
        "decisions": decisions,
        "warning_codes": [item.error_code for item in envelope.warnings],
        "error_code": None if envelope.error is None else envelope.error.error_code,
        "fallback_count": sum(bool(item["fallback_backend"]) for item in files),
    }


def run_rust_shadow(
    request: ContextScheduleRequest,
    work_dir: Path,
    db_path: Path,
) -> Observation:
    comparison_config = SimpleNamespace(
        work_dir=work_dir,
        scanner_contract_profile=config.scanner_contract_profile,
    )
    client = RustContextClient(
        config=comparison_config,
        scanner_binary=config.rust_scanner_bin,
        scan_db_path=db_path,
        timeout_seconds=config.rust_process_timeout_seconds,
    )
    started_at = perf_counter()
    envelope = client.build_context(request)
    build_wall_duration_ms = max(0.0, (perf_counter() - started_at) * 1000)
    files: list[dict[str, Any]] = []
    decisions: list[dict[str, Any]] = []
    run_status: str | None = None
    inspection_warning_codes: list[str] = []
    if envelope.scan_run_id is not None:
        inspection = client.inspect_run(
            envelope.scan_run_id,
            include_content=False,
        )
        if inspection.context_run_id != envelope.context_run_id:
            raise ValueError("Rust build and inspect context run ids disagree")
        run_status = inspection.run_status
        files = [
            {
                "relative_path": _normalize_relative(item.relative_path),
                "extension": Path(item.relative_path).suffix.lower(),
                "parse_status": item.parse_status,
                "parser_backend": item.parser_backend,
                "worker_lane": item.worker_lane,
                "cache_status": item.cache_status,
                "cache_miss_reason": item.cache_miss_reason,
                "truncated": item.truncated,
                "content_sha256": item.content_sha256,
                "parse_duration_ms": item.parse_duration_ms,
                "failure_class": item.failure_class,
                "fallback_backend": item.fallback_backend,
                "fallback_reason_code": item.fallback_reason_code,
            }
            for item in inspection.files
        ]
        decisions = [
            {
                **item.model_dump(mode="json"),
                "relative_path": _normalize_relative(item.relative_path),
            }
            for item in inspection.decisions
        ]
        inspection_warning_codes = [
            item.error_code for item in inspection.warnings
        ]

    return {
        "engine": "rust_v2",
        "engine_version": envelope.engine_version,
        "engine_build": envelope.engine_build,
        "status": envelope.status,
        "run_status": run_status,
        "scan_run_id": envelope.scan_run_id,
        "context_run_id": envelope.context_run_id,
        "summary": envelope.summary.model_dump(mode="json"),
        "context_sha256": _content_sha256(envelope.file_context),
        "normalized_context_sha256": _normalized_context_sha256(
            envelope.file_context,
            work_dir,
        ),
        "build_wall_duration_ms": round(build_wall_duration_ms, 3),
        "files": files,
        "decisions": decisions,
        "warning_codes": list(
            dict.fromkeys(
                [item.error_code for item in envelope.warnings]
                + inspection_warning_codes
            )
        ),
        "error_code": None if envelope.error is None else envelope.error.error_code,
        "fallback_count": sum(bool(item["fallback_backend"]) for item in files),
    }


def _verified_child(path: Path, root: Path) -> Path:
    candidate = path
    if not candidate.is_absolute():
        candidate = root / candidate
    candidate = candidate.resolve(strict=False)
    if not candidate.is_relative_to(root):
        raise ValueError("output must stay under the ephemeral DB root")
    return candidate


def _relative_identifier(path: str | Path, work_dir: Path) -> str:
    candidate = Path(path).resolve(strict=False)
    try:
        relative = candidate.relative_to(work_dir)
    except ValueError as exc:
        raise ValueError(
            "legacy scanner returned a path outside the work directory"
        ) from exc
    return _normalize_relative(str(relative))


def _normalize_relative(value: str) -> str:
    normalized = value.replace("\\", "/").strip("/")
    parts = [part for part in normalized.split("/") if part not in {"", "."}]
    if not parts or any(part == ".." for part in parts):
        raise ValueError("comparison received an invalid relative identifier")
    return "/".join(parts)


def _absolute_key(value: str | Path) -> str:
    return str(Path(value).resolve(strict=False)).casefold()


def _content_sha256(content: str) -> str:
    return sha256(content.encode("utf-8")).hexdigest()


def _normalized_context_sha256(content: str, work_dir: Path) -> str:
    """Hash context after replacing the fixture root with a stable token."""
    resolved = work_dir.resolve(strict=True)
    normalized = content
    variants = {str(resolved), resolved.as_posix()}
    for variant in sorted(variants, key=len, reverse=True):
        normalized = normalized.replace(variant, "<WORK_DIR>")
    return _content_sha256(normalized)


def _reason_code(reason: str) -> str:
    candidate = reason.partition(":")[0].strip().upper()
    return candidate if _SAFE_CODE.fullmatch(candidate) else "LEGACY_FALLBACK"


def _legacy_error_code(error: str) -> str:
    normalized = error.strip().lower()
    if not normalized:
        return ""
    if normalized.startswith("file too large:"):
        return "FILE_TOO_LARGE"
    if normalized.startswith("timeout:") or "timeout: file parse exceeded" in normalized:
        return "PARSER_TIMEOUT"
    return "PARSER_FAILED"


def _items_by_path(items: Any, label: str) -> dict[str, dict[str, Any]]:
    if not isinstance(items, list):
        raise ValueError(f"{label} must be a list")
    result: dict[str, dict[str, Any]] = {}
    for item in items:
        if not isinstance(item, dict):
            raise ValueError(f"{label} entries must be objects")
        relative_path = _normalize_relative(str(item.get("relative_path", "")))
        normalized = {**item, "relative_path": relative_path}
        key = relative_path.casefold()
        if key in result:
            raise ValueError(f"{label} contains duplicate relative identifiers")
        result[key] = normalized
    return result


def _ordered_paths(items: Any) -> list[str]:
    if not isinstance(items, list):
        raise ValueError("decisions must be a list")
    return [
        _normalize_relative(str(item.get("relative_path", ""))).casefold()
        for item in items
    ]


def _assert_metadata_only(value: Any) -> None:
    if isinstance(value, dict):
        forbidden = _FORBIDDEN_OUTPUT_KEYS.intersection(value)
        if forbidden:
            raise ValueError(
                "comparison output contains forbidden content fields: "
                + ", ".join(sorted(forbidden))
            )
        for child in value.values():
            _assert_metadata_only(child)
    elif isinstance(value, list):
        for child in value:
            _assert_metadata_only(child)


def _parse_date(value: str) -> date:
    try:
        return date.fromisoformat(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("date must use YYYY-MM-DD") from exc


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Compare complete Python legacy and Rust context engines",
    )
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--start-date", type=_parse_date, required=True)
    parser.add_argument("--end-date", type=_parse_date, required=True)
    parser.add_argument(
        "--report-mode",
        choices=("daily", "weekly", "monthly"),
        default="daily",
    )
    parser.add_argument("--redact-content", action="store_true")
    parser.add_argument("--ephemeral-db-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    return parser


def main() -> int:
    args = build_parser().parse_args()
    logging.getLogger("ai_daily_report").setLevel(logging.CRITICAL + 1)
    try:
        payload = compare_context_engines(
            work_dir=args.work_dir,
            start_date=args.start_date,
            end_date=args.end_date,
            report_mode=args.report_mode,
            redact_content=args.redact_content,
            ephemeral_db_root=args.ephemeral_db_root,
            output=args.output,
        )
    except Exception as exc:
        print(
            f"context engine comparison failed ({type(exc).__name__})",
            file=sys.stderr,
        )
        return 1
    print(
        json.dumps(
            {
                "inventory_difference_count": payload[
                    "inventory_difference_count"
                ],
                "content_hash_difference_count": payload[
                    "content_hash_difference_count"
                ],
                "decision_difference_count": payload[
                    "decision_difference_count"
                ],
                "fallback_count": payload["fallback_count"],
            },
            sort_keys=True,
        )
    )
    statuses = [
        payload["engines"]["python_legacy"]["status"],
        payload["engines"]["rust_v2"]["status"],
    ]
    return 1 if "error" in statuses else 0


if __name__ == "__main__":
    raise SystemExit(main())
