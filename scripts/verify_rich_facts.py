"""Compare a Windows bundle on fixed, synthetic, source-located report facts."""
from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import sqlite3
import subprocess
from threading import Thread
from time import perf_counter


DAY = "2026-11-10"
# All terms of a fact must survive together. Supplemental evidence intentionally
# receives no model citation: it exercises publication independently of the model.
FACTS = {
    "csv_late_record": ("records.csv", ["-170.50", "待复核"]),
    "sheet_six_right_column": ("settlement.xlsx", ["498765.43", "实付"]),
    "budget_caption": ("scopes.docx", ["预算", "万元", "12"]),
    "paid_caption": ("scopes.docx", ["实付", "元", "12"]),
    "slide_chart": ("delivery.pptx", ["实收", "第三期", "7319"]),
    "slide_notes": ("delivery.pptx", ["批准前复核数量"]),
    "pdf_condition": ("pages.pdf", ["Effective only after review"]),
    "pdf_settlement": ("pages.pdf", ["Final settlement USD 4817.25"]),
    "supplement": ("retention.md", ["样品留存三十日"]),
}

# The localhost responder emits a readable example only for facts it can find
# in an actual request block. The supplemental fact deliberately stays uncited.
DETAILS = {
    "csv_late_record": ("实付差额待复核", "第 20 号记录的实付差额为 -170.50 元，状态为待复核。", "attention"),
    "sheet_six_right_column": ("结算表已列出实付合计", "结算汇总表第 508 行的实付合计为 498765.43 元。", "progress"),
    "budget_caption": ("预算表以万元计量", "预算表将本期金额列为 12 万元。", "progress"),
    "paid_caption": ("实付表以元计量", "实付表将本期金额列为 12 元。", "progress"),
    "slide_chart": ("交付图表列出第三期实收", "第三期的“实收”系列数值为 7319。", "progress"),
    "slide_notes": ("批准前需复核数量", "演示文稿备注要求批准前复核数量。", "next_step"),
    "pdf_condition": ("文件约定复核后生效", "第 5 页写明“Effective only after review”，即复核后方可生效。", "attention"),
    "pdf_settlement": ("末页列出最终美元结算额", "第 8 页写明“Final settlement USD 4817.25”，最终结算额为 4817.25 美元。", "progress"),
}


def file_hash(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def make_pdf(path: Path) -> None:
    from pypdf import PdfWriter
    from pypdf.generic import DecodedStreamObject, DictionaryObject, NameObject

    writer = PdfWriter()
    name = NameObject
    font = writer._add_object(DictionaryObject({
        name("/Type"): name("/Font"), name("/Subtype"): name("/Type1"),
        name("/BaseFont"): name("/Helvetica"),
    }))
    for page in range(1, 9):
        sheet = writer.add_blank_page(width=595, height=842)
        sheet[name("/Resources")] = DictionaryObject({name("/Font"): DictionaryObject({name("/F1"): font})})
        lines = {1: [f"Section {i:02} reviewed transfer records with complete inventory details." for i in range(75)],
                 5: ["Effective only after review"], 8: ["Final settlement USD 4817.25"]}.get(page, [f"Section {page}"])
        commands = ["BT /F1 8 Tf 10 TL 20 818 Td"]
        commands += [f"({line}) Tj T*" for line in lines]
        commands.append("ET")
        stream = DecodedStreamObject()
        stream.set_data("\n".join(commands).encode("ascii"))
        sheet[name("/Contents")] = writer._add_object(stream)
    writer.write(path)


def create_corpus(work: Path) -> dict:
    from docx import Document
    from openpyxl import Workbook
    from pptx import Presentation
    from pptx.chart.data import CategoryChartData
    from pptx.enum.chart import XL_CHART_TYPE
    from pptx.util import Inches

    manifest = work.parent / "corpus-hashes.json"
    if manifest.exists():
        hashes = json.loads(manifest.read_text(encoding="utf-8"))
        if hashes != {p.name: file_hash(p) for p in work.iterdir()}:
            raise ValueError("The frozen corpus changed")
        return hashes
    work.mkdir(parents=True, exist_ok=False)
    with (work / "records.csv").open("w", encoding="gb18030", newline="") as stream:
        rows = csv.writer(stream)
        rows.writerow(["编号", "实付差额（元）", "状态"])
        for i in range(1, 20):
            rows.writerow([i, "0.00", "已登记"])
        rows.writerow([20, "-170.50", "待复核\n次日核对"])
    book = Workbook()
    for i in range(5):
        sheet = book.active if i == 0 else book.create_sheet()
        sheet.title = f"基础明细{i + 1}"
        sheet.append(["名称", "数量"])
        sheet.append(["库存", 2])
    sheet = book.create_sheet("结算汇总")
    sheet["A1"], sheet["V1"], sheet["W1"] = "项目", "预算额（元）", "实付额（元）"
    for i in range(2, 508):
        sheet.cell(i, 1, f"库存{i}")
        sheet.cell(i, 22, 100)
        sheet.cell(i, 23, 90)
    sheet["A508"], sheet["V508"], sheet["W508"] = "合计", 512345.67, 498765.43
    book.save(work / "settlement.xlsx")
    word = Document()
    for caption in ["表1 预算（万元）", "表2 实付（元）"]:
        word.add_paragraph(caption, style="Caption")
        table = word.add_table(rows=2, cols=2)
        for row, fields in zip(table.rows, [["项目", "金额"], ["本期", "12"]]):
            for cell, value in zip(row.cells, fields):
                cell.text = value
    word.save(work / "scopes.docx")
    slides = Presentation()
    slide = slides.slides.add_slide(slides.slide_layouts[6])
    chart = CategoryChartData()
    chart.categories = ["第三期"]
    chart.add_series("实收", (7319,))
    slide.shapes.add_chart(XL_CHART_TYPE.COLUMN_CLUSTERED, Inches(1), Inches(1), Inches(6), Inches(3), chart)
    group = slide.shapes.add_group_shape()
    group.shapes.add_textbox(Inches(1), Inches(5), Inches(4), Inches(1)).text = "交付台账"
    slide.notes_slide.notes_text_frame.text = "批准前复核数量"
    slides.save(work / "delivery.pptx")
    make_pdf(work / "pages.pdf")
    (work / "retention.md").write_text("样品留存三十日。", encoding="utf-8")
    stamp = datetime.fromisoformat(DAY).replace(hour=12, tzinfo=timezone(timedelta(hours=8))).timestamp()
    for item in work.iterdir():
        os.utime(item, (stamp, stamp))
    hashes = {p.name: file_hash(p) for p in work.iterdir()}
    manifest.write_text(json.dumps(hashes, indent=2), encoding="utf-8")
    return hashes


def includes(text: str, terms: list[str]) -> bool:
    return all(term in text for term in terms)


def located(key: str, ranges: list[dict]) -> bool:
    expected = {"csv_late_record": ("csv_record", 21), "sheet_six_right_column": ("sheet_row", 508),
                "budget_caption": ("table_row", 2), "paid_caption": ("table_row", 2),
                "supplement": ("text_char", 0),
                "pdf_condition": ("pdf_page", 5), "pdf_settlement": ("pdf_page", 8),
                "slide_chart": ("slide", 1), "slide_notes": ("slide", 1)}
    unit, point = expected[key]
    for source in ranges:
        if source["unit"] != unit or not source["start"] <= point < source["end"]:
            continue
        if key == "sheet_six_right_column":
            if "结算汇总" not in (source.get("label") or "") or source.get("column_start") != 23 or source.get("column_end") != 24:
                continue
        if key in ("budget_caption", "paid_caption"):
            label = "Table 1" if key == "budget_caption" else "Table 2"
            if source.get("label") != label or not (source.get("column_start") or 0) <= 2 < (source.get("column_end") or 0):
                continue
        return True
    return False


def exercise(bundle: Path, work: Path, output: Path, require: bool, markitdown_python: Path | None) -> dict:
    output.mkdir(parents=True, exist_ok=False)
    calls = []
    request_bytes = []
    response_content_bytes = []

    class Model(BaseHTTPRequestHandler):
        def do_POST(self):
            raw_request = self.rfile.read(int(self.headers["Content-Length"]))
            request_bytes.append(len(raw_request))
            request = json.loads(raw_request)
            sources = json.loads(request["messages"][1]["content"])["sources"]
            calls.append(sources)
            detailed = "事项详述" in request["messages"][0]["content"] or "详述" in request["messages"][0]["content"]
            items = []
            for key, (filename, terms) in FACTS.items():
                if key == "supplement":
                    continue
                matching = next((s for s in sources if s["path"] == filename
                                 and includes(s["text"] + json.dumps(s.get("context", []), ensure_ascii=False), terms)), None)
                if matching:
                    title, text, kind = DETAILS[key]
                    items.append({"title": title, "text": text, "kind": kind,
                                  "statement_kind": "observed", "source_ids": [matching["source_id"]]})
            content = json.dumps({"schema_version": 5, "items": items if detailed else items[:5]}, ensure_ascii=False)
            response_content_bytes.append(len(content.encode("utf-8")))
            body = json.dumps({"choices": [{"finish_reason": "stop", "message": {"content": content}}]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_):
            pass

    with HTTPServer(("127.0.0.1", 0), Model) as server:
        thread = Thread(target=server.serve_forever, daemon=True)
        thread.start()
        paths = {key: str(output / folder) for key, folder in
                 [("data_dir", "data"), ("db_dir", "db"), ("reports_dir", "reports"), ("log_dir", "logs")]}
        settings = {"paths": dict(paths, work_dir=str(work)), "archive": {"allow_work_dir_for_model": True},
                    "llm": {"base_url": f"http://127.0.0.1:{server.server_port}", "model_id": "local-rich-facts",
                            "max_tokens": 4096, "context_window_tokens": 256000, "verified_max_output_tokens": 8192,
                            "max_request_utf8_bytes": 256000},
                    "scanner": {"csv_encodings": ["utf-8", "utf-8-sig", "gb18030"], "excel_max_rows": 500,
                                "excel_max_sheets": 5, "excel_max_columns": 20, "pdf_max_pages": 5,
                                "document_excerpt_max_chars": 3200, "pptx_include_notes": True}}
        config = output / "config"
        config.mkdir()
        (config / "settings.yaml").write_text(json.dumps(settings, ensure_ascii=False), encoding="utf-8")
        env = {key: os.environ[key] for key in ("SystemRoot", "WINDIR", "TEMP", "TMP", "PATHEXT") if key in os.environ}
        env["PATH"] = str(Path(env["SystemRoot"]) / "System32")
        env.update(DEEPSEEK_API_KEY="synthetic-local", NO_PROXY="localhost,127.0.0.1")
        started = perf_counter()
        try:
            run = subprocess.run([str(bundle / "ai-daily-report.exe"), "--config-dir", str(config),
                                  "daily", "--target-date", DAY], env=env, capture_output=True, timeout=360)
            elapsed = perf_counter() - started
        finally:
            server.shutdown()
            thread.join()
        if run.returncode:
            raise RuntimeError(run.stderr.decode("utf-8", "replace"))
    outcome = json.loads(run.stdout)
    with sqlite3.connect(f"file:{(output / 'db/reports.sqlite3').as_posix()}?mode=ro", uri=True) as db:
        report = json.loads(db.execute("SELECT raw_json FROM daily_reports WHERE date=?", (DAY,)).fetchone()[0])
        snapshot = db.execute("SELECT snapshot_path FROM report_runs WHERE run_id=?", (outcome["run_id"],)).fetchone()[0]
    frozen = json.loads((output / "data/daily_cache/snapshots" / snapshot).read_bytes())["selection"]["materials"]
    index = next(path for path in (output / "db").glob("*.sqlite3") if "scan" in path.name)
    with sqlite3.connect(f"file:{index.as_posix()}?mode=ro", uri=True) as db:
        parsed = {Path(key).name.lower(): json.loads(content)["text"] for key, content in db.execute(
            "SELECT file_identity,content FROM parse_cache")}
    results = {}
    locations = {}
    cited = {source for item in report["items"] for source in item["source_ids"]}
    for key, (filename, terms) in FACTS.items():
        def match_blocks(blocks):
            return any(includes(block, terms) for block in blocks)
        matching_evidence = []
        for source in report["sources"]:
            if source.get("relative_path") != filename:
                continue
            refs = report["evidence"].get(source["source_id"], [])
            context = source["coverage_limits"].get("personal", {}).get("context", [])
            quote = "\n".join([part["quote"] for part in refs] + [part["text"] for part in context])
            if includes(quote, terms):
                matching_evidence.append((source["source_id"], [r for part in refs for r in part["ranges"]]))
        locations[key] = any(located(key, ranges) for _, ranges in matching_evidence)
        results[key] = {
            "parsed": match_blocks(text for name, text in parsed.items() if name.endswith(filename)),
            "frozen": match_blocks(m["text"] + json.dumps(m["source"]["coverage_limits"].get("personal", {}).get("context", []), ensure_ascii=False)
                                   for m in frozen if m["source"]["relative_path"] == filename),
            "sent": match_blocks(s["text"] + json.dumps(s.get("context", []), ensure_ascii=False)
                                 for call in calls for s in call if s["path"] == filename),
            "detail": any(includes(item["text"], terms) for item in report["items"]),
            "attachment": bool(matching_evidence) and includes(report["evidence_attachment"]["markdown"], terms),
        }
        if key == "supplement" and any(source in cited for source, _ in matching_evidence):
            raise ValueError("The local fixture unexpectedly cited supplemental evidence")
    result = {"schema": report["schema_version"], "seconds": round(elapsed, 3),
              "program_sha256": file_hash(bundle / "ai-daily-report.exe"),
              "corpus_manifest_sha256": file_hash(work.parent / "corpus-hashes.json"),
              "request_utf8_bytes": sum(request_bytes), "fixture_response_utf8_bytes": sum(response_content_bytes),
              "body_bytes": len(outcome["markdown"].encode("utf-8")),
              "attachment_bytes": len(report["evidence_attachment"]["markdown"].encode("utf-8")),
              "local_requests": len(calls), "remote_requests": 0, "facts": results, "located_evidence": locations,
              "facts_per_stage": {stage: sum(f[stage] for f in results.values()) for stage in next(iter(results.values()))}}
    if markitdown_python:
        # Use an isolated test environment containing markitdown[pptx].
        probe = """from importlib.metadata import version
from markitdown import MarkItDown
import json, sys
text = MarkItDown(enable_plugins=False).convert(sys.argv[1]).text_content
print(json.dumps({"version": version("markitdown"), "markdown": text}))
"""
        converted = json.loads(subprocess.check_output(
            [markitdown_python, "-I", "-B", "-c", probe, work / "delivery.pptx"], env=env, timeout=90))
        expected = ["第三期", "实收", "7319", "批准前复核数量", "交付台账"]
        result["markitdown"] = {"version": converted["version"],
                                "values": {term: term in converted["markdown"] for term in expected}}
        (output / "upstream-presentation.md").write_text(converted["markdown"], encoding="utf-8")
        if not all(result["markitdown"]["values"].values()):
            raise ValueError("PPT facts differ from the upstream converter")
    (output / "facts.json").write_text(json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8")
    if require:
        failures = [(key, stage) for key, row in results.items() for stage, hit in row.items()
                    if hit != (key != "supplement" or stage != "detail")]
        failures.extend((key, "location") for key, correct in locations.items() if not correct)
        if failures or len(calls) != 1:
            raise ValueError(f"fact mismatches: {failures}; local requests: {len(calls)}")
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--require", action="store_true")
    parser.add_argument("--markitdown-python", type=Path, help="Isolated interpreter with markitdown[pptx]")
    args = parser.parse_args()
    create_corpus(args.corpus.resolve())
    print(json.dumps(exercise(args.bundle.resolve(), args.corpus.resolve(), args.output.resolve(),
                              args.require, args.markitdown_python),
                     ensure_ascii=True))
