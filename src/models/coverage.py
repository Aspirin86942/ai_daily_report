"""Versioned source audit and deterministic coverage counts; no I/O or model calls."""

from __future__ import annotations
from src.workers.pdf_pages import PdfPageAudit

from collections import Counter
from typing import Any, Literal

from pydantic import Field, model_validator

from .content_audit import AuditModel, ContentUnit, SourceRange


PrimaryStatus = Literal["usable_excerpt", "not_parsed", "parse_failed", "empty_excerpt"]
PrimaryReason = Literal[
    "candidate_file_budget", "pdf_classification_page_quota_exhausted",
    "pdf_text_extraction_quota_exhausted", "global_context_budget_exceeded",
    "excerpt_character_budget", "file_too_large", "unsupported_format",
    "source_unstable", "parse_timeout", "parser_failed", "pdf_window_no_text",
    "empty_document", "no_business_content", "unknown",
]

PRIMARY_REASON_LABELS = {
    "candidate_file_budget": "候选文件名额耗尽",
    "pdf_classification_page_quota_exhausted": "PDF 分类页额耗尽",
    "pdf_text_extraction_quota_exhausted": "PDF 正文提取名额耗尽",
    "global_context_budget_exceeded": "上下文预算耗尽",
    "excerpt_character_budget": "摘录字符预算耗尽",
    "file_too_large": "文件体积超限", "unsupported_format": "不支持的格式",
    "source_unstable": "来源读取期间变化", "parse_timeout": "解析超时",
    "parser_failed": "解析失败", "pdf_window_no_text": "PDF 已读窗口无字",
    "empty_document": "有效空文档", "no_business_content": "没有有效业务文字",
    "unknown": "原因未知",
}
_REASON_ALIASES = {
    "semantic_file_quota_exhausted": "candidate_file_budget",
    "file_size_policy": "file_too_large",
    "legacy_extension_disabled": "unsupported_format",
}


class ModelRange(AuditModel):
    logical_step: str = Field(pattern=r"^batch_[0-9]{3}$")
    revision: int = Field(default=0, ge=0, le=1)
    start: int = Field(ge=0)
    end: int = Field(ge=0)

    @model_validator(mode="after")
    def check_bounds(self):
        if self.end <= self.start:
            raise ValueError("model range must contain text")
        return self


class SourceCoverageAudit(AuditModel):
    pdf_page_audit: PdfPageAudit | None = None
    version: Literal[1] = 1
    primary_status: PrimaryStatus
    primary_reason: PrimaryReason | None = None
    secondary_tags: list[str] = Field(default_factory=list, max_length=32)
    parser_version: str | None = Field(default=None, max_length=256)
    algorithm_versions: dict[str, str] = Field(default_factory=dict)
    effective_plan_sha256: str | None = Field(default=None, pattern=r"^[0-9a-f]{64}$")
    read_ranges: list[SourceRange] | None = Field(default=None, max_length=10000)
    excerpt_ranges: list[SourceRange] | None = Field(default=None, max_length=10000)
    content_units: list[ContentUnit] | None = Field(default=None, max_length=10000)
    usable_chars: int | None = Field(default=None, ge=0)
    encoding: Literal["utf-8", "utf-8-sig", "gb18030"] | None = None
    detected_format: Literal["xls", "xlsx", "docx", "pptx", "pdf", "text", "csv"] | None = None
    planned_model_chars: int | None = Field(default=None, ge=0)
    attempted_model_chars: int | None = Field(default=None, ge=0)
    model_ranges: list[ModelRange] | None = Field(default=None, max_length=10000)
    sent_unknown: bool = False
    text_unverified: bool = False

    @model_validator(mode="after")
    def check_identity_labels(self):
        if len(set(self.secondary_tags)) != len(self.secondary_tags):
            raise ValueError("coverage labels must be distinct")
        if any(not tag or not tag.replace("_", "").isalnum() or len(tag) > 128
               for tag in self.secondary_tags):
            raise ValueError("coverage labels must be safe codes")
        if self.content_units and len({unit.unit_id for unit in self.content_units}) != len(self.content_units):
            raise ValueError("content unit identities must be distinct")
        return self

    def validate_excerpt(self, chars: int) -> None:
        if self.usable_chars is not None and self.usable_chars > chars:
            raise ValueError("usable text exceeds the excerpt")
        if any(unit.text_end > chars for unit in self.content_units or []):
            raise ValueError("content unit exceeds the excerpt")
        if len(self.model_dump_json().encode("utf-8")) > 256 * 1024:
            raise ValueError("source audit exceeds its metadata byte budget")


def validate_coverage_limits(limits: dict[str, Any]) -> dict[str, Any]:
    if "evidence_audit" in limits:
        SourceCoverageAudit.model_validate(limits["evidence_audit"])
    return limits


def source_audit(source: Any) -> SourceCoverageAudit:
    limits = source.coverage_limits
    values = dict(limits.get("evidence_audit") or {})
    chars = getattr(source, "excerpt_chars", None)
    if chars is None:
        chars = len(source.excerpt.strip())
    if values.get("usable_chars") == 0:
        chars = 0
    reason = source.reason or ""
    if source.parse_status == "not_parsed":
        status = "not_parsed"
        reason = _REASON_ALIASES.get(reason, reason)
        primary = reason if reason in PrimaryReason.__args__ else (
            "file_too_large" if "size" in reason else "unknown")
    elif source.parse_status != "success" or (chars and not source.content_sha256):
        status = "parse_failed"
        primary = "parse_timeout" if source.parse_status == "timeout" else (
            "source_unstable" if "unstable" in reason or "source_changed" in reason else "parser_failed")
    elif chars:
        status, primary = "usable_excerpt", None
    else:
        status = "empty_excerpt"
        primary = ("excerpt_character_budget" if source.excerpt_truncated or reason == "excerpt_character_budget"
                   else "pdf_window_no_text" if source.extension == ".pdf" and "no_text" in reason
                   else values.get("primary_reason") if values.get("primary_reason") in {
                       "pdf_window_no_text", "empty_document", "no_business_content"}
                   else "empty_document" if "empty" in reason else "no_business_content")
    tags = set(values.get("secondary_tags", []))
    if source.parse_status != "success":
        tags.add("SOURCE_UNAVAILABLE")
    if source.parse_truncated or source.extension == ".csv":
        tags.add("PARSE_BOUNDED")
    if source.excerpt_truncated or status == "empty_excerpt":
        tags.add("EXCERPT_BOUNDED")
    if getattr(source, "model_truncated", False):
        tags.add("MODEL_INPUT_BOUNDED")
    unverified = bool(limits.get("ocr", {}).get("text_unverified")) or values.get("text_unverified", False)
    if unverified:
        tags.add("OCR_TEXT_UNVERIFIED")
    values.update(version=1, primary_status=status, primary_reason=primary,
                  secondary_tags=sorted(tags), text_unverified=unverified)
    return SourceCoverageAudit.model_validate(values)


class CoverageSummary(AuditModel):
    registered_count: int
    usable_excerpt_count: int
    not_parsed_count: int
    parse_failed_count: int
    empty_excerpt_count: int
    model_covered_count: int
    presented_count: int
    user_statement_count: int
    primary_reasons: dict[str, int]
    actual_ranges_known_count: int

    @property
    def primary_reason_labels(self) -> dict[str, int]:
        return {PRIMARY_REASON_LABELS[key]: count for key, count in self.primary_reasons.items()}


def summarize_coverage(sources: Any, items: Any = ()) -> CoverageSummary:
    files = [source for source in sources if getattr(source, "source_kind", "file") == "file"]
    audits = [source_audit(source) for source in files]
    counts = Counter(audit.primary_status for audit in audits)
    reasons = Counter(audit.primary_reason for audit in audits if audit.primary_reason)
    presented = {source_id for item in items for source_id in item.source_ids}
    return CoverageSummary(
        registered_count=len(files), usable_excerpt_count=counts["usable_excerpt"],
        not_parsed_count=counts["not_parsed"], parse_failed_count=counts["parse_failed"],
        empty_excerpt_count=counts["empty_excerpt"],
        model_covered_count=sum(bool(getattr(source, "model_chars", 0)) and audit.primary_status == "usable_excerpt"
                                for source, audit in zip(files, audits)),
        presented_count=sum(source.source_id in presented for source in files),
        user_statement_count=len(sources) - len(files), primary_reasons=dict(sorted(reasons.items())),
        actual_ranges_known_count=sum(audit.read_ranges is not None for audit in audits),
    )
