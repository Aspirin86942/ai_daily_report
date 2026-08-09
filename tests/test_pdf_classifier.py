"""pypdfium2 PDF 文本层分类器（pdf_text_presence_v1）的严格测试。

该模块负责 `text_in_parse_window / no_text_in_parse_window / unknown / error`
四态判定，以及数值门禁（spec Part 3.3）。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from src.workers.pdf_classifier import (
    POLICY_VERSION,
    classify_pdf,
    classifier_version_payload,
)

FIXTURES = Path(__file__).parent / "fixtures" / "pdf_benchmark"
CLASSIFIER_FIXTURES = Path(__file__).parent / "fixtures" / "pdf_classifier"


def test_text_pdf_is_text_in_parse_window():
    # case_01 是含中文的文本 PDF（fixture 已存在）
    result = classify_pdf(str(FIXTURES / "case_01.pdf"), max_pages=5)
    assert result["status"] == "text_in_parse_window", result
    assert result["page_count"] == 1
    assert result["result_examined_pages"] == 1


def test_image_pdf_is_no_text_in_parse_window(tmp_path):
    # 用 reportlab 造一张无文字层 PDF（纯图形，无文本 operator）
    from reportlab.pdfgen import canvas

    pdf = tmp_path / "img.pdf"
    c = canvas.Canvas(str(pdf))
    c.setFillColorRGB(0.9, 0.9, 0.9)
    c.rect(10, 10, 100, 100, fill=1, stroke=1)
    c.circle(200, 200, 40, fill=1, stroke=1)
    c.showPage()
    c.save()
    result = classify_pdf(str(pdf), max_pages=5)
    assert result["status"] == "no_text_in_parse_window", result
    assert result["page_count"] == 1
    assert result["result_examined_pages"] == 1


def test_blank_page_is_no_text_in_parse_window(tmp_path):
    from reportlab.pdfgen import canvas

    pdf = tmp_path / "blank.pdf"
    c = canvas.Canvas(str(pdf))
    c.showPage()
    c.save()
    result = classify_pdf(str(pdf), max_pages=5)
    assert result["status"] == "no_text_in_parse_window", result


def test_max_pages_window_stops_scanning_early():
    # 第一页有文字，第二页没有：只检查第一页即返回 text
    from reportlab.pdfgen import canvas

    pdf = FIXTURES / "case_02.pdf"
    result = classify_pdf(str(pdf), max_pages=5)
    assert result["status"] == "text_in_parse_window"
    assert result["result_examined_pages"] == 1


def test_missing_file_is_unknown_not_bare_exception(tmp_path):
    result = classify_pdf(str(tmp_path / "nope.pdf"), max_pages=5)
    assert result["status"] == "unknown", result
    assert result["diagnostic"] is not None
    assert result["diagnostic"]["retryable"] is True


def test_corrupt_pdf_is_deterministic_error(tmp_path):
    pdf = tmp_path / "corrupt.pdf"
    pdf.write_bytes(b"%PDF-1.4 not a real pdf\n" + b"\x00\x01\x02" * 8)
    result = classify_pdf(str(pdf), max_pages=5)
    assert result["status"] == "error", result
    assert result["diagnostic"] is not None
    assert result["diagnostic"]["retryable"] is False


def test_valid_text_char_rejects_whitespace_controls_and_replacement():
    from src.workers.pdf_classifier import _is_valid_text_char

    assert _is_valid_text_char("a") is True
    assert _is_valid_text_char("中") is True
    assert _is_valid_text_char(" ") is False
    assert _is_valid_text_char("\n") is False
    assert _is_valid_text_char("\t") is False
    assert _is_valid_text_char("\x00") is False  # Cc
    assert _is_valid_text_char("�") is False  # replacement
    assert _is_valid_text_char("​") is False  # Cf zero-width space
    assert _is_valid_text_char("\ud800") is False  # Cs surrogate


def test_classifier_version_payload_is_strict_and_stable():
    payload = classifier_version_payload()
    assert payload["contract"] == "ai_daily_pdf_classifier"
    assert payload["protocol_version"] == 1
    assert payload["classifier_contract_version"] == "ai_daily_pdf_classifier_v1"
    assert payload["policy_version"] == POLICY_VERSION
    assert payload["classifier_build"]
    assert len(payload["classifier_build"]) == 64
    for field in (
        "python_implementation",
        "python_version",
        "unicode_data_version",
        "pypdfium2_version",
        "pdfium_version",
        "target_triple",
    ):
        assert payload[field], field
    # 两次调用稳定
    assert classifier_version_payload() == payload


def test_classifier_version_json_is_one_frame():
    from src.workers.pdf_classifier import classifier_version_json

    frame = classifier_version_json()
    assert frame.endswith(b"\n")
    parsed = json.loads(frame)
    assert parsed["contract"] == "ai_daily_pdf_classifier"
    assert len(parsed["classifier_build"]) == 64


def test_lightweight_identity_matches_loaded_pdf_runtime():
    import importlib.metadata

    import pypdfium2

    from src.workers.pdf_classifier_identity import (
        _pdfium_native_version,
        _pypdfium2_version,
    )

    assert _pypdfium2_version() == importlib.metadata.version("pypdfium2")
    assert _pdfium_native_version() == str(pypdfium2.internal.PDFIUM_INFO)


# ---------------------------------------------------------------------------
# 数值门禁（spec Part 3.3）：固定 corpus manifest
# ---------------------------------------------------------------------------


def test_classifier_numeric_gate():
    manifest_path = CLASSIFIER_FIXTURES / "manifest.json"
    if not manifest_path.exists():
        pytest.skip("classifier corpus manifest not generated")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    entries = manifest["entries"]
    assert isinstance(entries, list) and len(entries) >= 135

    by_ground_truth: dict[str, list[dict[str, object]]] = {}
    for entry in entries:
        by_ground_truth.setdefault(entry["ground_truth"], []).append(entry)

    text_entries = by_ground_truth.get("text_in_parse_window", [])
    no_text_entries = by_ground_truth.get("no_text_in_parse_window", [])
    error_entries = by_ground_truth.get("error", [])

    assert len(text_entries) >= 30, f"text corpus {len(text_entries)} < 30"
    assert len(no_text_entries) >= 100, f"no-text corpus {len(no_text_entries)} < 100"
    assert len(error_entries) >= 5, f"error corpus {len(error_entries)} < 5"

    # 特殊类别各至少 3 份
    categories: dict[str, int] = {}
    for entry in entries:
        for category in entry.get("categories", []):
            categories[category] = categories.get(category, 0) + 1
    for required in (
        "sparse",
        "cjk",
        "rotated",
        "mixed",
        "ocr_hidden",
        "blank",
        "beyond_max_pages",
        "encrypted",
        "corrupt",
    ):
        assert categories.get(required, 0) >= 3, f"{required} corpus < 3"

    false_negatives = 0
    false_positives = 0
    unknown_on_valid = 0
    error_on_valid = 0
    error_mismatch = 0

    for entry in entries:
        fixture = CLASSIFIER_FIXTURES / entry["file"]
        assert fixture.exists(), f"missing fixture {fixture}"
        max_pages = int(entry.get("max_pages", 5))
        result = classify_pdf(str(fixture), max_pages=max_pages)
        status = result["status"]
        ground_truth = entry["ground_truth"]

        if ground_truth == "text_in_parse_window":
            if status == "no_text_in_parse_window":
                false_negatives += 1
            elif status == "unknown":
                unknown_on_valid += 1
            elif status == "error":
                error_on_valid += 1
        elif ground_truth == "no_text_in_parse_window":
            if status == "text_in_parse_window":
                false_positives += 1
            elif status == "unknown":
                unknown_on_valid += 1
            elif status == "error":
                error_on_valid += 1
        elif ground_truth == "error":
            if status != "error":
                error_mismatch += 1

    # text false-negative 必须 0/全部
    assert false_negatives == 0, f"text false-negative = {false_negatives}"
    # no-text false-positive ≤ 0.1%；分母不足 1000 时等价为 0 个误判
    assert false_positives == 0, f"no-text false-positive = {false_positives}"
    # valid fixture unknown/error = 0
    assert unknown_on_valid == 0, f"valid fixture unknown = {unknown_on_valid}"
    assert error_on_valid == 0, f"valid fixture error = {error_on_valid}"
    # deterministic-error fixture 状态匹配率 100%
    assert error_mismatch == 0, f"error fixture mismatch = {error_mismatch}"
