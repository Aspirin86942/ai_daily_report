"""Verify the immutable synthetic corpus through the installed native scanner."""
from __future__ import annotations

import argparse
from datetime import date, timedelta
from hashlib import sha256
import json
from pathlib import Path
import shutil
import sys
from time import perf_counter
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from src.models.coverage import source_audit
from src.services.native_scanner import NativeScanner, ScanRequest
from src.services.report_runner.scan_evidence import annotate_sources


def verify(corpus: Path, output: Path) -> dict:
    truth_bytes = (corpus / "truth.json").read_bytes()
    truth = json.loads(truth_bytes)
    target = date.fromisoformat(truth["target_date"])
    output.mkdir(parents=True, exist_ok=False)
    profile = {"allowed_extensions": [".docx", ".xlsx", ".xls", ".pptx", ".pdf", ".csv"],
        "legacy_office_enabled": True, "pptx_include_notes": True,
        "csv_encodings": ["utf-8", "utf-8-sig", "gb18030"],
        "total_max_chars": 50000, "text_max_chars": 6000, "excel_max_rows": 50,
        "pdf_max_pages": 5, "max_candidate_files": 96,
        "max_total_pdf_classification_pages": 80, "max_pdf_text_extractions": 8}
    rows = []
    for case in truth["cases"]:
        original = corpus / "work" / case["path"]
        assert sha256(original.read_bytes()).hexdigest() == case["sha256"], case["path"]
        directory = output / case["path"].replace(".", "_")
        work = directory / "work"
        work.mkdir(parents=True)
        shutil.copy2(original, work / original.name)
        scanner = NativeScanner(SimpleNamespace(work_dir=work,
            index_db_path=directory / "scan_index_v3.sqlite3",
            office_worker_path=ROOT / "rust/target/release/ai-daily-office-parser.exe",
            scanner_settings=lambda: profile))
        modes = []
        for mode, start in (("cold", target), ("parse_cache", target - timedelta(days=1)),
                            ("snapshot", target - timedelta(days=1))):
            started = perf_counter()
            scan = annotate_sources(scanner.build_context(ScanRequest("daily", start, target)))
            assert len(scan.sources) == 1, (case["path"], mode, scan.envelope.error)
            source = scan.sources[0]
            audit = source_audit(source)
            boundaries = case["boundaries"]
            for fact in case["expected_facts"]:
                assert fact in source.excerpt, (case["path"], mode, fact)
            if boundaries.get("expected_failure"):
                assert audit.primary_status == "parse_failed", (case["path"], audit.primary_status)
            elif boundaries.get("valid_empty_document"):
                assert not source.excerpt and audit.primary_status == "empty_excerpt"
                assert audit.primary_reason == "empty_document"
            elif case["path"] == "no_text.pdf":
                assert audit.primary_reason == "pdf_window_no_text" and not source.excerpt
                assert audit.pdf_page_audit.checked_pages == [1, 2, 8, 3, 5]
            if boundaries.get("outside_default_prefix"):
                assert "148500" not in source.excerpt and "全表合计" not in source.excerpt
            if boundaries.get("actual_format"):
                assert audit.detected_format == boundaries["actual_format"]
            if boundaries.get("encoding"):
                assert audit.encoding == boundaries["encoding"]
            if boundaries.get("text_page"):
                assert any(r.unit == "pdf_page" and r.start <= boundaries["text_page"] < r.end
                           for r in audit.read_ranges)
            metrics = scan.evidence.execution_metrics
            if mode != "cold" and not boundaries.get("expected_failure"):
                assert metrics.parse_attempt_count == 0 and metrics.classify_attempt_count == 0
                assert metrics.confirmed_run_inspected_pages_total == 0
            modes.append({"mode": mode, "elapsed_ms": round((perf_counter() - started) * 1000, 3),
                "parse_status": source.parse_status, "parser_backend": source.parser_backend,
                "worker_lane": source.worker_lane, "excerpt_sha256": source.excerpt_sha256,
                "facts_found": len(case["expected_facts"]), "audit": audit.model_dump(mode="json"),
                "execution_metrics": metrics.model_dump(mode="json")})
        assert len({item["excerpt_sha256"] for item in modes}) == 1
        changed = {key: {item["mode"]: item["audit"][key] for item in modes}
                   for key in modes[0]["audit"]
                   if len({json.dumps(item["audit"][key], sort_keys=True) for item in modes}) > 1}
        assert not changed, (case["path"], changed)
        rows.append({"path": case["path"], "sha256": case["sha256"], "results": modes})
    value = {"version": 1, "truth_sha256": sha256(truth_bytes).hexdigest(),
        "profile": profile, "model_calls": 0, "cases": rows,
        "facts_expected": sum(len(case["expected_facts"]) for case in truth["cases"]),
        "facts_found": sum(row["results"][0]["facts_found"] for row in rows)}
    (output / "evidence.json").write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n",
                                          encoding="utf-8", newline="\n")
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    evidence = verify(args.corpus, args.output)
    print(json.dumps({"cases": len(evidence["cases"]), "facts_found": evidence["facts_found"],
        "facts_expected": evidence["facts_expected"], "model_calls": 0}))


if __name__ == "__main__":
    main()
