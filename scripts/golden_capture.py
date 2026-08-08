"""Capture the pre-change / post-change scanner golden on a frozen synthetic corpus.

Runs the release scanner binary end-to-end (build-context + inspect-run), reads
the scan DB for the exact context_sha256 and per-file decisions, and writes an
anonymous golden JSON (no real paths, no file bodies).

Usage:
  python scripts/golden_capture.py --out .artifacts/golden-pre-change.json
  python scripts/golden_capture.py --out .artifacts/golden-post-change.json --tag post-change
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import sqlite3
import subprocess
import sys
import uuid
from datetime import date, datetime, timezone
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT_ROOT))

CORPUS_DIR = PROJECT_ROOT / ".artifacts" / "golden-corpus"
START = date(2026, 8, 1)
END = date(2026, 8, 8)

# A small text used for a deterministic "large" file that exceeds the weekly
# per-file budget (5000 chars) so the compressor picks Compress.
LARGE_BODY = "工作记录 " * 1200  # 2400 chars -> > 2000? weekly per_file 5000
LARGE_BODY = "line of evidence\n" * 800  # ~19k chars -> Compress under 5000 budget
SMALL_BODY = "今日完成扫描调度器组装与状态矩阵重定义。\n"


def write_with_mtime(path: Path, content: str, mtime: datetime) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    os.utime(path, (mtime.timestamp(), mtime.timestamp()))


def build_corpus(work_dir: Path) -> None:
    base_time = datetime(2026, 8, 5, 10, 0, 0, tzinfo=timezone.utc)
    files = {
        "notes/meeting.md": SMALL_BODY,
        "notes/history.md": LARGE_BODY,
        "report.txt": SMALL_BODY,
        "data/over_budget.md": "x" * (2 * 1024 * 1024),  # 2 MiB > 1 MiB limit
    }
    for index, (rel, content) in enumerate(files.items()):
        mtime = base_time.replace(hour=10 + index % 6)
        write_with_mtime(work_dir / rel, content, mtime)


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def run_build_context(
    *,
    scanner: Path,
    office_worker: Path,
    python_executable: Path,
    work_dir: Path,
    db_path: Path,
    request_id: str,
    profile: dict,
) -> dict:
    request = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": request_id,
        "work_dir": str(work_dir),
        "start_date": START.isoformat(),
        "end_date": END.isoformat(),
        "report_mode": "weekly",
        "compression_profile": None,
        "scan_db_path": str(db_path),
        "scanner_profile": profile,
        "adapters": {
            "office_worker_path": str(office_worker),
            "python_executable": str(python_executable),
            "python_module_root": str(PROJECT_ROOT),
            "python_document_worker_module": "src.workers.document_parser_worker",
        },
    }
    proc = subprocess.run(
        [str(scanner), "build-context"],
        input=json.dumps(request).encode("utf-8"),
        capture_output=True,
        timeout=120,
    )
    stdout = proc.stdout.decode("utf-8", errors="replace")
    try:
        envelope = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(
            f"scanner did not return JSON (exit={proc.returncode}): {stdout[:400]}"
        ) from exc
    if proc.returncode != 0 or envelope.get("status") == "error":
        raise RuntimeError(
            f"build-context failed exit={proc.returncode} status={envelope.get('status')} "
            f"error={envelope.get('error')} warnings={envelope.get('warnings')}"
        )
    return envelope


def run_inspect(scanner: Path, db_path: Path, scan_run_id: int) -> dict:
    request = {
        "contract": "ai_daily_context",
        "protocol_version": 1,
        "request_id": str(uuid.uuid4()),
        "scan_db_path": str(db_path),
        "scan_run_id": scan_run_id,
        "include_content": False,
    }
    proc = subprocess.run(
        [str(scanner), "inspect-run"],
        input=json.dumps(request).encode("utf-8"),
        capture_output=True,
        timeout=60,
    )
    stdout = proc.stdout.decode("utf-8", errors="replace")
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(
            f"inspect-run did not return JSON (exit={proc.returncode}): {stdout[:400]}"
        ) from exc
    if proc.returncode != 0:
        raise RuntimeError(
            f"inspect-run failed exit={proc.returncode}: {payload.get('error')}"
        )
    return payload


def decision_sort_key(decision: dict) -> tuple:
    return (
        decision["priority"],
        decision["relative_path"].lower(),
        decision["relative_path"],
        decision["identity_hash"],
    )


def capture_golden(*, tag: str) -> dict:
    scanner = PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-scanner.exe"
    office_worker = PROJECT_ROOT / "rust" / "target" / "release" / "ai-daily-office-parser.exe"
    python_executable = Path(sys.executable).resolve()
    assert scanner.is_file(), scanner
    assert office_worker.is_file(), office_worker

    corpus = CORPUS_DIR
    if corpus.exists():
        import shutil

        shutil.rmtree(corpus)
    build_corpus(corpus)

    temp_root = PROJECT_ROOT / ".artifacts" / "golden-db"
    temp_root.mkdir(parents=True, exist_ok=True)
    db_path = temp_root / f"db_{uuid.uuid4().hex[:8]}" / "scan_index_v2.sqlite3"
    db_path.parent.mkdir(parents=True, exist_ok=True)

    profile = {
        "schema_version": "scanner_profile_v1",
        "max_file_size_mb": 1,
    }
    envelope = run_build_context(
        scanner=scanner,
        office_worker=office_worker,
        python_executable=python_executable,
        work_dir=corpus,
        db_path=db_path,
        request_id=str(uuid.uuid4()),
        profile=profile,
    )
    scan_run_id = envelope["scan_run_id"]
    context_run_id = envelope["context_run_id"]
    inspection = run_inspect(scanner, db_path, scan_run_id)
    files = {item["relative_path"]: item for item in inspection["files"]}

    connection = sqlite3.connect(db_path)
    row = connection.execute(
        "SELECT context_sha256, final_context, source_file_count, success_count,"
        " timeout_count, included_file_count, omitted_file_count, error_file_count,"
        " input_chars, output_chars"
        " FROM context_runs WHERE context_run_id=?",
        (context_run_id,),
    ).fetchone()
    if row is None:
        raise RuntimeError("context_runs row is missing")
    (
        context_sha256,
        final_context,
        source_file_count,
        success_count,
        timeout_count,
        included_file_count,
        omitted_file_count,
        error_file_count,
        input_chars,
        output_chars,
    ) = row
    decisions_raw = connection.execute(
        "SELECT file_identity, relative_path, action, reason, priority, input_chars,"
        " output_chars, truncated, error_code"
        " FROM context_decisions WHERE context_run_id=?",
        (context_run_id,),
    ).fetchall()
    connection.close()

    decisions = []
    for (
        file_identity,
        relative_path,
        action,
        reason,
        priority,
        d_input,
        d_output,
        truncated,
        error_code,
    ) in decisions_raw:
        decisions.append(
            {
                "identity_hash": sha256_text(file_identity),
                "relative_path": relative_path,
                "action": action,
                "reason": reason,
                "priority": priority,
                "input_chars": d_input,
                "output_chars": d_output,
                "truncated": bool(truncated),
                "error_code": error_code or "",
                "parse_status": files.get(relative_path, {}).get("parse_status", "n/a"),
                "parser_backend": files.get(relative_path, {}).get("parser_backend", "n/a"),
            }
        )
    decisions.sort(key=decision_sort_key)

    included = [
        d["relative_path"]
        for d in decisions
        if d["action"] in ("keep", "compress", "metadata_only")
    ]
    omitted = [
        d["relative_path"] for d in decisions if d["action"] == "omit"
    ]
    errors = [d["relative_path"] for d in decisions if d["action"] == "error"]

    # Prove the current ordering bug: a non-Success file jumps to priority 80.
    error_file = next((d for d in decisions if d["parse_status"] != "success"), None)
    pre_change_order_evidence = None
    if error_file is not None:
        pre_change_order_evidence = {
            "file": error_file["relative_path"],
            "parse_status": error_file["parse_status"],
            "assigned_priority": error_file["priority"],
            "bug": (
                error_file["priority"] == 80
                and error_file["parse_status"] in ("error", "timeout", "not_parsed")
            ),
            "note": "legacy decide_files maps every non-Success status to priority 80",
        }

    return {
        "tag": tag,
        "corpus_manifest": {
            "count": 4,
            "files": sorted(files.keys()),
            "extensions": sorted({Path(p).suffix for p in files}),
        },
        "context_sha256": context_sha256,
        "context_output_chars": output_chars,
        "summary": {
            "source_file_count": source_file_count,
            "success_count": success_count,
            "timeout_count": timeout_count,
            "included_file_count": included_file_count,
            "omitted_file_count": omitted_file_count,
            "error_file_count": error_file_count,
            "input_chars": input_chars,
            "output_chars": output_chars,
        },
        "included": included,
        "omitted": omitted,
        "error_files": errors,
        "reason_sets": {
            "included_reasons": sorted(
                {d["reason"] for d in decisions if d["action"] in ("keep", "compress", "metadata_only")}
            ),
            "omitted_reasons": sorted({d["reason"] for d in decisions if d["action"] == "omit"}),
            "error_reasons": sorted({d["reason"] for d in decisions if d["action"] == "error"}),
        },
        "parse_order": [d["relative_path"] for d in decisions],
        "decisions": decisions,
        "pre_change_order_evidence": pre_change_order_evidence,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--tag", default="pre-change")
    args = parser.parse_args()
    golden = capture_golden(tag=args.tag)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(golden, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(golden, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
