"""Freeze synthetic Python contracts before removing the Python implementation.

This is a development-only migration tool. It never opens the production report
database, sends a network request, or reads files from the configured work root.
"""
from __future__ import annotations

import ast
from contextlib import closing
from dataclasses import replace
from datetime import UTC, datetime
from hashlib import sha256
import importlib.util
import json
import os
from pathlib import Path
import re
import sqlite3
import sys
from tempfile import TemporaryDirectory
from time import perf_counter

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from src.models.schemas import MonthlyReportData, WeeklyReportData
from src.services.native_scanner import NativeScanner, ScanRequest
from src.services.report_gen import ReportGenerator
from src.services.scan_preview import ReportScannerConfig
from src.services.report_runner.snapshots import canonical_json

FIXTURES = ROOT / "rust/report_app/tests/fixtures"
ARTIFACTS = ROOT / ".artifacts/rust-migration"
NOW = datetime(2026, 10, 12, 1, tzinfo=UTC)


def write_json(name, value):
    data = canonical_json(value)
    (FIXTURES / name).write_bytes(data)


def main():
    FIXTURES.mkdir(parents=True, exist_ok=True)
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    spec = importlib.util.spec_from_file_location("baseline_helpers", ROOT / "tests/test_evidence_daily_service.py")
    helpers = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helpers)
    with TemporaryDirectory(prefix="report-migration-") as temporary:
        scratch = Path(temporary)
        cfg = helpers.config.__wrapped__(scratch)
        transport = helpers.Transport()
        app = helpers.service(cfg, helpers.Scanner(helpers.scan_result(helpers.source())), transport)
        context = replace(helpers.context(), run_id="0123456789abcdef0123456789abcdef")
        started = perf_counter()
        outcome = app.run(context)
        fixed_report_ms = (perf_counter() - started) * 1000
        (FIXTURES / "daily-v2.md").write_bytes(outcome.markdown.encode("utf-8"))
        report = app.runs.store.get_report(outcome.target_date)
        report_data = report.model_dump(mode="json")
        report_data["is_historical_reconstruction"] = report.is_historical_reconstruction
        report_data["quality_label"] = report.quality_label
        report_data["source_labels"] = report.source_labels
        report_data["coverage_summary"] = report.coverage_summary.model_dump(mode="json")
        report_data["coverage_summary"]["primary_reason_labels"] = report.coverage_summary.primary_reason_labels
        write_json("daily-v2.json", report_data)
        write_json("model-request-v2.json", transport.requests[0])
        write_json("model-response-v2.json", transport.send(transport.requests[0], timeout=1))
        write_json("context-v2.json", context.to_dict())
        run = app.runs.get(context.run_id)
        snapshot = cfg.data_dir / "report_snapshots" / run["snapshot_path"]
        if not snapshot.exists():
            snapshot = app.snapshots.root / run["snapshot_path"]
        (FIXTURES / "input-snapshot-v2.json").write_bytes(snapshot.read_bytes())
        with closing(sqlite3.connect(cfg.db_dir / "reports.sqlite3")) as db:
            schema = "\n\n".join(row[0] + ";" for row in db.execute(
                "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' ORDER BY type DESC, name"))
        (FIXTURES / "report-schema-v1.sql").write_bytes((schema + "\n").encode("utf-8"))
        renderer = ReportGenerator(cfg.reports_dir)
        weekly = WeeklyReportData(week_label="2026-W41", date_range="2026-10-05 ~ 2026-10-11",
            completed_work="材料包含采购审批方案草稿，金额单位为人民币元。", self_growth="明确了审批条件。",
            improvement_actions="补充例外情形。", work_summary="方案尚待评审，不能据文件修改时间认定已经上线。",
            next_plan="评审后确认生效日期。", support_needed="请业务负责人确认金额口径。", other_notes="未提供上线记录。")
        monthly = MonthlyReportData(year_month="2026-10", overview="本月材料涉及采购审批规则整理。",
            completed_work="材料中列出了金额档位和例外条件。", work_summary="尚无生效凭据。", next_plan="确认后发布制度。")
        for kind, value in (("weekly", weekly), ("monthly", monthly)):
            data = value.model_dump(mode="json")
            write_json(f"{kind}-v1.json", data)
            template = renderer.env.get_template(f"{kind}_template.md")
            rendered = template.render(report=value, now="2026-10-12 09:00")
            (FIXTURES / f"{kind}-v1.md").write_bytes(rendered.encode("utf-8"))
        canonical_cases = [
            {"中文": "逐项核对\n换行\r\n", "a": None, "b": True, "nested": {"z": [], "a": 1}},
            {"float": 1.0, "negative_zero": -0.0, "small": 1e-7, "large": 1e20, "decimal": 0.00001},
            {"unicode": "😀é\u2028", "escape": '"\\\t', "int": 9223372036854775807},
        ]
        write_json("canonical-json.json", [{"input": value, "utf8": canonical_json(value).decode(),
            "sha256": sha256(canonical_json(value)).hexdigest()} for value in canonical_cases])
        for index in range(32):
            path = cfg.work_dir / f"synthetic-{index:02d}.md"
            path.write_bytes((f"采购方案 {index}\n" + "金额五万元以内由部门负责人审批；紧急采购另行登记。\n" * 12).encode())
            timestamp = datetime(2026, 10, 11, 1, tzinfo=UTC).timestamp()
            os.utime(path, (timestamp, timestamp))
        scanner = NativeScanner(ReportScannerConfig(cfg))
        request = ScanRequest("daily", context.target_date, context.target_date,
            window_start_utc=context.window_start_utc, window_end_utc=context.window_end_utc)
        timings = []
        for _ in range(2):
            started = perf_counter()
            result = scanner.build_context(request)
            timings.append({"elapsed_ms": round((perf_counter() - started) * 1000, 3),
                            "sources": len(result.sources), "status": result.envelope.status})
        performance = {"input": "32 synthetic Markdown files; fixed 2026-10-11 modification time",
            "scan_cold": timings[0], "scan_warm": timings[1], "fixed_response_report_ms": round(fixed_report_ms, 3),
            "scope": "same-machine baseline, synthetic input, no network; no process peak-memory measurement"}
        (ARTIFACTS / "python-performance.json").write_bytes(canonical_json(performance))
        del scanner
        states = []
        preview = app.run(replace(context, run_id="1" * 32), save=False)
        states.append({"case": "preview", "action": preview.action, "output": None,
                       "model_calls": preview.model_calls})
        outcome.output_path.write_bytes(b"external edit\n")
        try:
            app.repair(outcome.target_date)
            raise AssertionError("external edits must conflict")
        except helpers.ReportRunError as error:
            states.append({"case": "external_edit", "error": error.code,
                           "output_preserved": outcome.output_path.read_bytes() == b"external edit\n"})
        outcome.output_path.unlink()
        repaired = app.repair(outcome.target_date)
        states.append({"case": "repair", "action": repaired.action, "model_calls": repaired.model_calls,
                       "sha256": sha256(repaired.output_path.read_bytes()).hexdigest()})
        recovery_root = scratch / "partial"
        recovery_root.mkdir()
        recovery_cfg = helpers.config.__wrapped__(recovery_root)
        interrupted = helpers.Transport(interrupt_at=2)
        recovery = helpers.service(recovery_cfg,
            helpers.Scanner(helpers.scan_result(helpers.source(text="合成审批条件。" * 3000))), interrupted)
        try:
            recovery.run(replace(context, run_id="2" * 32))
            raise AssertionError("expected interrupted second batch")
        except KeyboardInterrupt:
            before = [call["status"] for call in recovery.runs.calls("2" * 32)]
        resumed = recovery.run(replace(context, run_id="3" * 32))
        states.append({"case": "partial_recovery", "before": before,
                       "after": [call["status"] for call in recovery.runs.calls(resumed.run_id)],
                       "resumed_run_id": resumed.run_id, "requests_total": len(interrupted.requests)})
        write_json("state-scenarios.json", states)
    options = {}
    for source in ("src/cli/parser.py", "scripts/backfill_daily_reports.py", "scripts/daily_task_plan.py"):
        tree = ast.parse((ROOT / source).read_text(encoding="utf-8-sig"))
        options[source] = [dict(line=node.lineno,
            flags=[arg.value for arg in node.args if isinstance(arg, ast.Constant) and isinstance(arg.value, str)])
            for node in ast.walk(tree) if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
            and node.func.attr == "add_argument"]
    write_json("legacy-cli-options.json", options)
    manifest = {path.name: sha256(path.read_bytes()).hexdigest() for path in sorted(FIXTURES.iterdir())
        if path.is_file() and path.name != "manifest.json"}
    write_json("manifest.json", {"baseline_commit": "069045f", "clock": NOW.isoformat(), "files": manifest})
    rust_log = (ARTIFACTS / "rust-baseline.log").read_text(encoding="utf-8-sig")
    count = sum(int(item) for item in re.findall(r"test result: ok\. (\d+) passed", rust_log))
    print(json.dumps({"fixtures": len(manifest), "rust_baseline_passed": count, "performance": performance}))


if __name__ == "__main__":
    main()
