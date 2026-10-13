from hashlib import sha256
from types import SimpleNamespace

import pytest

from src.models.coverage import SourceCoverageAudit, SourceRange, source_audit, summarize_coverage
from src.models.evidence_report import ReportSource
from src.models.scan_sources import ScanSource


def record(name, *, status="success", chars=40, sent=20, reason=None, **changes):
    values = dict(source_id="src_" + sha256(name.encode()).hexdigest(), relative_path=name,
                  extension=".md", size_bytes=80, modified_at_utc="2026-10-11T01:00:00Z",
                  observed_at_utc="2026-10-12T01:00:00Z", content_sha256="a" * 64 if status == "success" else None,
                  excerpt_sha256="b" * 64, excerpt_chars=chars, model_chars=sent,
                  parser_backend="light_text_v2", worker_lane="rust_core", parse_status=status,
                  reason=reason)
    values.update(changes)
    return ReportSource.model_validate(values)


def test_primary_counts_do_not_double_count_overlapping_gaps_or_user_statements():
    sources = [record("used.md", parse_truncated=True, excerpt_truncated=True, model_truncated=True),
               record("skipped.md", status="not_parsed", chars=0, sent=0, reason="candidate_file_budget"),
               record("failed.md", status="timeout", chars=0, sent=0, excerpt_truncated=True),
               record("empty.md", chars=0, sent=0, reason="excerpt_character_budget", excerpt_truncated=True)]
    user = record("user.md").model_copy(update={"source_id": "user_input", "source_kind": "user_statement",
                                               "relative_path": None})
    summary = summarize_coverage(sources + [user], [SimpleNamespace(source_ids=[sources[0].source_id, "user_input"])])
    assert (summary.registered_count, summary.usable_excerpt_count, summary.not_parsed_count,
            summary.parse_failed_count, summary.empty_excerpt_count) == (4, 1, 1, 1, 1)
    assert summary.model_covered_count == summary.presented_count == summary.user_statement_count == 1
    assert summary.actual_ranges_known_count == 0
    assert summary.primary_reason_labels["摘录字符预算耗尽"] == 1


def test_legacy_ranges_stay_unknown_and_invalid_audit_is_rejected():
    assert source_audit(record("legacy.md")).read_ranges is None
    with pytest.raises(ValueError):
        record("bad.md", coverage_limits={"evidence_audit": {"version": 2, "primary_status": "usable_excerpt"}})
    with pytest.raises(ValueError):
        SourceRange(unit="pdf_page", start=0, end=1)
    with pytest.raises(ValueError):
        SourceRange(unit="text_char", start=4, end=4)
    with pytest.raises(ValueError):
        SourceCoverageAudit(primary_status="parse_failed", secondary_tags=["PRIVATE BODY"])


def test_declared_placeholder_is_not_usable_even_with_nonempty_formatted_text():
    source = record("empty.docx", coverage_limits={"evidence_audit": {
        "version": 1, "primary_status": "empty_excerpt", "usable_chars": 0,
        "primary_reason": "no_business_content"}})
    assert source_audit(source).primary_status == "empty_excerpt"
    assert summarize_coverage([source]).usable_excerpt_count == 0
    assert summarize_coverage([source]).model_covered_count == 0


@pytest.mark.parametrize("reason, expected", [
    ("semantic_file_quota_exhausted", "candidate_file_budget"),
    ("file_size_policy", "file_too_large"),
    ("legacy_extension_disabled", "unsupported_format"),
])
def test_native_policy_reasons_have_stable_primary_labels(reason, expected):
    assert source_audit(record("policy.md", status="not_parsed", chars=0, sent=0,
                               reason=reason)).primary_reason == expected


def test_report_header_computes_counts_from_sources_and_keeps_legacy_ranges_unknown(tmp_path):
    from src.models.evidence_report import EvidenceDailyReportData
    from src.services.report_gen import ReportGenerator

    source = record("render.md")
    report = EvidenceDailyReportData(date="2026-10-11", quality="complete", sources=[source],
        window_start_utc="2026-10-10T16:00:00Z", window_end_utc="2026-10-11T16:00:00Z",
        generated_at="2026-10-12T01:00:00Z", summary_items=[{
            "text": "材料记载金额为100元。", "source_ids": [source.source_id]}], verification_items=[])
    template = ReportGenerator(tmp_path).env.get_template("evidence_report_template.md")
    markdown = template.render(report=report)
    assert "登记 1 条" in markdown and "有效摘录 1 条" in markdown
    assert "成功批次覆盖 1 条" in markdown and "摘要或待核实引用 1 条" in markdown
    assert "其余 1 条来源实际范围未知" in markdown


def test_native_source_validates_nested_range_contract():
    text = "材料记载金额为100元。"
    values = record("range.md", chars=len(text), sent=0).model_dump(exclude={
        "source_kind", "excerpt_chars", "model_chars", "model_truncated", "snapshot_ref"})
    values.update(excerpt=text, excerpt_sha256=sha256(text.encode()).hexdigest(), coverage_limits={
        "evidence_audit": {"version": 1, "primary_status": "usable_excerpt", "read_ranges": [
            {"unit": "text_char", "start": 0, "end": len(text)}]}})
    source = ScanSource.model_validate(values)
    assert summarize_coverage([source]).actual_ranges_known_count == 1
    values["coverage_limits"]["evidence_audit"]["read_ranges"][0]["end"] = 0
    with pytest.raises(ValueError):
        ScanSource.model_validate(values)
