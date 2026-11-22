"""Exercise the ordinary Windows daily CLI using synthetic PDFs and a loopback model stub."""

import argparse
import hashlib
import io
import json
import os
import re
import shutil
import sqlite3
import struct
import subprocess
import threading
import uuid
from datetime import date, datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import win32api
import win32con
import win32job
from pypdf import PdfReader, PdfWriter
from pypdf.generic import DecodedStreamObject, DictionaryObject, NameObject


REPO = Path(__file__).resolve().parents[1]
STAGE = REPO / ".artifacts/kernel-v2-20261122/ocr-acceptance"
IMAGE = REPO / "rust/pdf_worker/tests/fixtures/ocr-image-pages.pdf"
ZONE = timezone(timedelta(hours=8))
DAY = date(2026, 11, 20)
ANCHORS = {1: ("4827", "7359", "金额"), 2: ("1936", "2064", "验收")}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def staged_programs(run, name, bundle, *, ocr):
    bundle = bundle.resolve(strict=True)
    dest = run / name
    dest.mkdir()
    pin = json.loads((REPO / "rust/build-inputs.json").read_text(encoding="utf-8"))
    for filename in ("ai-daily-report.exe", "ai-daily-pdf-worker.exe", "pdfium.dll"):
        shutil.copyfile(bundle / filename, dest / filename)
        assert sha(dest / filename) == sha(bundle / filename)
    assert sha(dest / "pdfium.dll") == pin["pdfium"]["dll_sha256"]
    assert (bundle / "ocr-models").is_dir() == ocr
    if ocr:
        shutil.copytree(bundle / "ocr-models", dest / "ocr-models")
        for entry in pin["image_ocr"]["files"]:
            model = dest / "ocr-models" / entry["name"]
            assert model.stat().st_size == entry["size"] and sha(model) == entry["sha256"]
    shutil.copytree(bundle / "templates", dest / "templates")
    app = build_info(dest, "ai-daily-report.exe", "build-info")
    worker = build_info(dest, "ai-daily-pdf-worker.exe", "--build-info")
    for field in ("parser_content_identity", "protocol_version"):
        assert app[field] == worker[field], f"{name} has unmatched {field}"
    assert worker["ocr_enabled"] is ocr
    inputs = {path.relative_to(dest).as_posix(): sha(path)
              for path in sorted(dest.rglob("*")) if path.is_file()}
    return dest, {"app": app, "worker": worker, "file_sha256": inputs}


def build_info(binary, filename, argument):
    result = subprocess.run([binary / filename, argument], cwd=binary, env=env(binary),
                            capture_output=True, check=True, timeout=20,
                            creationflags=subprocess.CREATE_NO_WINDOW)
    return json.loads(result.stdout)


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
        if source is not None:
            writer.add_page(PdfReader(source).pages[number - 1])
            continue
        page = writer.add_blank_page(width=612, height=792)
        font = DictionaryObject({
            NameObject("/Type"): NameObject("/Font"),
            NameObject("/Subtype"): NameObject("/Type1"),
            NameObject("/BaseFont"): NameObject("/Helvetica"),
        })
        page[NameObject("/Resources")] = DictionaryObject({
            NameObject("/Font"): DictionaryObject({NameObject("/F1"): font}),
        })
        stream = DecodedStreamObject()
        stream.set_data(b"BT /F1 12 Tf 72 700 Td (Synthetic revenue 128,400 USD) Tj ET")
        page.replace_contents(stream)
    writer.write(destination)
    noon = datetime.combine(DAY, datetime.min.time(), tzinfo=ZONE) + timedelta(hours=12)
    os.utime(destination, (noon.timestamp(), noon.timestamp()))
    return destination


def context(run, name):
    root = run / name
    for sub in ("config", "work", "data", "db", "reports", "logs"):
        (root / sub).mkdir(parents=True)
    return root


def daily(root, binary, server, ocr=None, *, dry=True, workers=1, files=1, pages=2,
          user_note=None, error_code=None):
    scanner = {
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
    if error_code is not None:
        assert result.returncode and error_code.encode() in result.stdout + result.stderr
        assert len(server.unverified) == before
        return
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


def cache_evidence(root):
    path = root / "db/scan_cache_v4.sqlite3"
    assert path.is_file(), "isolated v4 scan cache missing"
    assert not list(root.rglob("scan_index_rust_v3.sqlite3"))
    with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as db:
        tables = {r[1]: (r[4], r[5]) for r in db.execute("PRAGMA table_list")
                  if r[0] == "main" and not r[1].startswith("sqlite_")}
        assert tables == dict.fromkeys(("parse_cache", "pdf_negative_cache", "scan_snapshots"), (1, 1))
        assert db.execute("PRAGMA quick_check").fetchone() == ("ok",)
        evidence = {}
        snapshots = db.execute("SELECT manifest,refs FROM scan_snapshots").fetchall()
        assert snapshots, "scanner wrote no complete snapshot"
        for manifest, refs in snapshots:
            bindings = {(ref["reference"]["kind"], tuple(ref["reference"]["key"][k] for k in
                         ("file_id", "source_guard", "profile_hash"))): ref["record_sha256"]
                        for ref in json.loads(refs)}
            for name, _status, reason, key, _pdf_budget in json.loads(manifest[8:])[-1]:
                if key is None:
                    continue
                kind = "negative" if reason in ("pdf_window_no_text", "pdf_damaged", "pdf_protected") else "parse"
                values = tuple(key[k] for k in ("file_id", "source_guard", "profile_hash"))
                table = "pdf_negative_cache" if kind == "negative" else "parse_cache"
                record = db.execute(
                    f"SELECT payload, record_sha256 FROM {table} "
                    "WHERE file_id=? AND source_guard=? AND profile_hash=?", values).fetchone()
                assert record and record[1] == bindings[kind, values], f"missing snapshot record for {name}"
                payload = json.loads(record[0])
                observation = (payload["evidence"].get("no_text") if kind == "negative"
                               else payload["observation"])
                if observation:
                    evidence.setdefault(name, []).append((kind, payload["window"], observation))
        return evidence


def classification(root, name):
    entries = cache_evidence(root)[name]
    native = {(("text_in_parse_window" if item["pages"]["observations"][-1]["has_text"]
                else "no_text_in_parse_window"), item["page_count"],
               len(item["pages"]["checked_pages"]))
              for _, window, item in entries if window["selected_pages"] is None}
    assert len(native) == 1, f"ambiguous native PDF classification for {name}"
    return native.pop()


def assert_ocr(materials):
    assert materials, "fully observed image-only PDF was omitted"
    text = "\n".join(m["text"] for m in materials)
    for page, terms in ANCHORS.items():
        assert all(term in text for term in terms), f"missing synthetic anchors for physical page {page}"
    for m in materials:
        audit = m["source"]["coverage_limits"]["evidence_audit"]
        assert audit.get("text_unverified") is True, "OCR evidence must be unverified"
    ranges = [entry for m in materials for entry in m["source"]["coverage_limits"]["evidence_audit"]["read_ranges"]]
    assert {(r["start"], r["end"]) for r in ranges if r.get("label") == "offline_ocr_unverified"} == {(1, 2), (2, 3)}


def frame(version, id, command):
    data = json.dumps({"version": version, "id": id, "command": command}).encode("utf-8")
    return struct.pack("<I", len(data)) + data


def read_frame(stream, version, id):
    prefix = stream.read(4)
    assert len(prefix) == 4, "missing worker frame"
    size, = struct.unpack("<I", prefix)
    assert 0 < size <= 2 * 1024 * 1024
    data = stream.read(size)
    assert len(data) == size, "incomplete worker frame"
    reply = json.loads(data)
    assert reply["version"] == version and reply["id"] == id
    return reply["reply"]


def direct_worker(root, binary, build):
    pdf = fixture(root / "work", "job.pdf", [(IMAGE, 1), (IMAGE, 2)])
    stat = pdf.stat()
    version = build["protocol_version"]
    request = {"inspect_and_extract": {
        "parse": {"file_path": str(pdf), "file_type": ".pdf", "backend": "rust_pdfium_v3",
                  "remaining_timeout_ms": 15_000, "max_file_size_bytes": 1_048_576,
                  "expected_source_version": f"mtime_ns={stat.st_mtime_ns}:size={stat.st_size}",
                  "parser_limits": {"kind": "pdf_image_ocr", "max_pages": 2,
                                    "excerpt_max_chars": 8192, "selected_pages": [1, 2]}},
        "inspect_max_pages": 2, "policy_version": "pdf_text_presence_v2", "extract": True,
        "expected_source_guard": {"kind": "content_sha256_v1", "sha256": sha(pdf)},
    }}
    job = win32job.CreateJobObject(None, "")
    try:
        with subprocess.Popen([binary / "ai-daily-pdf-worker.exe", "--stdio"], cwd=root, env=env(root),
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                              creationflags=win32con.DETACHED_PROCESS) as proc:
            handle = win32api.OpenProcess(win32con.PROCESS_SET_QUOTA | win32con.PROCESS_TERMINATE, False, proc.pid)
            try:
                win32job.AssignProcessToJobObject(job, handle)
            finally:
                handle.Close()
            try:
                raw, _ = proc.communicate(frame(version, 0, "hello") + frame(version, 1, request), timeout=20)
            except subprocess.TimeoutExpired:
                win32job.TerminateJobObject(job, 1)
                proc.wait()
                raise
            assert proc.returncode == 0
            pid = proc.pid
        stream = io.BytesIO(raw)
        hello = read_frame(stream, version, 0)["hello"]
        assert hello == {"pid": pid, "parser_identity": build["parser_content_identity"],
                         "pdfium_sha256": sha(binary / "pdfium.dll"), "ocr": True}
        reply = read_frame(stream, version, 1)["inspect_and_extract"]
        assert not stream.read(), "unexpected extra worker business reply"
        count = win32job.QueryInformationJobObject(job, win32job.JobObjectBasicAccountingInformation)["TotalProcesses"]
        assert count == 1, f"OCR started {count - 1} extra processes"
        assert (reply["native_document_opens"], reply["native_page_reads"]) == (1, 2)
        assert reply["classification"]["status"] == "no_text_in_parse_window"
        result = reply["extraction"]["Ok"]
        units = result["evidence_audit"]["content_units"]
        for page, terms in ANCHORS.items():
            content = "".join(result["content"][u["text_start"]:u["text_end"]] for u in units
                              if any(r["start"] == page and r["label"] == "offline_ocr_unverified" for r in u["ranges"]))
            assert all(t in content for t in terms), f"page {page} lost its labelled evidence"
        return {"job_total_processes": count, "business_replies": 1, "native_opens": 1,
                "native_page_reads": 2, "verified_page_anchors": 6, "duration_ms": result["duration_ms"]}
    finally:
        job.Close()


def main():
    if os.name != "nt":
        raise SystemExit("This acceptance test requires Windows.")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ocr-bundle", type=Path, default=os.environ.get("AI_DAILY_TEST_BUNDLE"),
                        required=not os.environ.get("AI_DAILY_TEST_BUNDLE"))
    parser.add_argument("--default-bundle", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=os.environ.get("AI_DAILY_ACCEPTANCE_OUTPUT", STAGE))
    parser.add_argument("cases", nargs="*")
    args = parser.parse_args()
    run = args.output.resolve() / ("run-" + uuid.uuid4().hex)
    run.mkdir(parents=True)
    binary, ocr_build = staged_programs(run, "ocr-bin", args.ocr_bundle, ocr=True)
    default_binary, default_build = staged_programs(run, "default-bin", args.default_bundle, ocr=False)
    (run / "programs.json").write_text(json.dumps({
        "ocr": ocr_build, "default": default_build, "fixture_sha256": sha(IMAGE),
        "target_date": str(DAY), "model_server": "loopback",
    }, indent=2), encoding="utf-8")
    server = HTTPServer(("127.0.0.1", 0), Reply)
    server.unverified = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    results = {}

    def check(name, test):
        if args.cases and name not in args.cases:
            return
        try:
            results[name] = {"status": "passed", "evidence": test()}
        except (AssertionError, KeyError, subprocess.TimeoutExpired) as exc:
            results[name] = {"status": "failed", "reason": str(exc)[:180]}
        (run / "verification.json").write_text(json.dumps(results, indent=2), encoding="utf-8")

    root = context(run, "switch")
    fixture(root / "work", "image.pdf", [(IMAGE, 1), (IMAGE, 2)])

    def off(explicit, *, program=binary, data=root):
        count = len(server.unverified)
        selection = daily(data, program, server, explicit)
        assert not excerpts(selection, "image.pdf"), "disabled OCR returned image text"
        assert classification(data, "image.pdf") == ("no_text_in_parse_window", 2, 2), "full image not classified"
        return {"selected_files": selection["summary"]["selected_files"], "loopback_calls": len(server.unverified) - count}

    def default_package():
        box = context(run, "default-package")
        fixture(box / "work", "image.pdf", [(IMAGE, 1), (IMAGE, 2)])
        result = off(None, program=default_binary, data=box)
        daily(box, default_binary, server, True, error_code="SCANNER_UNAVAILABLE")
        assert all(kind == "negative" for kind, _, _ in cache_evidence(box)["image.pdf"])
        return result | {"explicit_enable_rejected": True}

    def on():
        selection = daily(root, binary, server, True)
        assert_ocr(excerpts(selection, "image.pdf"))
        assert classification(root, "image.pdf") == ("no_text_in_parse_window", 2, 2), "OCR changed native classification"
        assert any(g["code"] == "OCR_TEXT_UNVERIFIED" for g in selection["gaps"]), "missing report OCR gap"
        cached = cache_evidence(root)["image.pdf"]
        assert any(kind == "parse" and window["selected_pages"] == [1, 2]
                   for kind, window, _ in cached), "missing OCR parse row"
        return {"physical_pages": 2, "unverified": True, "ocr_cache_bound_to_snapshot": True}

    def mixed():
        box = context(run, "exclusions")
        fixture(box / "work", "mixed.pdf", [(IMAGE, 1), (None, 1)])
        fixture(box / "work", "outside-window.pdf", [(IMAGE, 1)] * 3 + [(None, 1)] + [(IMAGE, 2)] * 2)
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
        fixture(box / "work", "native.pdf", [(None, 1)])
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
        body = markdown.split("[完整依据](", 1)[0]
        assert "4827" in body and "待确认" in body
        assert re.search(r"\[S\d+\]\(" + re.escape(relative) + r"#s\d+\)", body)
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
        fixture(box / "work", "native.pdf", [(None, 1)])
        value = daily(box, binary, server, dry=False)
        path = Path(value["output_path"]).resolve(strict=True)
        assert path.is_relative_to(box) and server.unverified[-1] == []
        return {"ordinary_daily": True, "local_stub_calls": 1, "verified_text_source": True}

    try:
        check("worker_process_tree", lambda: direct_worker(context(run, "worker"), binary, ocr_build["worker"]))
        check("default_package", default_package)
        check("native_daily_control", native_publication)
        check("default_off", lambda: off(None))
        check("explicit_off", lambda: off(False))
        check("enabled_after_off", on)
        if results.get("enabled_after_off", {}).get("status") == "passed":
            check("disabled_after_enabled", lambda: off(False))
        elif not args.cases or "disabled_after_enabled" in args.cases:
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
    if set(args.cases) - results.keys():
        raise SystemExit("unknown acceptance case")
    return int(any(v["status"] != "passed" for v in results.values()))


if __name__ == "__main__":
    raise SystemExit(main())
