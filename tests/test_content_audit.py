import pytest

from src.models.content_audit import ContentUnit, ParsedContentAudit, SourceRange


def test_parser_units_use_unicode_positions_and_preserve_unknown_vs_empty():
    text = "金额1234.56元𠀀。"
    origin = SourceRange(unit="sheet_row", label="采购", start=2, end=3, column_start=1, column_end=4)
    audit = ParsedContentAudit(read_ranges=[origin], content_units=[ContentUnit(
        unit_id="sheet-1-row-2", kind="table_row", text_start=0, text_end=len(text), ranges=[origin])],
        usable_chars=len(text), detected_format="xlsx")
    audit.validate_content(text)
    assert ParsedContentAudit().read_ranges is None
    assert ParsedContentAudit(read_ranges=[], content_units=[], usable_chars=0).read_ranges == []
    with pytest.raises(ValueError, match="usable text"):
        audit.validate_content(text[:-1])
    with pytest.raises(ValueError, match="within parser text"):
        audit.model_copy(update={"usable_chars": None}).validate_content(text[:-1])


def test_metadata_identity_and_column_coordinates_are_bounded():
    with pytest.raises(ValueError, match="columns"):
        SourceRange(unit="pdf_page", start=1, end=2, column_start=1, column_end=3)
    unit = ContentUnit(unit_id="p1", kind="paragraph", text_start=0, text_end=1)
    with pytest.raises(ValueError, match="identities"):
        ParsedContentAudit(content_units=[unit, unit])
    with pytest.raises(ValueError):
        ParsedContentAudit(read_ranges=[SourceRange(unit="paragraph", start=1, end=2, label="甲" * 512)] * 200)
