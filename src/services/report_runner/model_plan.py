"""Deterministic bounded messages; no model calls or filesystem reads in this planner."""

from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass
from hashlib import sha256
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field

from ...models.evidence_report import EvidenceItem, EvidenceMergedSummaryData, EvidenceSummaryData, ReportSource
from ...models.report_settings import ModelRequestSettings
from ...models.scan_sources import ScanSource
from .context import RunContext
from .errors import ReportRunError
from .snapshots import canonical_json, fingerprint


class ModelMessage(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)
    role: Literal["system", "user"]
    content: str


class PlannedBatch(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)
    logical_step: str = Field(pattern=r"^batch_[0-9]{3}$")
    model: str = Field(min_length=1)
    messages: list[ModelMessage] = Field(min_length=2, max_length=2)
    temperature: float = Field(ge=0, le=2)
    max_tokens: int = Field(ge=1)
    source_ids: list[str] = Field(min_length=1)
    source_chars: dict[str, int]

    def request(self) -> dict[str, Any]:
        return {"model": self.model, "messages": [message.model_dump() for message in self.messages],
                "temperature": self.temperature, "max_tokens": self.max_tokens,
                "response_format": {"type": "json_object"}}

    @property
    def message_bytes(self) -> int:
        return len(canonical_json([message.model_dump() for message in self.messages]))


@dataclass(frozen=True, slots=True)
class EvidenceModelPlan:
    batches: tuple[PlannedBatch, ...]
    sources: tuple[ReportSource, ...]
    input_fingerprint: str


def check_capabilities(settings: ModelRequestSettings, *, message_bytes: int, max_tokens: int) -> None:
    if settings.context_window_tokens is None or settings.verified_max_output_tokens is None:
        raise ReportRunError("MODEL_CAPABILITIES_UNVERIFIED")
    if max_tokens > settings.verified_max_output_tokens:
        raise ReportRunError("MODEL_OUTPUT_LIMIT_EXCEEDED")
    if message_bytes > settings.max_request_utf8_bytes:
        raise ReportRunError("MODEL_REQUEST_BYTES_EXCEEDED")
    if message_bytes + 1024 + max_tokens > settings.context_window_tokens:
        raise ReportRunError("MODEL_CONTEXT_LIMIT_EXCEEDED")


def plan_evidence(context: RunContext, sources: tuple[ScanSource, ...], *, user_input: str,
                  system_prompt: str, llm_config: dict, settings: ModelRequestSettings,
                  scanner_profile: dict | None = None, gaps: list[dict] | None = None) -> EvidenceModelPlan:
    metadata = [ReportSource.from_scan(source) for source in sorted(sources, key=lambda item: item.source_id)]
    text_by_id = {source.source_id: source.excerpt for source in sources
                  if source.parse_status == "success" and source.content_sha256}
    if user_input.strip():
        digest = sha256(user_input.encode("utf-8")).hexdigest()
        metadata.append(ReportSource(source_id="user_input", source_kind="user_statement", relative_path=None,
            extension=None, size_bytes=len(user_input.encode("utf-8")), modified_at_utc=None,
            observed_at_utc=context.started_at.isoformat(), content_sha256=digest, excerpt_sha256=digest,
            excerpt_chars=len(user_input), parser_backend=None, worker_lane=None, parse_status="success"))
        text_by_id["user_input"] = user_input
    metadata.sort(key=lambda item: item.source_id)
    window = {"target_date": str(context.target_date), "timezone": context.timezone,
              "window_start_utc": context.window_start_utc.isoformat(), "window_end_utc": context.window_end_utc.isoformat()}

    def messages(entries: list[dict]) -> list[dict]:
        return [{"role": "system", "content": system_prompt},
                {"role": "user", "content": canonical_json({**window, "sources": entries}).decode("utf-8")}]

    def fits(entries: list[dict]) -> bool:
        return len(canonical_json(messages(entries))) <= settings.max_request_utf8_bytes

    if not fits([]):
        raise ReportRunError("MODEL_PROMPT_EXCEEDS_REQUEST_BUDGET")
    groups: list[list[dict]] = []
    current: list[dict] = []
    sent: dict[str, int] = defaultdict(int)
    for source in metadata:
        text = text_by_id.get(source.source_id, "")
        cursor = 0
        part = 0
        while cursor < len(text):
            if len(current) == settings.max_sources_per_batch:
                groups.append(current)
                current = []
            if len(groups) >= settings.max_batches:
                break
            part += 1
            entry = {"source_id": source.source_id, "source_kind": source.source_kind,
                     "relative_path": source.relative_path, "modified_at_utc": source.modified_at_utc,
                     "coverage_limits": source.coverage_limits, "parse_truncated": source.parse_truncated,
                     "excerpt_truncated": source.excerpt_truncated, "part": part, "text": ""}
            low, high = 0, len(text) - cursor
            while low < high:
                middle = (low + high + 1) // 2
                if fits(current + [{**entry, "text": text[cursor:cursor + middle]}]):
                    low = middle
                else:
                    high = middle - 1
            if low == 0:
                if current:
                    groups.append(current)
                    current = []
                    part -= 1
                    continue
                # An unusually long name or metadata can leave no room for text.
                break
            if cursor + low < len(text):
                newline = text.rfind("\n", cursor, cursor + low)
                if newline >= cursor:
                    low = newline - cursor + 1
            entry["text"] = text[cursor:cursor + low]
            current.append(entry)
            sent[source.source_id] += low
            cursor += low
    if current:
        groups.append(current)
    batches = []
    for index, entries in enumerate(groups):
        counts: dict[str, int] = defaultdict(int)
        for entry in entries:
            counts[entry["source_id"]] += len(entry["text"])
        batches.append(PlannedBatch(logical_step=f"batch_{index:03d}", model=llm_config["model_id"],
            messages=messages(entries), temperature=llm_config["temperature"], max_tokens=llm_config["max_tokens"],
            source_ids=sorted(counts), source_chars=dict(counts)))
    covered = tuple(source.model_copy(update={"model_chars": sent[source.source_id],
        "model_truncated": sent[source.source_id] < source.excerpt_chars}) for source in metadata)
    # Observation times, worker/cache decisions, elapsed time and run IDs never affect content identity.
    stable_sources = [{key: value for key, value in source.model_dump(mode="json").items()
                       if key not in {"observed_at_utc", "snapshot_ref", "reason", "worker_lane"}} for source in covered]
    identity = {"version": 1, "scope_id": context.scope_id, **window, "sources": stable_sources,
                "gateway": llm_config["base_url"], "provider": llm_config.get("provider", "deepseek"),
                "system_prompt_sha256": sha256(system_prompt.encode("utf-8")).hexdigest(),
                "model": llm_config["model_id"], "temperature": llm_config["temperature"],
                "max_tokens": llm_config["max_tokens"], "response_format": settings.response_format,
                "request_limits": settings.model_dump(), "scanner_profile": scanner_profile or {},
                "gaps": gaps or [], "requests": [batch.request() for batch in batches]}
    return EvidenceModelPlan(tuple(batches), covered, fingerprint(identity))


def merge_summaries(results: list[EvidenceSummaryData]) -> EvidenceMergedSummaryData:
    def merge(field: str) -> list[EvidenceItem]:
        seen = set()
        items = []
        for result in results:
            for item in getattr(result, field):
                key = (item.text, tuple(sorted(item.source_ids)))
                if key not in seen:
                    seen.add(key)
                    items.append(item)
        return items
    return EvidenceMergedSummaryData(summary_items=merge("summary_items"), verification_items=merge("verification_items"))
