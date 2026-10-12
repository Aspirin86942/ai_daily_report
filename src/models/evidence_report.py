"""Versioned file-evidence reports, separate from the historical work diary."""

from __future__ import annotations

from datetime import date, datetime, timedelta
from typing import Annotated, Any, Literal
from zoneinfo import ZoneInfo

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator

from .scan_sources import ScanSource, Sha256
from .scanner_contract import DateString, RelativePath

SourceId = Annotated[str, Field(pattern=r"^(src_[0-9a-f]{64}|user_input)$")]


class EvidenceModel(BaseModel):
    model_config = ConfigDict(extra="forbid")


class EvidenceItem(EvidenceModel):
    text: str = Field(min_length=1, max_length=4000)
    source_ids: list[SourceId] = Field(min_length=1, max_length=64)

    @model_validator(mode="after")
    def require_distinct_sources(self):
        if not self.text.strip() or len(set(self.source_ids)) != len(self.source_ids):
            raise ValueError("summary text and distinct source references are required")
        return self


class EvidenceSummaryData(EvidenceModel):
    summary_items: list[EvidenceItem] = Field(max_length=256)
    verification_items: list[EvidenceItem] = Field(max_length=256)

    def validate_source_ids(self, allowed: set[str]) -> EvidenceSummaryData:
        if any(set(item.source_ids) - allowed for item in self.summary_items + self.verification_items):
            raise ValueError("summary references a source outside this request")
        return self


class EvidenceMergedSummaryData(EvidenceSummaryData):
    """Preserve every item from up to eight validated 256-item batches."""

    summary_items: list[EvidenceItem] = Field(max_length=2048)
    verification_items: list[EvidenceItem] = Field(max_length=2048)


class ReportSource(EvidenceModel):
    source_id: SourceId
    source_kind: Literal["file", "user_statement"] = "file"
    relative_path: RelativePath | None
    extension: str | None
    size_bytes: int = Field(ge=0)
    modified_at_utc: str | None
    observed_at_utc: str
    content_sha256: Sha256 | None
    excerpt_sha256: Sha256
    excerpt_chars: int = Field(ge=0)
    model_chars: int = Field(default=0, ge=0)
    parser_backend: str | None
    worker_lane: str | None
    parse_status: Literal["success", "error", "timeout", "not_parsed"]
    parse_truncated: bool = False
    excerpt_truncated: bool = False
    model_truncated: bool = False
    coverage_limits: dict[str, Any] = Field(default_factory=dict)
    reason: str | None = None
    snapshot_ref: str | None = None

    @field_validator("modified_at_utc", "observed_at_utc")
    @classmethod
    def require_utc(cls, value):
        if value is not None and datetime.fromisoformat(value).utcoffset() != timedelta(0):
            raise ValueError("source timestamps must be UTC-aware")
        return value

    @model_validator(mode="after")
    def validate_source_kind(self):
        if self.source_kind == "file" and (not self.relative_path or self.source_id == "user_input"):
            raise ValueError("file source requires a relative path and file identity")
        if self.source_kind == "user_statement" and (self.source_id != "user_input" or self.relative_path is not None):
            raise ValueError("user statement must retain its distinct identity")
        if self.model_chars > self.excerpt_chars:
            raise ValueError("sent text cannot exceed extracted text")
        return self

    @classmethod
    def from_scan(cls, source: ScanSource) -> ReportSource:
        data = source.model_dump(exclude={"excerpt"})
        return cls(**data, excerpt_chars=len(source.excerpt))


class CoverageGap(EvidenceModel):
    code: str = Field(min_length=1, max_length=128)
    message: str = Field(min_length=1, max_length=1000)
    source_id: SourceId | None = None
    relative_path: RelativePath | None = None
    stage: Literal["discovery", "parse", "excerpt", "model", "schedule"]


class EvidenceDailyReportData(EvidenceMergedSummaryData):
    schema_version: Literal[2] = 2
    report_kind: Literal["file_evidence"] = "file_evidence"
    date: DateString
    scope_id: Literal["personal_work"] = "personal_work"
    root_alias: str = Field(default="work", min_length=1, max_length=64)
    timezone: Literal["Asia/Shanghai"] = "Asia/Shanghai"
    window_start_utc: str
    window_end_utc: str
    quality: Literal["complete", "partial", "no_changes"]
    sources: list[ReportSource]
    gaps: list[CoverageGap] = Field(default_factory=list)
    missed_dates: list[DateString] = Field(default_factory=list)
    generated_at: str
    user_notes: str = Field(default="", max_length=65536)

    @field_validator("window_start_utc", "window_end_utc", "generated_at")
    @classmethod
    def require_utc(cls, value: str):
        if datetime.fromisoformat(value).utcoffset() != timedelta(0):
            raise ValueError("report timestamps must be UTC-aware")
        return value

    @model_validator(mode="after")
    def validate_window_and_sources(self):
        target = date.fromisoformat(self.date)
        zone = ZoneInfo(self.timezone)
        start = datetime.combine(target, datetime.min.time(), zone)
        end = start + timedelta(days=1)
        if datetime.fromisoformat(self.window_start_utc) != start or datetime.fromisoformat(self.window_end_utc) != end:
            raise ValueError("report date and UTC scan window disagree")
        ids = [s.source_id for s in self.sources]
        if len(ids) != len(set(ids)):
            raise ValueError("report source identities must be unique")
        available = {s.source_id for s in self.sources if s.parse_status == "success" and s.excerpt_chars and s.content_sha256}
        self.validate_source_ids(available)
        if any(g.source_id is not None and g.source_id not in ids for g in self.gaps):
            raise ValueError("coverage gap references an unknown source")
        if self.quality == "no_changes" and (self.sources or self.summary_items or self.verification_items or self.gaps):
            raise ValueError("no_changes requires a complete empty scan")
        if self.quality != "no_changes" and not available:
            raise ValueError("a published evidence report needs usable evidence")
        if self.quality == "complete" and (self.gaps or any(s.parse_status != "success" or s.parse_truncated or s.excerpt_truncated or s.model_truncated for s in self.sources)):
            raise ValueError("complete report cannot hide coverage gaps")
        return self

    @property
    def is_historical_reconstruction(self) -> bool:
        generated_day = datetime.fromisoformat(self.generated_at).astimezone(ZoneInfo(self.timezone)).date()
        return generated_day > date.fromisoformat(self.date) + timedelta(days=1)

    @property
    def source_labels(self) -> dict[str, str]:
        return {source.source_id: f"S{index:03d}" for index, source in enumerate(self.sources, 1)}

    @property
    def quality_label(self) -> str:
        return {"complete": "完整覆盖", "partial": "部分覆盖", "no_changes": "未发现目标日期内修改的文件"}[self.quality]
