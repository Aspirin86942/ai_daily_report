"""Deterministic bounded PDF page selection and observed page facts."""
from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator

POLICY_VERSION = "pdf_text_presence_v2"


def sample_pages(page_count: int, max_pages: int, policy_version: str = POLICY_VERSION) -> list[int]:
    count = min(page_count, max_pages)
    if count <= 0:
        return []
    if policy_version == "pdf_text_presence_v1":
        return list(range(1, count + 1))
    if policy_version != POLICY_VERSION:
        raise ValueError("unsupported PDF selection policy")
    pages = list(range(1, min(2, count) + 1))
    if len(pages) < count and page_count not in pages:
        pages.append(page_count)
    remaining = count - len(pages)
    for i in range(1, remaining + 1):
        page = 1 + (page_count - 1) * i // (remaining + 1)
        if page not in pages:
            pages.append(page)
    for page in range(1, page_count + 1):
        if len(pages) == count:
            break
        if page not in pages:
            pages.append(page)
    return pages


class PdfPageObservation(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)
    page: int = Field(ge=1)
    has_text: bool
    has_image: bool | None


class PdfPageAudit(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)
    policy_version: Literal["pdf_text_presence_v1", "pdf_text_presence_v2"]
    checked_pages: list[int] = Field(max_length=10_000)
    observations: list[PdfPageObservation] | None = Field(default=None, max_length=10_000)

    @model_validator(mode="after")
    def validate_pages(self):
        if any(page < 1 for page in self.checked_pages) or len(set(self.checked_pages)) != len(self.checked_pages):
            raise ValueError("checked PDF pages must be positive and distinct")
        if self.observations is not None and [item.page for item in self.observations] != self.checked_pages:
            raise ValueError("PDF observations must match the checked pages")
        return self
