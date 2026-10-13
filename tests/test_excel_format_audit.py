from pathlib import Path

from openpyxl import Workbook

from src.services.document_parser import parse_document_file
from src.workers.contracts import parse_legacy_excel_payload


def test_mislabeled_xlsx_uses_real_format_and_preserves_full_amount_rows(tmp_path: Path):
    path = tmp_path / "采购.xls"
    workbook = Workbook()
    sheet = workbook.active
    sheet.title = "采购"
    sheet.append(["单位：元", None])
    sheet.append(["项目", "金额"])
    sheet.append(["𠀀采购", 1234.56])
    sheet.append(["合计", 1234.56])
    workbook.save(path)
    original = path.read_bytes()
    payload = parse_legacy_excel_payload(path, ".xls", {"excel_max_rows": 3})
    assert payload.error is None
    assert payload.evidence_audit.detected_format == "xlsx"
    assert "1234.56" in payload.content and "𠀀采购" in payload.content
    assert "合计" not in payload.content
    assert payload.truncated
    assert path.read_bytes() == original
    rows = payload.evidence_audit.read_ranges
    assert [row.start for row in rows] == [1, 2, 3]
    assert [unit.kind for unit in payload.evidence_audit.content_units if unit.kind != "heading"] == [
        "unit", "table_header", "table_header", "table_row"
    ]


def test_true_xls_range_and_format_are_known():
    path = Path(__file__).parent / "fixtures/worker_documents/legacy_sample.xls"
    payload = parse_document_file(path, ".xls", {"excel_max_rows": 1, "excel_max_columns": 2, "excel_max_sheets": 1})
    assert payload.error is None
    assert payload.evidence_audit.detected_format == "xls"
    assert "Legacy XLS worker content" in payload.content
    assert "Row beyond budget" not in payload.content
    assert [row.start for row in payload.evidence_audit.read_ranges] == [1, 2]
    payload.evidence_audit.validate_content(payload.content)


def test_unknown_and_corrupt_xls_return_controlled_errors_without_source_content(tmp_path: Path):
    path = tmp_path / "private.xls"
    for data in [b"sensitive-amount-1234.56", b"PK\x03\x04sensitive-amount-1234.56",
            b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1sensitive-amount-1234.56"]:
        path.write_bytes(data)
        payload = parse_legacy_excel_payload(path, ".xls", {})
        assert payload.error and "1234.56" not in payload.error
        assert payload.content == ""
        assert not payload.retryable
