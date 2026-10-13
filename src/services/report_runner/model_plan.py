"""Deterministic bounded messages; no model calls or filesystem reads in this planner."""

from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass
from hashlib import sha256
import json
import re
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator

from ...models.evidence_report import (EvidenceBatchSummaryData, EvidenceItem, EvidenceMergedSummaryData,
                                      EvidenceReducedBatchSummaryData, EvidenceSummaryData, ReportSource)
from ...models.report_settings import ModelRequestSettings
from ...models.coverage import source_audit
from ...models.scan_sources import ScanSource
from .context import RunContext
from .errors import ReportRunError
from .snapshots import canonical_json, fingerprint
from .model_slicing import (batch_entries, batch_ranges, clip_entry, input_units, make_entry,
                           prefix_ends, union_chars, unit_chunks, validate_entry_ranges, wire_entry)


class ModelMessage(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)
    role: Literal["system", "user"]
    content: str


def request_payload(model: str, messages: list[dict], temperature: float, max_tokens: int,
                    thinking_mode: str | None = None, thinking_parameter_location: str | None = None) -> dict:
    request = {"model": model, "messages": messages, "temperature": temperature,
               "max_tokens": max_tokens, "response_format": {"type": "json_object"}}
    if thinking_mode is not None:
        if thinking_parameter_location == "extra_body":
            request["extra_body"] = {"thinking": {"type": thinking_mode}}
        else:
            request["thinking"] = {"type": thinking_mode}
    return request


class PlannedBatch(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)
    logical_step: str = Field(pattern=r"^batch_[0-9]{3}$")
    model: str = Field(min_length=1)
    messages: list[ModelMessage] = Field(min_length=2, max_length=2)
    temperature: float = Field(ge=0, le=2)
    max_tokens: int = Field(ge=1)
    source_ids: list[str] = Field(min_length=1)
    source_chars: dict[str, int]
    response_schema_version: Literal[1, 2] = 1
    thinking_mode: Literal["enabled", "disabled"] | None = None
    thinking_parameter_location: Literal["top_level", "extra_body"] | None = None
    response_schema_sha256: str | None = Field(default=None, pattern=r"^[0-9a-f]{64}$")
    revision: int = Field(default=0, ge=0, le=1)
    parent_request_fingerprint: str | None = Field(default=None, pattern=r"^[0-9a-f]{64}$")
    revision_reason: Literal["MODEL_OUTPUT_TRUNCATED"] | None = None
    input_range_version: Literal[1, 2, 3] | None = None
    input_audit: list[dict] | None = None

    @property
    def response_model(self) -> type[EvidenceSummaryData]:
        if self.response_schema_version == 1:
            return EvidenceSummaryData
        return EvidenceReducedBatchSummaryData if self.revision else EvidenceBatchSummaryData

    @model_validator(mode="after")
    def validate_frozen_contract(self):
        if self.thinking_parameter_location is not None and self.thinking_mode is None:
            raise ValueError("thinking parameter location requires a frozen thinking mode")
        if len(set(self.source_ids)) != len(self.source_ids) or set(self.source_chars) != set(self.source_ids):
            raise ValueError("batch source identities must match their character counts")
        if any(type(chars) is not int or chars <= 0 for chars in self.source_chars.values()):
            raise ValueError("batch source character counts must be positive integers")
        if self.response_schema_version == 2:
            if self.response_schema_sha256 != fingerprint(self.response_model.model_json_schema()):
                raise ValueError("batch schema does not match its frozen identity")
            if bool(self.revision) != bool(self.parent_request_fingerprint and self.revision_reason):
                raise ValueError("batch revision requires its parent request and failure reason")
            wire = json.loads(self.messages[1].content)["sources"]
            entries = batch_entries(self)
            if self.input_range_version in {2, 3}:
                if [wire_entry(entry, self.input_range_version) for entry in entries] != wire:
                    raise ValueError("local model input audit must match the exact wire text")
            elif self.input_audit is not None:
                raise ValueError("legacy input ranges must retain their original wire audit")
            counts: dict[str, int] = defaultdict(int)
            for entry in entries:
                if not isinstance(entry["text"], str):
                    raise ValueError("batch excerpt must be text")
                counts[entry["source_id"]] += len(entry["text"])
                if self.input_range_version is not None:
                    if type(self.input_range_version) is not int:
                        raise ValueError("input range version must be an integer")
                    validate_entry_ranges(entry)
            if dict(counts) != self.source_chars:
                raise ValueError("batch counts must match frozen input text")
        elif (self.revision or self.response_schema_sha256 or self.parent_request_fingerprint
              or self.revision_reason or self.input_audit is not None or self.thinking_mode is not None
              or self.thinking_parameter_location is not None):
            raise ValueError("legacy batch cannot be reinterpreted as a new revision")
        return self

    def request(self) -> dict[str, Any]:
        return request_payload(self.model, [message.model_dump() for message in self.messages],
                               self.temperature, self.max_tokens, self.thinking_mode,
                               self.thinking_parameter_location)

    @property
    def message_bytes(self) -> int:
        return len(canonical_json([message.model_dump() for message in self.messages]))

    @property
    def request_bytes(self) -> int:
        return len(canonical_json(self.request()))


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


def _prepare_model_sources(context: RunContext, sources: tuple[ScanSource, ...], user_input: str):
    metadata = [ReportSource.from_scan(source) for source in sorted(sources, key=lambda item: item.source_id)]
    text_by_id = {source.source_id: source.excerpt for source in sources
                  if source_audit(source).primary_status == "usable_excerpt"}
    if user_input.strip():
        digest = sha256(user_input.encode("utf-8")).hexdigest()
        metadata.append(ReportSource(source_id="user_input", source_kind="user_statement", relative_path=None,
            extension=None, size_bytes=len(user_input.encode("utf-8")), modified_at_utc=None,
            observed_at_utc=context.started_at.isoformat(), content_sha256=digest, excerpt_sha256=digest,
            excerpt_chars=len(user_input), parser_backend=None, worker_lane=None, parse_status="success"))
        text_by_id["user_input"] = user_input
    metadata.sort(key=lambda item: item.source_id)
    return metadata, text_by_id


def _prepare_source_entries(metadata, text_by_id, fits):
    queues: dict[str, list[dict]] = {}
    entry_inputs: dict[str, tuple[dict, str, list]] = {}
    omitted: dict[str, str] = {}
    priority_ids = {source.source_id for source in metadata
        if (history := source.coverage_limits.get("history", {})).get("excerpt_priority") is True
        and source.content_sha256 is not None
        and history.get("prior_content_sha256") == source.content_sha256}
    for source in metadata:
        text = text_by_id.get(source.source_id, "")
        audit = source.coverage_limits.get("evidence_audit")
        units = input_units(text, audit)
        compact = {key: (audit or {}).get(key) for key in ("encoding", "detected_format", "text_unverified")}
        base = {"source_id": source.source_id, "source_kind": source.source_kind,
                "relative_path": source.relative_path, "modified_at_utc": source.modified_at_utc,
                "coverage_limits": compact, "parse_truncated": source.parse_truncated,
                "excerpt_truncated": source.excerpt_truncated}
        if source.source_id in priority_ids:
            base["history_priority"] = True
        entry_inputs[source.source_id] = (base, text, units)
        entries = []
        for part, chunk in enumerate(unit_chunks(units, len(text)), 1):
            entry = make_entry(base, text, units, chunk, part)
            if not fits([entry]):
                # Split oversized text only at proven semantic boundaries; rows remain complete.
                possible = [end for end in prefix_ends(entry) if fits([clip_entry(entry, end)])]
                if possible:
                    end = max(possible)
                    entries.append(clip_entry(entry, end))
                    omitted[source.source_id] = "MODEL_CONTENT_UNIT_BOUNDED"
                else:
                    omitted[source.source_id] = ("MODEL_METADATA_EXCEEDS_REQUEST_BUDGET"
                        if not fits([{**base, "part": part, "text": "", "text_ranges": [], "input_units": []}])
                        else "MODEL_CONTENT_UNIT_EXCEEDS_REQUEST_BUDGET")
            else:
                entries.append(entry)
        queues[source.source_id] = entries
    return queues, entry_inputs, omitted, priority_ids


def _pack_source_batches(metadata, queues, entry_inputs, omitted, priority_ids, settings, fits):
    groups: list[list[dict]] = []
    current: list[dict] = []
    # Match scanner fairness: types take turns, with stable paths within each type.
    by_type: dict[str, list[ReportSource]] = defaultdict(list)
    for source in metadata:
        by_type[source.extension or ""].append(source)
    for group in by_type.values():
        group.sort(key=lambda source: (source.relative_path or "", source.source_id))
    order = [group[index] for index in range(max(map(len, by_type.values()), default=0))
             for _, group in sorted(by_type.items()) if index < len(group)]
    order.sort(key=lambda source: source.source_id not in priority_ids)
    # Complete every source's first opportunity before scheduling any depth.
    if priority_ids:
        active_count = sum(bool(queues[source.source_id]) for source in order)
        batch_count = min(settings.max_batches,
            (active_count + settings.max_sources_per_batch - 1) // settings.max_sources_per_batch)
        groups = [[] for _ in range(batch_count)]
    first_cursor = 0
    for source in order:
        queue = queues[source.source_id]
        if not queue:
            continue
        entry = queue[0]
        if priority_ids:
            candidates = [(first_cursor + offset) % len(groups) for offset in range(len(groups))]
            selected = next((index for index in candidates
                if len(groups[index]) < settings.max_sources_per_batch
                and fits(groups[index] + [entry])), None)
            if selected is None and len(groups) < settings.max_batches:
                selected = len(groups)
                groups.append([])
            if selected is None:
                omitted[source.source_id] = "MODEL_BATCH_BUDGET_EXHAUSTED"
            else:
                groups[selected].append(entry)
                first_cursor = (selected + 1) % len(groups)
            continue
        if current and (len(current) >= settings.max_sources_per_batch or not fits(current + [entry])):
            groups.append(current)
            current = []
        if len(groups) >= settings.max_batches:
            omitted[source.source_id] = "MODEL_BATCH_BUDGET_EXHAUSTED"
            continue
        current.append(entry)
    if current:
        groups.append(current)
    groups = [group for group in groups if group]
    # A batch's source cap counts distinct files. Reuse its unspent byte space for
    # later complete blocks from those files before spending another batch.
    depth_orders = ([source for source in order if source.source_id in priority_ids],
                    [source for source in order if source.source_id not in priority_ids])
    for depth_order in depth_orders:
        for depth in range(1, max((len(queues[source.source_id]) for source in depth_order), default=0)):
            for source in depth_order:
                queue = queues[source.source_id]
                if depth >= len(queue):
                    continue
                entry = queue[depth]
                candidates = sorted(range(len(groups)), key=lambda index:
                    source.source_id not in {item["source_id"] for item in groups[index]})
                selected, selected_entries = None, None
                for index in candidates:
                    proposed = list(groups[index])
                    prior_index = next((offset for offset in reversed(range(len(proposed)))
                        if proposed[offset]["source_id"] == source.source_id
                        and entry["text_ranges"][-1]["start"]
                            <= proposed[offset]["text_ranges"][-1]["end"] + 2), None)
                    if prior_index is not None:
                        prior = proposed[prior_index]
                        base, text, units = entry_inputs[source.source_id]
                        proposed[prior_index] = make_entry(base, text, units,
                            (prior["text_ranges"][-1]["start"], entry["text_ranges"][-1]["end"]),
                            prior["part"])
                    else:
                        proposed.append(entry)
                    if (len({item["source_id"] for item in proposed}) <= settings.max_sources_per_batch
                            and fits(proposed)):
                        selected, selected_entries = index, proposed
                        break
                if selected is not None:
                    groups[selected] = selected_entries
                elif len(groups) < settings.max_batches:
                    groups.append([entry])
                else:
                    omitted[source.source_id] = "MODEL_BATCH_BUDGET_EXHAUSTED"
                    continue
    return groups


def _build_model_batches(groups, messages, llm_config, settings):
    sent_ranges: dict[str, list[dict]] = defaultdict(list)
    batches = []
    for index, entries in enumerate(groups):
        counts: dict[str, int] = defaultdict(int)
        for entry in entries:
            counts[entry["source_id"]] += len(entry["text"])
            sent_ranges[entry["source_id"]].extend(entry["text_ranges"])
        batches.append(PlannedBatch(logical_step=f"batch_{index:03d}", model=llm_config["model_id"],
            messages=messages(entries), temperature=llm_config["temperature"], max_tokens=llm_config["max_tokens"],
            source_ids=sorted(counts), source_chars=dict(counts), response_schema_version=2,
            thinking_mode=settings.thinking_mode,
            thinking_parameter_location=settings.thinking_parameter_location,
            input_range_version=3, input_audit=entries,
            response_schema_sha256=fingerprint(EvidenceBatchSummaryData.model_json_schema())))
    return batches, sent_ranges


def _annotate_model_coverage(metadata, sent_ranges, omitted):
    covered = []
    for source in metadata:
        planned = union_chars(sent_ranges[source.source_id])
        audit = source_audit(source).model_dump(mode="json")
        audit["planned_model_chars"] = planned
        audit["algorithm_versions"]["model_input"] = "source_first_semantic_v11"
        if source.source_id in omitted:
            audit["secondary_tags"] = sorted(set(audit["secondary_tags"]) | {omitted[source.source_id], "MODEL_INPUT_BOUNDED"})
        covered.append(source.model_copy(update={"model_chars": planned,
            "model_truncated": planned < source.excerpt_chars,
            "coverage_limits": {**source.coverage_limits, "evidence_audit": audit}}))
    covered = tuple(covered)
    return covered


def plan_evidence(context: RunContext, sources: tuple[ScanSource, ...], *, user_input: str,
                  system_prompt: str, llm_config: dict, settings: ModelRequestSettings,
                  scanner_profile: dict | None = None, gaps: list[dict] | None = None) -> EvidenceModelPlan:
    metadata, text_by_id = _prepare_model_sources(context, sources, user_input)
    window = {"target_date": str(context.target_date), "timezone": context.timezone,
              "window_start_utc": context.window_start_utc.isoformat(), "window_end_utc": context.window_end_utc.isoformat()}

    def messages(entries: list[dict]) -> list[dict]:
        return [{"role": "system", "content": system_prompt},
                {"role": "user", "content": canonical_json({**window,
                    "sources": [wire_entry(entry, 3) for entry in entries]}).decode("utf-8")}]

    def fits(entries: list[dict]) -> bool:
        return len(canonical_json(request_payload(llm_config["model_id"], messages(entries),
            llm_config["temperature"], llm_config["max_tokens"], settings.thinking_mode,
            settings.thinking_parameter_location))) <= settings.max_request_utf8_bytes

    if not fits([]):
        raise ReportRunError("MODEL_PROMPT_EXCEEDS_REQUEST_BUDGET")
    queues, entry_inputs, omitted, priority_ids = _prepare_source_entries(metadata, text_by_id, fits)
    groups = _pack_source_batches(metadata, queues, entry_inputs, omitted, priority_ids, settings, fits)
    batches, sent_ranges = _build_model_batches(groups, messages, llm_config, settings)
    covered = _annotate_model_coverage(metadata, sent_ranges, omitted)
    # Observation times, worker/cache decisions, elapsed time and run IDs never affect content identity.
    stable_sources = [{key: value for key, value in source.model_dump(mode="json").items()
                       if key not in {"observed_at_utc", "snapshot_ref", "reason", "worker_lane"}} for source in covered]
    identity = {"version": 3, "scope_id": context.scope_id, **window, "sources": stable_sources,
                "model_input_policy": "source_first_semantic_v11",
                "batch_contract": "bounded_summary_v1", "merge_policy": "distinct_text_and_sources_v1",
                "gateway": llm_config["base_url"], "provider": llm_config.get("provider", "deepseek"),
                "system_prompt_sha256": sha256(system_prompt.encode("utf-8")).hexdigest(),
                "model": llm_config["model_id"], "temperature": llm_config["temperature"],
                "max_tokens": llm_config["max_tokens"], "response_format": settings.response_format,
                "request_limits": settings.fingerprint_data(), "scanner_profile": scanner_profile or {},
                "gaps": gaps or [], "requests": [batch.request() for batch in batches]}
    return EvidenceModelPlan(tuple(batches), covered, fingerprint(identity))


def merge_summaries(results: list[EvidenceSummaryData], *, bounded: bool = False) -> EvidenceMergedSummaryData:
    if bounded and any(len(result.summary_items) > 6 or len(result.verification_items) > 3 for result in results):
        raise ReportRunError("MODEL_MERGE_BATCH_LIMIT_EXCEEDED")
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


def reduce_truncated_batch(batch: PlannedBatch, *, parent_request_fingerprint: str) -> PlannedBatch:
    """One smaller request within the original logical step; keep complete text units."""
    if batch.response_schema_version != 2 or batch.revision:
        raise ReportRunError("MODEL_STRATEGY_BUDGET_EXHAUSTED")
    payload = json.loads(batch.messages[1].content)
    original = batch_entries(batch)
    base = {key: value for key, value in payload.items() if key != "sources"}
    system = batch.messages[0].content + "\n本次为有界缩批：最多3条摘要、1条待核实；仅引用本次提供的来源。"
    byte_limit = batch.request_bytes // 2

    def messages(entries):
        return [{"role": "system", "content": system},
                {"role": "user", "content": canonical_json({**base, "sources":
                    [wire_entry(entry, batch.input_range_version) for entry in entries] if batch.input_range_version in {2, 3}
                    else entries}).decode("utf-8")}]

    def fits(entries):
        return len(canonical_json(request_payload(batch.model, messages(entries),
            batch.temperature, batch.max_tokens, batch.thinking_mode,
            batch.thinking_parameter_location))) <= byte_limit

    selected, ends_by_index = [], {}
    seen = set()
    for entry in original:
        source_id = entry["source_id"]
        if source_id in seen:
            continue
        seen.add(source_id)
        text = entry["text"]
        ends = prefix_ends(entry)
        if not ends:
            continue
        first = clip_entry(entry, ends[0])
        if fits(selected + [first]):
            ends_by_index[len(selected)] = (entry, ends)
            selected.append(first)
    if not selected:
        raise ReportRunError("MODEL_ADAPTIVE_PLAN_UNAVAILABLE")
    # Source first, then add bounded depth while retaining whole sentences/rows.
    for index, (original_entry, ends) in ends_by_index.items():
        for end in ends[1:]:
            if end > 512:
                break
            candidate = clip_entry(original_entry, end)
            if not fits(selected[:index] + [candidate] + selected[index + 1:]):
                break
            selected[index] = candidate
    counts = {entry["source_id"]: len(entry["text"]) for entry in selected}
    return PlannedBatch(logical_step=batch.logical_step, model=batch.model, messages=messages(selected),
        temperature=batch.temperature, max_tokens=batch.max_tokens, source_ids=sorted(counts), source_chars=counts,
        response_schema_version=2, response_schema_sha256=fingerprint(EvidenceReducedBatchSummaryData.model_json_schema()),
        thinking_mode=batch.thinking_mode,
        thinking_parameter_location=batch.thinking_parameter_location,
        input_range_version=batch.input_range_version,
        input_audit=selected if batch.input_range_version in {2, 3} else None,
        revision=1, parent_request_fingerprint=parent_request_fingerprint, revision_reason="MODEL_OUTPUT_TRUNCATED")
