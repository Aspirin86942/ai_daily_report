"""Exercise the ordinary Windows daily CLI using synthetic PDFs and a loopback model stub."""

import hashlib
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import threading
import uuid
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import win32api
import win32con
import win32job
from pypdf import PdfReader, PdfWriter


REPO = Path(__file__).resolve().parents[1]
STAGE = Path(os.environ.get("AI_DAILY_ACCEPTANCE_OUTPUT", REPO / ".artifacts/kernel-once-20261121/ocr-daily-acceptance")).resolve()
PACKAGED = os.environ.get("AI_DAILY_TEST_BUNDLE")
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", REPO / ".artifacts/rust-migration/cargo-release")).resolve()
BUILT = Path(PACKAGED).resolve(strict=True) if PACKAGED else TARGET / "x86_64-pc-windows-msvc/release"
IMAGE = REPO / "rust/pdf_parser/tests/fixtures/ocr-image-pages.pdf"
TEXT = REPO / "tests/fixtures/pdf_benchmark/case_02.pdf"
ZONE = timezone(timedelta(hours=8))
DAY = datetime.now(ZONE).date() - timedelta(days=1)
ANCHORS = {1: ("4827", "7359", "金额"), 2: ("1936", "2064", "验收")}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def staged_programs(run):
    dest = run / "bin"
    dest.mkdir()
    pin = json.loads((REPO / "rust/build-inputs.json").read_text(encoding="utf-8"))
    for name in ("ai-daily-report.exe", "ai-daily-office-parser.exe", "ai-daily-pdf-parser.exe"):
        shutil.copyfile(BUILT / name, dest / name)
        assert sha(dest / name) == sha(BUILT / name)
    shutil.copyfile((BUILT if PACKAGED else TARGET / "debug") / "pdfium.dll", dest / "pdfium.dll")
    assert sha(dest / "pdfium.dll") == pin["pdfium"]["dll_sha256"]
    model_roots = [BUILT / "ocr-models"] if PACKAGED else [TARGET / "inputs/ppocrv6-tiny", TARGET / "debug/ocr-models"]
    model_files = pin["image_ocr"]["files"]
    model_dir = next(p for p in model_roots if all((p / f["name"]).is_file() for f in model_files))
    (dest / "ocr-models").mkdir()
    for entry in model_files:
        name = entry["name"]
        assert Path(name).name == name
        target = dest / "ocr-models" / name
        shutil.copyfile(model_dir / name, target)
        assert target.stat().st_size == entry["size"] and sha(target) == entry["sha256"]
    shutil.copytree((BUILT if PACKAGED else REPO) / "templates", dest / "templates")
    return dest


def env(root):
    # Existing user credentials, service selections and proxy settings cannot reach this process.
    keys = ("SystemRoot", "WINDIR", "ComSpec", "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS")
    out = {key: os.environ[key] for key in keys if key in os.environ}
    out.update(PATH=str(Path(os.environ["SystemRoot"]) / "System32"), TEMP=str(root), TMP=str(root),
               OPENAI_API_KEY="loopback-test-only", NO_PROXY="127.0.0.1")
    return out


class Reply(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        prompt = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        sources = json.loads(prompt["messages"][1]["content"])["sources"]
        unverified = [s for s in sources if s.get("text_unverified") is True]
        self.server.unverified.append([s["source_id"] for s in unverified])
        items = [{"title": "扫描材料待核对", "text": "材料中的验收金额待核对。",
                  "kind": "attention", "statement_kind": "uncertain", "source_ids": [s["source_id"]]}
                 for s in unverified[:1]]
        if unverified and any(s["source_id"] == "user_input" for s in sources):
            items = [{"title": "用户已确认合成金额 4827 元", "text": "用户已确认合成金额 4827 元。",
                      "kind": "attention", "statement_kind": "user_statement",
                      "source_ids": ["user_input", unverified[0]["source_id"]]}]
        if not items and sources:
            items = [{"title": "合成文档记录收入", "text": "合成文档列示收入 128,400 美元。",
                      "kind": "progress", "statement_kind": "observed", "source_ids": [sources[0]["source_id"]]}]
        payload = json.dumps({"choices": [{"finish_reason": "stop", "message": {
            "content": json.dumps({"schema_version": 5, "items": items}, ensure_ascii=False)}}],
            "usage": {"prompt_tokens": 28, "completion_tokens": 18}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def fixture(directory, name, pages):
    destination = directory / name
    writer = PdfWriter()
    for source, number in pages:
        writer.add_page(PdfReader(source).pages[number - 1])
    writer.write(destination)
    noon = datetime.combine(DAY, datetime.min.time(), tzinfo=ZONE) + timedelta(hours=12)
    os.utime(destination, (noon.timestamp(), noon.timestamp()))
    return destination


def context(run, name):
    root = run / name
    for sub in ("config", "work", "data", "db", "reports", "logs"):
        (root / sub).mkdir(parents=True)
    return root


def daily(root, binary, server, ocr=None, *, dry=True, workers=1, files=1, pages=2, user_note=None):
    scanner = {
        "pdf_worker_path": str(binary / "ai-daily-pdf-parser.exe"),
        "office_worker_path": str(binary / "ai-daily-office-parser.exe"),
        "max_workers": workers, "pdf_max_pages": 3, "max_total_pdf_classification_pages": 60,
        "max_pdf_text_extractions": 1, "total_deadline_ms": 120_000,
    }
    if ocr is not None:
        scanner["pdf_image_ocr"] = {"enabled": ocr, "max_files": files, "max_pages": pages}
    config = {
        "paths": {a: str(root / b) for a, b in [("work_dir", "work"), ("data_dir", "data"),
                  ("db_dir", "db"), ("reports_dir", "reports"), ("log_dir", "logs")]},
        "scanner": scanner, "report": {"profile": "personal"}, "archive": {"allow_work_dir_for_model": True},
        "llm": {"provider": "openai", "base_url": f"http://127.0.0.1:{server.server_port}",
                "model_id": "synthetic-local", "max_request_utf8_bytes": 256000, "context_window_tokens": 256000,
                "verified_max_output_tokens": 8192, "max_tokens": 4096, "request_timeout_seconds": 5},
    }
    (root / "config/settings.yaml").write_text(json.dumps(config), encoding="utf-8")
    cmd = [binary / "ai-daily-report.exe", "--config-dir", root / "config", "daily",
           "--target-date", str(DAY)]
    if user_note is not None:
        cmd.extend(["-i", user_note])
    if dry:
        cmd.append("--dry-run")
    before = len(server.unverified)
    result = subprocess.run(cmd, cwd=root, env=env(root), capture_output=True, timeout=140,
                            creationflags=subprocess.CREATE_NO_WINDOW)
    if result.returncode:
        # Preserve only machine error codes, never arbitrary application text.
        codes = re.findall(rb"\b[A-Z][A-Z0-9_]{4,}\b", result.stderr + result.stdout)
        raise AssertionError(f"CLI exit {result.returncode}, codes {sorted(set(c.decode() for c in codes))[:8]}")
    value = json.loads(result.stdout)
    if dry:
        assert value["action"] == "dry_run" and value["model_calls"] == 0 and before == len(server.unverified)
        return json.loads(value["markdown"])
    assert len(server.unverified) == before + 1 and value["model_calls"] == 1, "expected one loopback model call"
    return value


def excerpts(selection, name):
    return [m for m in selection["materials"] if Path(m["source"]["relative_path"]).name == name]


def classification(root, name):
    path = root / "db/scan_index_rust_v3.sqlite3"
    assert path.is_file(), "isolated scanner index missing"
    with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as db:
        row = db.execute("""
            SELECT e.classification_status, e.classification_page_count, e.classification_result_examined_pages
            FROM scan_file_execution e JOIN scan_file_results f USING (scan_run_id, file_identity)
            WHERE f.relative_path=? ORDER BY e.scan_run_id DESC LIMIT 1
        """, (name,)).fetchone()
    assert row is not None, f"no scanner classification audit for {name}"
    return row


def assert_ocr(materials):
    assert materials, "fully observed image-only PDF was omitted"
    text = "\n".join(m["text"] for m in materials)
    for page, terms in ANCHORS.items():
        assert all(term in text for term in terms), f"missing synthetic anchors for physical page {page}"
    for m in materials:
        audit = m["source"]["coverage_limits"]["evidence_audit"]
        assert audit.get("text_unverified") is True, "OCR evidence must be unverified"
        assert "OCR_TEXT_UNVERIFIED" in audit["secondary_tags"]
    ranges = [entry for m in materials for entry in m["source"]["coverage_limits"]["evidence_audit"]["read_ranges"]]
    assert {(r["start"], r["end"]) for r in ranges if r.get("label") == "offline_ocr_unverified"} == {(1, 2), (2, 3)}


def direct_worker(root, binary):
    pdf = fixture(root / "work", "job.pdf", [(IMAGE, 1), (IMAGE, 2)])
    stat = pdf.stat()
    request = {"contract": "ai_daily_worker", "protocol_version": 4, "request_id": str(uuid.uuid4()),
               "operation": "pdf_parse", "payload": {"file_path": str(pdf), "file_type": ".pdf",
               "backend": "rust_pdfium_v3", "remaining_timeout_ms": 15_000, "max_file_size_bytes": 1_048_576,
               "expected_source_version": f"mtime_ns={stat.st_mtime_ns}:size={stat.st_size}",
               "parser_limits": {"kind": "pdf_image_ocr", "max_pages": 2, "excerpt_max_chars": 8192,
                                 "selected_pages": [1, 2]}}}
    # The job has no process-count limit. Windows counts even children that exit between observations.
    job = win32job.CreateJobObject(None, "")
    try:
        with subprocess.Popen([binary / "ai-daily-pdf-parser.exe", "session"], cwd=root, env=env(root),
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                              creationflags=win32con.DETACHED_PROCESS) as proc:
            handle = win32api.OpenProcess(win32con.PROCESS_SET_QUOTA | win32con.PROCESS_TERMINATE, False, proc.pid)
            try:
                win32job.AssignProcessToJobObject(job, handle)
            finally:
                handle.Close()
            try:
                raw, _ = proc.communicate((json.dumps(request) + "\n").encode(), timeout=20)
            except subprocess.TimeoutExpired:
                win32job.TerminateJobObject(job, 1)
                proc.wait()
                raise
        hello, reply = map(json.loads, raw.splitlines())
        assert hello["worker_kind"] == "pdf"
        count = win32job.QueryInformationJobObject(job, win32job.JobObjectBasicAccountingInformation)["TotalProcesses"]
        assert reply["status"] == "ok", f"typed OCR rejected: {(reply.get('error') or {}).get('error_code')}"
        assert count == 1, f"OCR started {count - 1} extra processes"
        result = reply["result"]
        units = result["evidence_audit"]["content_units"]
        for page, terms in ANCHORS.items():
            content = "".join(result["content"][u["text_start"]:u["text_end"]] for u in units
                              if any(r["start"] == page and r["label"] == "offline_ocr_unverified" for r in u["ranges"]))
            assert all(t in content for t in terms), f"page {page} lost its labelled Chinese evidence"
        return {"job_total_processes": count, "verified_page_anchors": 6, "duration_ms": result["duration_ms"]}
    finally:
        job.Close()


def main():
    if os.name != "nt":
        raise SystemExit("This acceptance test requires Windows.")
    STAGE.mkdir(parents=True, exist_ok=True)
    run = STAGE / ("run-" + uuid.uuid4().hex)
    run.mkdir()
    binary = staged_programs(run)
    (run / "programs.json").write_text(json.dumps({
        "executables": {p.name: sha(p) for p in binary.glob("*.exe")},
        "dll": sha(binary / "pdfium.dll"), "model_inputs": {p.name: sha(p) for p in (binary / "ocr-models").iterdir()},
        "fixture_sha256": sha(IMAGE), "target_date": str(DAY), "server": "loopback",
    }, indent=2), encoding="utf-8")
    server = HTTPServer(("127.0.0.1", 0), Reply)
    server.unverified = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    results = {}

    def check(name, test):
        if len(sys.argv) > 1 and name not in sys.argv[1:]:
            return
        try:
            results[name] = {"status": "passed", "evidence": test()}
        except (AssertionError, KeyError, subprocess.TimeoutExpired) as exc:
            results[name] = {"status": "failed", "reason": str(exc)[:180]}
        (run / "verification.json").write_text(json.dumps(results, indent=2), encoding="utf-8")

    root = context(run, "switch")
    fixture(root / "work", "image.pdf", [(IMAGE, 1), (IMAGE, 2)])

    def off(explicit):
        count = len(server.unverified)
        selection = daily(root, binary, server, explicit)
        assert not excerpts(selection, "image.pdf"), "disabled OCR returned image text"
        assert classification(root, "image.pdf") == ("no_text_in_parse_window", 2, 2), "full image not classified"
        return {"selected_files": selection["summary"]["selected_files"], "loopback_calls": len(server.unverified) - count}

    def on():
        selection = daily(root, binary, server, True)
        assert_ocr(excerpts(selection, "image.pdf"))
        assert classification(root, "image.pdf") == ("no_text_in_parse_window", 2, 2), "OCR changed native classification"
        assert any(g["code"] == "OCR_TEXT_UNVERIFIED" for g in selection["gaps"]), "missing report OCR gap"
        return {"physical_pages": 2, "unverified": True}

    def mixed():
        box = context(run, "exclusions")
        fixture(box / "work", "mixed.pdf", [(IMAGE, 1), (TEXT, 1)])
        fixture(box / "work", "outside-window.pdf", [(IMAGE, 1)] * 3 + [(TEXT, 1)] + [(IMAGE, 2)] * 2)
        selection = daily(box, binary, server, True)
        mixed_text = " ".join(m["text"] for m in excerpts(selection, "mixed.pdf"))
        assert "128,400" in mixed_text and "4827" not in mixed_text, "mixed document lost native routing"
        assert not excerpts(selection, "outside-window.pdf"), "sampled pages were treated as a whole image document"
        assert classification(box, "mixed.pdf")[0] == "text_in_parse_window"
        status, total, seen = classification(box, "outside-window.pdf")
        assert status == "no_text_in_parse_window" and seen < total
        return {"native_text": True, "sample_only_admitted": False}

    def quota():
        box = context(run, "quota")
        for name, pages in [("one.pdf", [1, 2]), ("two.pdf", [2, 1])]:
            fixture(box / "work", name, [(IMAGE, p) for p in pages])
        fixture(box / "work", "native.pdf", [(TEXT, 1)])
        selection = daily(box, binary, server, True, workers=2)
        assert "128,400" in "".join(m["text"] for m in excerpts(selection, "native.pdf")), "native allocation lost"
        chosen = [name for name in ("one.pdf", "two.pdf") if excerpts(selection, name)]
        assert len(chosen) == 1, f"expected exactly one OCR file under budget, saw {len(chosen)}"
        assert selection["summary"]["omissions"].get("ocr_budget") == 1
        assert any(g["code"] == "OCR_BUDGET" for g in selection["gaps"])
        assert_ocr(excerpts(selection, chosen[0]))
        return {"native_files": 1, "ocr_files": 1, "ocr_pages": 2}

    def page_quota():
        box = context(run, "page-quota")
        for p in (1, 2):
            fixture(box / "work", f"image-{p}.pdf", [(IMAGE, p)])
        selection = daily(box, binary, server, True, files=2, pages=1)
        chosen = [p for p in (1, 2) if excerpts(selection, f"image-{p}.pdf")]
        assert len(chosen) == 1, f"one global OCR page should admit one of two files, saw {len(chosen)}"
        assert selection["summary"]["omissions"].get("ocr_budget") == 1
        page, = chosen
        text = "".join(m["text"] for m in excerpts(selection, f"image-{page}.pdf"))
        assert all(anchor in text for anchor in ANCHORS[page])
        return {"file_budget": 2, "page_budget": 1, "ocr_files": len(chosen)}

    def restore_weights():
        box = context(run, "weight-recovery")
        fixture(box / "work", "image.pdf", [(IMAGE, 1), (IMAGE, 2)])
        weights = binary / "ocr-models"
        assert weights.resolve().is_relative_to(run)
        held = binary / "held-fixture-models"
        weights.rename(held)
        try:
            selection = daily(box, binary, server, True)
            assert not excerpts(selection, "image.pdf"), "missing weights produced image text"
            assert any(gap["code"] == "OCR_UNAVAILABLE" for gap in selection["gaps"]), "missing weights have no OCR diagnostic"
        finally:
            held.rename(weights)
        assert_ocr(excerpts(daily(box, binary, server, True), "image.pdf"))
        return {"same_data_and_file": True, "recovered": True}

    def published():
        box = context(run, "daily")
        fixture(box / "work", "image.pdf", [(IMAGE, 1), (IMAGE, 2)])
        reply = daily(box, binary, server, True, dry=False, user_note="扫描件的金额尚未核对。")
        assert server.unverified[-1], "loopback model did not receive unverified evidence"
        with sqlite3.connect((box / "db/reports.sqlite3").as_uri() + "?mode=ro", uri=True) as db:
            row, = db.execute("SELECT raw_json FROM daily_reports WHERE date=?", (str(DAY),)).fetchall()
        report = json.loads(row[0])
        assert any(s["coverage_limits"]["evidence_audit"].get("text_unverified") is True for s in report["sources"])
        summary = Path(reply["output_path"]).resolve(strict=True)
        assert summary.is_relative_to(box)
        attachment = report["evidence_attachment"]
        relative = f"attachments/{DAY}.{attachment['sha256']}.md"
        markdown = summary.read_text(encoding="utf-8")
        assert relative in markdown
        assert report["items"] and all(item["statement_kind"] == "uncertain" for item in report["items"])
        overview = markdown.split("## 今日概览", 1)[1].split("## 事项详述", 1)[0]
        assert "4827" in overview and any(label in overview for label in ("待确认", "待核实"))
        assert "待确认" in markdown.split("## 事项详述", 1)[1]
        published = summary.parent / relative
        assert sha(published) == attachment["sha256"]
        text = published.read_bytes().decode("utf-8")
        assert text == attachment["markdown"] and "文字待核实" in text
        for page, words in ANCHORS.items():
            assert f"第 {page} 页" in text and all(word in text for word in words)
        return {"local_stub_calls": 1, "published_unverified": True, "labelled_pages": len(ANCHORS),
                "attachment_hash_matches": True, "mixed_user_ocr_claim_marked_uncertain": True}

    def native_publication():
        box = context(run, "native-control")
        fixture(box / "work", "native.pdf", [(TEXT, 1)])
        value = daily(box, binary, server, dry=False)
        path = Path(value["output_path"]).resolve(strict=True)
        assert path.is_relative_to(box) and server.unverified[-1] == []
        return {"ordinary_daily": True, "local_stub_calls": 1, "verified_text_source": True}

    try:
        check("worker_process_tree", lambda: direct_worker(context(run, "worker"), binary))
        check("native_daily_control", native_publication)
        check("default_off", lambda: off(None))
        check("explicit_off", lambda: off(False))
        check("enabled_after_off", on)
        if results.get("enabled_after_off", {}).get("status") == "passed":
            check("disabled_after_enabled", lambda: off(False))
        elif len(sys.argv) == 1 or "disabled_after_enabled" in sys.argv[1:]:
            results["disabled_after_enabled"] = {"status": "not_run", "reason": "positive OCR cache unavailable"}
        check("mixed_and_sampled", mixed)
        check("independent_global_quota", quota)
        check("page_budget_with_free_file_slots", page_quota)
        check("missing_weights_restored", restore_weights)
        check("daily_local_stub", published)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    print(run / "verification.json")
    print(json.dumps({k: v["status"] for k, v in results.items()}))
    if set(sys.argv[1:]) - results.keys():
        raise SystemExit("unknown acceptance case")
    return int(any(v["status"] != "passed" for v in results.values()))


if __name__ == "__main__":
    raise SystemExit(main())
