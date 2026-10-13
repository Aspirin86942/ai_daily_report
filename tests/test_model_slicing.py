from datetime import UTC, datetime
from hashlib import sha256
import json
from types import SimpleNamespace

import pytest

from src.models.content_audit import ContentUnit, SourceRange
from src.models.report_settings import ModelRequestSettings
from src.models.scan_sources import ScanSource
from src.services.report_runner.context import RunContext
from src.services.report_runner.model_plan import PlannedBatch, plan_evidence, reduce_truncated_batch
from src.services.report_runner.model_calls import AuditedModel
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.model_slicing import (batch_entries, batch_ranges, clip_entry, is_prefix_revision,
    prefix_ends, union_chars, validate_entry_ranges)
from src.services.report_runner.snapshots import canonical_json

CONFIG = {"model_id": "synthetic", "temperature": 0.2, "max_tokens": 2048, "base_url": "https://synthetic.invalid"}
SETTINGS = ModelRequestSettings(context_window_tokens=1_000_000, verified_max_output_tokens=4096)
CONTEXT = RunContext.create(now=datetime(2026, 10, 12, 1, tzinfo=UTC))


def source(name, text, units=None):
    digest = sha256(text.encode()).hexdigest()
    audit = {"version": 1, "primary_status": "usable_excerpt", "usable_chars": len(text)}
    if units is not None:
        audit["content_units"] = [unit.model_dump(mode="json") for unit in units]
    return ScanSource(source_id="src_" + sha256(name.encode()).hexdigest(), relative_path=name,
        extension=".md", size_bytes=len(text.encode()), modified_at_utc="2026-10-11T01:00:00Z",
        observed_at_utc="2026-10-12T01:00:00Z", content_sha256=digest, excerpt_sha256=digest,
        excerpt=text, parser_backend="light_text_v2", worker_lane="rust_core", parse_status="success",
        parse_truncated=False, excerpt_truncated=False, coverage_limits={"evidence_audit": audit}, reason=None)


def plan(sources, settings=SETTINGS):
    return plan_evidence(CONTEXT, tuple(sources), user_input="", system_prompt="仅返回有来源的 JSON。",
                         llm_config=CONFIG, settings=settings)


def test_all_first_pieces_precede_depth_and_messages_respect_real_bytes():
    sources = [source(f"a{index}.md", "计划日期2026-09-22，金额1234.56元𠀀🙂。\n" * 150) for index in range(9)]
    sources += [source(f"z{index}.md", "金额12.50元，尚未证明实际执行。") for index in range(12)]
    result = plan(sources)
    entries = [entry for batch in result.batches for entry in json.loads(batch.messages[1].content)["sources"]]
    assert {entry["source_id"] for entry in entries if entry["part"] == 1} == {
        source.source_id for source in sources}
    assert all(source.model_chars > 0 for source in result.sources)
    assert len(result.batches) <= SETTINGS.max_batches
    assert all(batch.message_bytes <= SETTINGS.max_request_utf8_bytes for batch in result.batches)
    assert all(len({entry["source_id"] for entry in json.loads(batch.messages[1].content)["sources"]})
               <= SETTINGS.max_sources_per_batch
               for batch in result.batches)
    assert plan(list(reversed(sources))).input_fingerprint == result.input_fingerprint


def test_repeated_headers_count_once_and_revision_keeps_complete_amount_rows():
    parts = ["## Sheet:采购", "单位: 元", "| 日期 | 金额 |"]
    parts += [f"| 2026-09-22 | {index}.56 |" for index in range(160)]
    units, cursor = [], 0
    for index, part in enumerate(parts):
        kind = "heading" if index == 0 else "unit" if index == 1 else "table_header" if index == 2 else "table_row"
        ranges = [] if index < 3 else [SourceRange(unit="sheet_row", start=index - 1, end=index, label="采购")]
        units.append(ContentUnit(unit_id=f"u{index}", kind=kind, text_start=cursor,
                                 text_end=cursor + len(part), ranges=ranges))
        cursor += len(part) + 1
    value = source("money.md", "\n".join(parts), units)
    result = plan([value])
    all_ranges = [span for batch in result.batches for span in batch_ranges(batch)[value.source_id]]
    assert union_chars(all_ranges) == len(value.excerpt) == result.sources[0].model_chars
    assert sum(batch.source_chars[value.source_id] for batch in result.batches) == len(value.excerpt)
    for batch in result.batches:
        for entry in batch_entries(batch):
            assert "单位: 元" in entry["text"] and "| 日期 | 金额 |" in entry["text"]
    reduced = reduce_truncated_batch(result.batches[0], parent_request_fingerprint="a" * 64)
    assert reduced.message_bytes <= result.batches[0].message_bytes // 2
    assert all(entry["text"].rstrip().endswith("|") for entry in json.loads(reduced.messages[1].content)["sources"])
    assert union_chars(batch_ranges(reduced)[value.source_id]) < union_chars(batch_ranges(result.batches[0])[value.source_id])


def test_oversized_metadata_explains_the_gap_without_creating_extra_batches():
    value = source("长" * 1200 + ".md", "计划金额1234.56元。")
    result = plan([value], SETTINGS.model_copy(update={"max_request_utf8_bytes": 2048}))
    assert not result.batches and result.sources[0].model_chars == 0
    assert result.sources[0].model_truncated
    assert "MODEL_METADATA_EXCEEDS_REQUEST_BUDGET" in result.sources[0].coverage_limits["evidence_audit"]["secondary_tags"]


def test_frozen_ranges_reject_false_and_invalid_offsets():
    batch = plan([source("work.md", "计划金额1234.56元。")]).batches[0]
    value = batch.model_dump(mode="json")
    value["input_audit"][0]["text_ranges"][0]["start"] = True
    with pytest.raises(ValueError, match="offsets must be integers"):
        PlannedBatch.model_validate(value)


def test_first_piece_includes_complete_body_after_short_title():
    text = "公司议事规则。\n股东会须包括投资人。\n关联交易超过50万元需要事先批准。"
    result = plan([source("company.docx", text)])
    entry = json.loads(result.batches[0].messages[1].content)["sources"][0]
    assert "投资人" in entry["text"] and "50万元" in entry["text"]
    assert entry["part"] == 1


def test_verified_history_gets_depth_after_every_source_first_piece():
    ordinary = source("a.md", "普通完整记录，金额12.50元。\n" * 100)
    prior = source("z.md", "历史完整记录，金额2000.25元。\n" * 100)
    prior = prior.model_copy(update={"coverage_limits": {**prior.coverage_limits,
        "history": {"excerpt_priority": True, "prior_content_sha256": prior.content_sha256}}})
    settings = SETTINGS.model_copy(update={"max_batches": 1, "max_sources_per_batch": 2,
        "max_request_utf8_bytes": 6144})
    result = plan([ordinary, prior], settings)
    counts = {item.source_id: item.model_chars for item in result.sources}
    assert counts[ordinary.source_id] > 0
    assert counts[prior.source_id] > counts[ordinary.source_id]
    changed = prior.model_copy(update={"content_sha256": "0" * 64})
    ordinary_changed = changed.model_copy(update={"coverage_limits": {
        key: value for key, value in changed.coverage_limits.items() if key != "history"}})
    assert [item.model_chars for item in plan([ordinary, changed], settings).sources] == [
        item.model_chars for item in plan([ordinary, ordinary_changed], settings).sources]
    assert plan([ordinary, changed], settings).batches[0].source_ids == result.batches[0].source_ids
    assert plan([ordinary, changed], settings).input_fingerprint != result.input_fingerprint
    assert result.batches[0].request_bytes <= 6144


def test_history_first_pieces_are_spread_across_requests_before_depth():
    values = [source(f"ordinary{index}.md", "完整记录。") for index in range(9)]
    prior_ids = set()
    for name in ("history_a.md", "history_b.md"):
        item = source(name, "历史完整记录，金额2000.25元。\n" * 20)
        item = item.model_copy(update={"coverage_limits": {**item.coverage_limits,
            "history": {"excerpt_priority": True, "prior_content_sha256": item.content_sha256}}})
        values.append(item)
        prior_ids.add(item.source_id)
    settings = SETTINGS.model_copy(update={"max_batches": 3, "max_sources_per_batch": 4,
        "max_request_utf8_bytes": 6144})
    result = plan(values, settings)
    assert len(result.batches) == 3
    assert all(item.model_chars for item in result.sources)
    assert all(len(prior_ids.intersection(batch.source_ids)) <= 1 for batch in result.batches)
    assert all(batch.request_bytes <= 6144 for batch in result.batches)


def test_batch_limit_keeps_opportunities_across_file_types_in_path_order():
    values = [source(name, "金额12.50元，需核对。") for name in
              ("z.md", "a.md", "z.txt", "a.txt", "z.xlsx", "a.xlsx")]
    values = [value.model_copy(update={"extension": "." + value.relative_path.rsplit(".", 1)[1]})
              for value in values]
    settings = SETTINGS.model_copy(update={"max_batches": 1, "max_sources_per_batch": 3})
    result = plan(values, settings)
    assert {value.relative_path for value in result.sources if value.model_chars} == {
        "a.md", "a.txt", "a.xlsx"}
    assert plan(list(reversed(values)), settings).input_fingerprint == result.input_fingerprint


def test_utf8_limit_includes_wire_envelope_and_reduction_overhead():
    settings = SETTINGS.model_copy(update={"max_request_utf8_bytes": 4096})
    result = plan_evidence(CONTEXT, [source(f"money{index}.md", "金额1234.56元，计划日期2026-09-22。\n" * 40)
                                    for index in range(3)],
        user_input="", system_prompt="仅返回有来源的 JSON。", settings=settings,
        llm_config={**CONFIG, "model_id": "synthetic" + "x" * 300})
    assert result.batches
    for batch in result.batches:
        assert len(canonical_json(batch.request())) <= 4096
    reduced = reduce_truncated_batch(result.batches[0], parent_request_fingerprint="a" * 64)
    assert len(canonical_json(reduced.request())) <= len(canonical_json(result.batches[0].request())) // 2


def test_depth_reuses_existing_batch_bytes_without_spending_source_or_call_limits():
    values = [source(f"policy{index}.md", "计划金额1234.56元；须同时保留审批条件。\n" * 35)
              for index in range(2)]
    settings = SETTINGS.model_copy(update={"max_batches": 1, "max_sources_per_batch": 2})
    result = plan(values, settings)
    assert len(result.batches) == 1
    assert {item.source_id for item in result.sources if item.model_chars} == {item.source_id for item in values}
    assert all(item.model_chars == item.excerpt_chars for item in result.sources)
    entries = json.loads(result.batches[0].messages[1].content)["sources"]
    assert len(entries) == len({entry["source_id"] for entry in entries}) == 2
    assert all(entry["part"] == 1 for entry in entries)
    assert len(canonical_json(result.batches[0].request())) <= settings.max_request_utf8_bytes


def test_compact_rows_clip_both_unicode_text_and_physical_range_at_the_same_boundary():
    rows = ["| 日期 | 金额 |", "| 2026-09-22𠀀 | 1234.56 |", "| 2026-09-23 | 20.25 |"]
    cursor, units = 0, []
    for index, row in enumerate(rows, 1):
        units.append(ContentUnit(unit_id=f"u{index}", kind="table_header" if index == 1 else "table_row",
            text_start=cursor, text_end=cursor + len(row), ranges=[SourceRange(unit="sheet_row",
                start=index, end=index + 1, label="采购", column_start=1, column_end=3)]))
        cursor += len(row) + 1
    entry = batch_entries(plan([source("money.xlsx", "\n".join(rows), units)]).batches[0])[0]
    assert len(entry["input_units"]) == 2
    reduced = clip_entry(entry, prefix_ends(entry)[0])
    assert "1234.56" in reduced["text"] and "20.25" not in reduced["text"]
    assert reduced["input_units"][-1]["ranges"][0]["start"] == 2
    assert reduced["input_units"][-1]["ranges"][0]["end"] == 3
    validate_entry_ranges(reduced)
    assert is_prefix_revision(reduced, entry)
    assert not is_prefix_revision({**reduced, "text": reduced["text"][:-1]}, entry)


def test_send_guard_checks_the_full_wire_body_even_if_messages_alone_fit():
    batch = plan([source("work.md", "计划金额1234.56元𠀀。")]).batches[0]
    batch = batch.model_copy(update={"model": "x" * SETTINGS.max_request_utf8_bytes})
    assert batch.message_bytes < SETTINGS.max_request_utf8_bytes
    with pytest.raises(ReportRunError, match="MODEL_REQUEST_BYTES_EXCEEDED"):
        AuditedModel.check_batch(SimpleNamespace(settings=SETTINGS), batch)


def test_local_audit_matches_wire_text_and_old_wire_audit_remains_recoverable():
    batch = plan([source("work.md", "计划金额1234.56元𠀀，日期2026-09-22。\n" * 40)]).batches[0]
    payload = json.loads(batch.messages[1].content)
    assert all("input_units" not in entry and "text_ranges" not in entry
               for entry in payload["sources"])
    assert batch_ranges(batch)
    tampered = batch.model_dump(mode="json")
    tampered["input_audit"][0]["text"] += "金额0元。"
    with pytest.raises(ValueError, match="audit must match the exact wire text"):
        PlannedBatch.model_validate(tampered)
    old = batch.model_dump(mode="json")
    old["input_range_version"] = 1
    payload["sources"] = old.pop("input_audit")
    old["messages"][1]["content"] = canonical_json(payload).decode("utf-8")
    restored = PlannedBatch.model_validate(old)
    assert restored.input_audit is None
    assert batch_ranges(restored) == batch_ranges(batch)
    reduced = reduce_truncated_batch(restored, parent_request_fingerprint="b" * 64)
    assert reduced.input_range_version == 1 and reduced.input_audit is None
    assert batch_ranges(reduced)
    old_local = batch.model_dump(mode="json")
    old_local["input_range_version"] = 2
    from src.services.report_runner.model_slicing import wire_entry
    payload["sources"] = [wire_entry(entry, 2) for entry in old_local["input_audit"]]
    old_local["messages"][1]["content"] = canonical_json(payload).decode("utf-8")
    restored_local = PlannedBatch.model_validate(old_local)
    assert restored_local.request() == {**batch.request(), "messages": old_local["messages"]}
    assert batch_ranges(restored_local) == batch_ranges(batch)
