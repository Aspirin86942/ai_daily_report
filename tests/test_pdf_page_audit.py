from hashlib import sha256
from pathlib import Path
import sys
import threading
import time

import pytest
from reportlab.pdfgen import canvas

from src.services.document_parser import parse_document_file
from src.workers.pdf_classifier import classify_pdf
from src.workers.pdf_pages import sample_pages
from src.workers.ocr import OcrCoordinator, OcrError, OcrLimits, ProcessOcrWorker


def pdf_file(tmp_path, count=8, text_page=None):
    path = tmp_path / "sample.pdf"
    pdf = canvas.Canvas(str(path), invariant=1)
    for page in range(1, count + 1):
        if page == text_page:
            pdf.drawString(20, 50, "Planned acceptance 2026-09-22; subtotal CNY 1234.56")
        pdf.showPage()
    pdf.save()
    return path


def test_later_page_probe_and_explicit_body_read_stay_within_five_pages(tmp_path):
    path = pdf_file(tmp_path, text_page=8)
    old = classify_pdf(str(path), 5, policy_version="pdf_text_presence_v1")
    new = classify_pdf(str(path), 5)
    assert old["status"] == "no_text_in_parse_window"
    assert new["status"] == "text_in_parse_window"
    assert new["result_examined_pages"] == 3
    assert new["page_audit"]["checked_pages"] == [1, 2, 8]
    assert [page["has_text"] for page in new["page_audit"]["observations"]] == [False, False, True]
    pages = sample_pages(8, 5)
    parsed = parse_document_file(path, ".pdf", {"pdf_max_pages": 5, "pdf_selected_pages": pages})
    assert not parsed.error and "1234.56" in parsed.content and "Planned" in parsed.content
    assert [range_.start for range_ in parsed.evidence_audit.read_ranges] == pages
    assert len(pages) == 5 and parsed.truncated
    assert {range_.start for unit in parsed.evidence_audit.content_units
        if unit.kind == "paragraph" for range_ in unit.ranges} == {8}


def test_toc_leaders_preserve_counts_and_actual_page_under_character_budget(tmp_path):
    path = tmp_path / "toc.pdf"
    pdf = canvas.Canvas(str(path), invariant=1)
    pdf.drawString(20, 100, "Investments(17)" + "." * 100 + "3")
    pdf.drawString(20, 80, "Partners(7)" + "." * 100 + "4")
    pdf.drawString(20, 60, "Ordinary value 1234.56; literal ... remains")
    pdf.save()
    parsed = parse_document_file(path, ".pdf", {"pdf_max_pages": 1,
        "document_excerpt_max_chars": 180})
    assert parsed.error is None
    assert "Investments(17) 3" in parsed.content and "Partners(7) 4" in parsed.content
    assert "1234.56; literal ... remains" in parsed.content
    assert len(parsed.content) <= 180
    assert [(r.start, r.end) for r in parsed.evidence_audit.read_ranges] == [(1, 2)]
    assert all(r.start == 1 for u in parsed.evidence_audit.content_units for r in u.ranges)


def test_observed_blank_and_raster_image_are_distinct(tmp_path):
    from PIL import Image
    from reportlab.lib.utils import ImageReader
    blank = pdf_file(tmp_path, count=1)
    assert classify_pdf(str(blank), 5)["page_audit"]["observations"] == [
        {"page": 1, "has_text": False, "has_image": False}]
    image_path = tmp_path / "image.pdf"
    pdf = canvas.Canvas(str(image_path), invariant=1)
    pdf.drawImage(ImageReader(Image.new("RGB", (40, 40), "white")), 10, 10)
    pdf.showPage()
    pdf.save()
    observed = classify_pdf(str(image_path), 5)
    assert observed["status"] == "no_text_in_parse_window"
    assert observed["page_audit"]["observations"][0]["has_image"] is True


@pytest.mark.parametrize("pages", [[9], [1, 1], [1, 2, 3, 4, 5, 6]])
def test_invalid_body_page_selection_is_controlled(tmp_path, pages):
    parsed = parse_document_file(pdf_file(tmp_path, text_page=8), ".pdf",
        {"pdf_max_pages": 5, "pdf_selected_pages": pages})
    assert parsed.error and not parsed.content and parsed.evidence_audit is None


def test_pdf_page_plan_is_deterministic_unique_and_bounded():
    for count in range(1, 35):
        for limit in range(1, 12):
            pages = sample_pages(count, limit)
            assert len(pages) == min(count, limit) == len(set(pages))
            assert pages[0] == 1 and all(1 <= page <= count for page in pages)
            if count > 2 and limit >= 3:
                assert pages[2] == count


def test_protected_pdf_is_distinct_from_corrupt_and_probe_timeout(tmp_path, monkeypatch):
    from reportlab.lib.pdfencrypt import StandardEncryption
    path = tmp_path / "protected.pdf"
    pdf = canvas.Canvas(str(path), encrypt=StandardEncryption("local-test-password"))
    pdf.drawString(10, 10, "text")
    pdf.showPage()
    pdf.save()
    protected = classify_pdf(str(path), 5)
    assert protected["status"] == "error" and protected["diagnostic"]["error_code"] == "PDF_PROTECTED"
    broken = tmp_path / "broken.pdf"
    broken.write_bytes(b"%PDF malformed")
    assert classify_pdf(str(broken), 5)["diagnostic"]["error_code"] == "PARSER_FAILED"
    blank = pdf_file(tmp_path, count=8)
    from src.workers import pdf_classifier
    times = iter([0.0, 100.0])
    monkeypatch.setattr(pdf_classifier, "monotonic", lambda: next(times))
    timed = classify_pdf(str(blank), 5)
    assert timed["status"] == "unknown" and timed["result_examined_pages"] is None
    assert timed["diagnostic"]["error_code"] == "PARSER_TIMEOUT"


def worker(tmp_path, code):
    path = tmp_path / "fake_ocr.py"
    path.write_text(code, encoding="utf-8")
    return ProcessOcrWorker((sys.executable, str(path)))


GOOD_WORKER = '''import json,sys,hashlib
request=json.load(sys.stdin)
text="Planned 2026-09-22 subtotal 1234.56"
json.dump({"content_sha256":request["content_sha256"],"text_unverified":True,
"pages":[{"page":page,"text":text,"text_sha256":hashlib.sha256(text.encode()).hexdigest()}
for page in request["pages"]]},sys.stdout)
'''


def test_ocr_disabled_has_no_io_and_enabled_worker_has_atomic_run_caps(tmp_path):
    with pytest.raises(OcrError, match="OCR_DISABLED"):
        OcrCoordinator(OcrLimits()).recognize(tmp_path / "missing.pdf", [1])
    path = pdf_file(tmp_path, count=2)
    app = OcrCoordinator(OcrLimits(enabled=True, max_files=1, max_total_pages=2), worker(tmp_path, GOOD_WORKER))
    result = app.recognize(path, [1, 2])
    assert result.text_unverified and [page.page for page in result.pages] == [1, 2]
    assert app.pages_used == 2 and app.files_used == 1
    with pytest.raises(OcrError, match="OCR_RUN_LIMIT"):
        app.recognize(path, [1])


def test_ocr_timeout_cancel_and_output_limit_leave_no_running_helper(tmp_path):
    path = pdf_file(tmp_path, count=1)
    app = OcrCoordinator(OcrLimits(enabled=True, max_runtime_seconds=0.2),
        worker(tmp_path, "import time; time.sleep(10)"))
    started = time.monotonic()
    with pytest.raises(OcrError, match="OCR_TIMEOUT"):
        app.recognize(path, [1])
    assert time.monotonic() - started < 3 and app.pages_used == 1
    app = OcrCoordinator(OcrLimits(enabled=True), worker(tmp_path, "import time; time.sleep(10)"))
    cancel = threading.Event()
    timer = threading.Timer(0.1, cancel.set)
    timer.start()
    try:
        with pytest.raises(OcrError, match="OCR_CANCELLED"):
            app.recognize(path, [1], cancel=cancel)
    finally:
        timer.join()
    app = OcrCoordinator(OcrLimits(enabled=True, max_output_chars=10),
        worker(tmp_path, "import sys; sys.stdout.write('x'*1000000); sys.stdout.flush()"))
    with pytest.raises(OcrError, match="OCR_OUTPUT_LIMIT"):
        app.recognize(path, [1])


def test_ocr_import_rejects_page_or_text_identity_changes(tmp_path):
    import json
    from src.services.ocr_import import load_ocr_manifest
    from src.services.report_runner.errors import ReportRunError
    text = tmp_path / "text.txt"
    text.write_text("subtotal 1234.56", encoding="utf-8")
    doc = {"source_id": "src_" + "a" * 64, "content_sha256": "b" * 64,
        "pages_total": 8, "pages": [{"page": 8, "text": str(text),
            "text_sha256": sha256(text.read_bytes()).hexdigest()}]}
    manifest = tmp_path / "ocr.json"
    manifest.write_text(json.dumps([doc]), encoding="utf-8")
    assert load_ocr_manifest(manifest)[doc["source_id"]]["pages"][0]["page"] == 8
    text.write_text("subtotal 2000", encoding="utf-8")
    with pytest.raises(ReportRunError, match="BACKFILL_OCR_TEXT_CHANGED"):
        load_ocr_manifest(manifest)
    doc["pages"][0]["page"] = 9
    manifest.write_text(json.dumps([doc]), encoding="utf-8")
    with pytest.raises(ReportRunError, match="BACKFILL_OCR_PAGE_OUT_OF_RANGE"):
        load_ocr_manifest(manifest)
