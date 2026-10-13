"""Crash boundaries, real Windows file replacement, and persisted model budgets."""

from concurrent.futures import ThreadPoolExecutor
from datetime import UTC, datetime, timedelta
from hashlib import sha256
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from src.models.evidence_report import EvidenceDailyReportData, EvidenceSummaryData
from src.models.report_settings import ModelRequestSettings, RecoverySettings
from src.services.report_run_store import ReportRunStore
from src.services.report_runner.context import RunContext
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.locking import ReportLock
from src.services.report_runner.model_calls import AuditedModel, Deadline
from src.services.report_runner.model_plan import check_capabilities, merge_summaries, plan_evidence
from src.services.report_runner.publication import ReportPublisher
from src.services.report_runner.snapshots import SnapshotRef, SnapshotStore
from src.services.sqlite_store import SQLiteStore


NOW = datetime(2026, 10, 12, 1, tzinfo=UTC)
CONFIG = {"model_id": "deepseek-flash", "temperature": 0.2, "max_tokens": 2048,
          "base_url": "https://gateway.example.invalid", "provider": "deepseek"}
SETTINGS = ModelRequestSettings(context_window_tokens=64000, verified_max_output_tokens=4096)


@pytest.fixture
def state(tmp_path):
    store = SQLiteStore(tmp_path / "reports.sqlite3")
    runs = ReportRunStore(store, clock=lambda: NOW)
    snapshots = SnapshotStore(tmp_path / "snapshots")
    return SimpleNamespace(store=store, runs=runs, snapshots=snapshots, root=tmp_path)


def prepared(state, *, text="用户陈述：今天整理了方案。", kind="report", settings=SETTINGS):
    context = RunContext.create(now=NOW)
    plan = plan_evidence(context, (), user_input=text, system_prompt="仅返回带来源的 JSON。",
                         llm_config=CONFIG, settings=settings)
    state.runs.create(context, kind=kind)
    reference = state.snapshots.put(context.run_id, "input", {"plan": [batch.model_dump() for batch in plan.batches]})
    state.runs.prepare(context.run_id, reference, plan.input_fingerprint, list(plan.sources))
    return context, plan


def report(context, plan):
    return EvidenceDailyReportData(date=str(context.target_date), window_start_utc=context.window_start_utc.isoformat(),
        window_end_utc=context.window_end_utc.isoformat(), generated_at=NOW.isoformat(),
        quality="partial" if any(s.model_truncated for s in plan.sources) else "complete", sources=list(plan.sources),
        summary_items=[{"text": "用户陈述已经整理方案。", "source_ids": ["user_input"]}], verification_items=[])


def response(text="用户陈述已经整理方案。", *, source="user_input", finish="stop"):
    return {"choices": [{"finish_reason": finish, "message": {"content": json.dumps({"summary_items": [
        {"text": text, "source_ids": [source]}], "verification_items": []}, ensure_ascii=False)}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 60, "total_tokens": 160}}


class FakeTransport:
    def __init__(self, *responses):
        self.responses = list(responses)
        self.requests = []

    def send(self, request, *, timeout):
        self.requests.append((request, timeout))
        result = self.responses.pop(0)
        if isinstance(result, BaseException):
            raise result
        return result


class HTTPFailure(Exception):
    def __init__(self, status, retry_after=None):
        super().__init__("PRIVATE_BODY_MUST_NOT_APPEAR")
        self.status_code = status
        self.response = SimpleNamespace(headers={"retry-after": retry_after} if retry_after else {})


def model(state, transport, **kwargs):
    return AuditedModel(state.runs, state.snapshots, transport=transport, settings=SETTINGS,
        limits=kwargs.pop("limits", RecoverySettings()), deadline=kwargs.pop("deadline", Deadline(900)),
        sleeper=kwargs.pop("sleeper", lambda _seconds: None), **kwargs)


def test_immutable_snapshots_detect_tampering_and_paths(state):
    context, _ = prepared(state)
    reference = state.snapshots.put(context.run_id, "request", {"text": "中文"})
    assert state.snapshots.put(context.run_id, "request", {"text": "中文"}) == reference
    assert state.snapshots.read(reference) == {"text": "中文"}
    (state.snapshots.root / reference.path).write_bytes(b"{}")
    with pytest.raises(ReportRunError, match="HASH_MISMATCH"):
        state.snapshots.read(reference)
    with pytest.raises(ReportRunError, match="PATH_INVALID"):
        state.snapshots.read(SnapshotRef("../secret", "a" * 64))
    with pytest.raises(ReportRunError, match="CAPACITY"):
        SnapshotStore(state.root / "small", max_bytes=3).put(context.run_id, "request", {"text": "large"})


def test_windows_lock_is_handle_owned_and_reusable(state):
    with ReportLock(state.store.db_path, wait_seconds=0):
        with pytest.raises(ReportRunError, match="ALREADY_RUNNING"):
            with ReportLock(state.store.db_path, wait_seconds=0):
                pass
    # The original lock file is intentionally still present.
    with ReportLock(state.store.db_path, wait_seconds=0):
        pass


def test_prepared_inputs_cannot_be_replaced(state):
    context, plan = prepared(state)
    other = state.snapshots.put(context.run_id, "input", {"changed": True})
    with pytest.raises(ReportRunError, match="ALREADY_FROZEN"):
        state.runs.prepare(context.run_id, other, "b" * 64, list(plan.sources))


def test_successful_call_is_reused_without_a_new_reservation(state):
    context, plan = prepared(state)
    transport = FakeTransport(response())
    result = model(state, transport).generate(context.run_id, plan.batches[0])
    again = model(state, transport).generate(context.run_id, plan.batches[0])
    assert result == again and len(transport.requests) == 1
    calls = state.runs.calls(context.run_id)
    assert len(calls) == 1 and calls[0]["status"] == "succeeded"
    assert json.loads(calls[0]["usage_json"])["total_tokens"] == 160
    assert 0 < transport.requests[0][1] <= 120


def test_cached_result_bypasses_send_capabilities_and_checks_request_and_response_hashes(state, monkeypatch):
    context, plan = prepared(state)
    transport = FakeTransport(response())
    client = model(state, transport)
    client.generate(context.run_id, plan.batches[0])
    client.settings = ModelRequestSettings()
    client.transport = SimpleNamespace(prepare=lambda: pytest.fail("prepare must not be called"),
                                       send=lambda *_args, **_kwargs: pytest.fail("send must not be called"))
    monkeypatch.setattr(state.runs, "reserve_call", lambda *_args, **_kwargs: pytest.fail("no reservation"))
    assert client.generate(context.run_id, plan.batches[0]).summary_items
    call = state.runs.calls(context.run_id)[0]
    (state.snapshots.root / call["request_path"]).write_bytes(b"{}")
    with pytest.raises(ReportRunError, match="HASH_MISMATCH"):
        client.generate(context.run_id, plan.batches[0])


@pytest.mark.parametrize("workers", [1, 3])
def test_mixed_cache_only_generates_missing_batches_and_preserves_order(state, workers):
    from src.services.report_runner.evidence_daily import generate_model_batches
    settings = SETTINGS.model_copy(update={"max_request_utf8_bytes": 7000, "max_batches": 3})
    context, plan = prepared(state, text="用户陈述：今天核对了资料。" * 1000, settings=settings)
    assert len(plan.batches) == 3
    client = model(state, FakeTransport(response("first"), response("last"), response("middle")))
    client.generate(context.run_id, plan.batches[0])
    client.generate(context.run_id, plan.batches[2])
    results = generate_model_batches(client, context.run_id, list(plan.batches), workers=workers)
    assert [item.summary_items[0].text for item in results] == ["first", "middle", "last"]
    assert len(state.runs.calls(context.run_id)) == 3


def test_unknown_call_survives_resume_and_exhausts_persisted_step_budget(state):
    context, plan = prepared(state)
    reference = state.snapshots.put(context.run_id, "request", plan.batches[0].request())
    for _ in range(3):
        state.runs.reserve_call(context.run_id, request_fingerprint="a" * 64, logical_step="batch_000",
                               request=reference, limits=RecoverySettings())
        state.runs.resume(context.run_id)
    assert all(call["status"] == "unknown" for call in state.runs.calls(context.run_id))
    with pytest.raises(ReportRunError, match="STEP_BUDGET"):
        model(state, FakeTransport(response())).generate(context.run_id, plan.batches[0])


def test_daily_budget_is_atomic_across_runs_and_counts_preview(state):
    contexts = [prepared(state, kind="preview")[0] for _ in range(4)]
    limits = RecoverySettings(max_calls_per_run=1, max_calls_per_calendar_day=2)

    def reserve(context):
        reference = state.snapshots.put(context.run_id, "request", {"run": context.run_id})
        try:
            state.runs.reserve_call(context.run_id, request_fingerprint="a" * 64, logical_step="batch_000",
                                   request=reference, limits=limits)
            return "reserved"
        except ReportRunError as error:
            return error.code

    with ThreadPoolExecutor(max_workers=4) as pool:
        result = list(pool.map(reserve, contexts))
    assert result.count("reserved") == 2 and result.count("MODEL_DAY_BUDGET_EXHAUSTED") == 2


def test_calendar_budget_uses_call_time_and_run_budget_survives_midnight(state):
    context, plan = prepared(state)
    clock = [datetime(2026, 10, 12, 15, 59, 59, tzinfo=UTC)]
    state.runs.clock = lambda: clock[0]
    limits = RecoverySettings(max_calls_per_run=2, max_calls_per_calendar_day=2)
    reference = state.snapshots.put(context.run_id, "request", plan.batches[0].request())
    for step in range(2):
        state.runs.reserve_call(context.run_id, request_fingerprint="a" * 64, logical_step=f"batch_{step:03d}",
                               request=reference, limits=limits)
        clock[0] += timedelta(seconds=2)
    assert [c["call_date"] for c in state.runs.calls(context.run_id)] == ["2026-10-12", "2026-10-13"]
    with pytest.raises(ReportRunError, match="RUN_BUDGET"):
        state.runs.reserve_call(context.run_id, request_fingerprint="a" * 64, logical_step="batch_003",
                               request=reference, limits=limits)


@pytest.mark.parametrize("status", [400, 401, 403, 404, 422])
def test_permanent_model_error_never_retries_or_leaks_body(state, status):
    context, plan = prepared(state)
    transport = FakeTransport(HTTPFailure(status))
    with pytest.raises(ReportRunError, match=f"MODEL_HTTP_{status}") as caught:
        model(state, transport).generate(context.run_id, plan.batches[0])
    assert "PRIVATE" not in str(caught.value) and len(transport.requests) == 1
    state.runs.resume(context.run_id)
    with pytest.raises(ReportRunError, match="PERMANENT_FAILURE"):
        model(state, transport).generate(context.run_id, plan.batches[0])
    assert len(transport.requests) == 1


def test_transient_error_honors_retry_after_and_validation_has_one_retry(state):
    context, plan = prepared(state, text="用户陈述：今天核对了资料。" * 500)
    waits = []
    transport = FakeTransport(HTTPFailure(429, "7"), response(finish="length"), response())
    model(state, transport, sleeper=waits.append).generate(context.run_id, plan.batches[0])
    assert len(transport.requests) == 3 and waits == [7]
    assert transport.requests[0][0] == transport.requests[1][0]
    assert transport.requests[1][0] != transport.requests[2][0]
    assert [c["status"] for c in state.runs.calls(context.run_id)] == ["failed", "failed", "succeeded"]
    other, other_plan = prepared(state)
    bad = FakeTransport(response(source="src_" + "e" * 64), response(source="src_" + "e" * 64))
    with pytest.raises(ReportRunError, match="SOURCE_REFERENCE"):
        model(state, bad).generate(other.run_id, other_plan.batches[0])
    state.runs.resume(other.run_id)
    with pytest.raises(ReportRunError, match="VALIDATION_BUDGET"):
        model(state, bad).generate(other.run_id, other_plan.batches[0])
    assert len(bad.requests) == 2


def test_truncation_revision_is_smaller_frozen_and_restores_without_sending(state):
    context, plan = prepared(state, text="用户陈述：金额100元，计划明日核对。\n" * 400)
    frozen = state.runs.get(context.run_id)
    original_hash = frozen["snapshot_sha256"]
    transport = FakeTransport(response(finish="length"), response())
    client = model(state, transport)
    generated = client.generate(context.run_id, plan.batches[0])
    revised, reference = client.effective_batch(context.run_id, plan.batches[0])
    assert revised.revision == 1 and reference is not None
    assert revised.message_bytes <= plan.batches[0].message_bytes // 2
    assert state.runs.get(context.run_id)["snapshot_sha256"] == original_hash
    calls = state.runs.calls(context.run_id)
    assert [row["attempt"] for row in calls] == [1, 2]
    assert calls[0]["request_fingerprint"] != calls[1]["request_fingerprint"]
    assert calls[0]["response_path"] and json.loads(calls[0]["usage_json"])["response_audit"]["finish_reason"] == "length"
    client.settings = ModelRequestSettings()
    client.transport = SimpleNamespace(prepare=lambda: pytest.fail("no prepare"),
                                       send=lambda *_args, **_kwargs: pytest.fail("no send"))
    assert client.generate(context.run_id, plan.batches[0]) == generated
    assert len(state.runs.calls(context.run_id)) == 2


def test_second_truncation_stops_without_splitting_or_resetting_attempts(state):
    context, plan = prepared(state, text="用户陈述：金额100元，计划明日核对。\n" * 400)
    transport = FakeTransport(response(finish="length"), response(finish="length"))
    client = model(state, transport)
    with pytest.raises(ReportRunError, match="MODEL_OUTPUT_TRUNCATED"):
        client.generate(context.run_id, plan.batches[0])
    state.runs.resume(context.run_id)
    with pytest.raises(ReportRunError, match="MODEL_VALIDATION_BUDGET_EXHAUSTED"):
        client.generate(context.run_id, plan.batches[0])
    assert len(transport.requests) == len(state.runs.calls(context.run_id)) == 2


def test_half_size_request_without_space_for_meaningful_text_stops(state):
    context, plan = prepared(state)
    transport = FakeTransport(response(finish="length"))
    with pytest.raises(ReportRunError, match="MODEL_ADAPTIVE_PLAN_UNAVAILABLE"):
        model(state, transport).generate(context.run_id, plan.batches[0])
    assert len(transport.requests) == 1


def test_new_schema_rejects_excessive_items_and_unicode_text_and_legacy_stays_readable():
    from src.models.evidence_report import EvidenceBatchSummaryData, EvidenceReducedBatchSummaryData
    item = {"text": "材料记载了金额。", "source_ids": ["user_input"]}
    for payload in ({"summary_items": [item] * 7, "verification_items": []},
                    {"summary_items": [{**item, "text": "甲" * 181}], "verification_items": []},
                    {"summary_items": [], "verification_items": [{**item, "text": "甲" * 121}]}):
        with pytest.raises(ValueError):
            EvidenceBatchSummaryData.model_validate(payload)
        assert EvidenceSummaryData.model_validate(payload)
    with pytest.raises(ValueError):
        EvidenceReducedBatchSummaryData(summary_items=[item] * 4, verification_items=[])


def test_legacy_frozen_batch_uses_original_response_contract(state):
    context, plan = prepared(state)
    legacy = plan.batches[0].model_copy(update={"response_schema_version": 1, "response_schema_sha256": None})
    transport = FakeTransport(response(text="旧版合法内容。" * 30))
    client = model(state, transport)
    assert len(client.generate(context.run_id, legacy).summary_items[0].text) > 180
    assert client.cached_result(context.run_id, legacy).summary_items


def test_response_persistence_failure_does_not_resend(state, monkeypatch):
    context, plan = prepared(state)
    transport = FakeTransport(response())
    original = state.snapshots.put

    def fail_response(run_id, kind, value):
        if kind == "response":
            raise OSError("disk full")
        return original(run_id, kind, value)

    monkeypatch.setattr(state.snapshots, "put", fail_response)
    with pytest.raises(OSError):
        model(state, transport).generate(context.run_id, plan.batches[0])
    state.runs.resume(context.run_id)
    assert len(transport.requests) == 1 and state.runs.calls(context.run_id)[0]["status"] == "unknown"


def test_backoff_cannot_exceed_run_deadline(state):
    context, plan = prepared(state)
    waits = []
    with pytest.raises(ReportRunError, match="TIME_BUDGET"):
        model(state, FakeTransport(HTTPFailure(429, "60")), deadline=Deadline(1), sleeper=waits.append).generate(
            context.run_id, plan.batches[0])
    assert waits == []


def test_request_byte_and_chunk_limits_disclose_omissions(state):
    settings = SETTINGS.model_copy(update={"max_request_utf8_bytes": 2300, "max_batches": 2, "max_sources_per_batch": 2})
    context, plan = prepared(state, text=("表格行：中文\n" * 3000), settings=settings)
    assert len(plan.batches) == 2 and all(batch.message_bytes <= 2300 for batch in plan.batches)
    assert plan.sources[0].model_truncated and 0 < plan.sources[0].model_chars < plan.sources[0].excerpt_chars
    actual_chars = sum(batch.source_chars["user_input"] for batch in plan.batches)
    assert actual_chars == plan.sources[0].model_chars
    again = plan_evidence(RunContext.create(now=NOW + timedelta(hours=1)), (), user_input="表格行：中文\n" * 3000,
        system_prompt="仅返回带来源的 JSON。", llm_config=CONFIG, settings=settings)
    assert plan.input_fingerprint == again.input_fingerprint
    with pytest.raises(ReportRunError, match="UNVERIFIED"):
        check_capabilities(ModelRequestSettings(), message_bytes=100, max_tokens=2048)


def test_local_merge_only_deduplicates_identical_text_and_source_set():
    data = EvidenceSummaryData(summary_items=[{"text": "同一句", "source_ids": ["user_input"]}], verification_items=[])
    assert merge_summaries([data, data]).model_dump() == data.model_dump()


def test_merge_preserves_all_valid_items_beyond_single_batch_limit(state):
    context, plan = prepared(state)
    batches = [EvidenceSummaryData(summary_items=[
        {"text": f"材料 {index}", "source_ids": ["user_input"]}
        for index in range(start, start + 256)], verification_items=[])
        for start in range(0, 2048, 256)]
    merged = merge_summaries(batches)
    combined = EvidenceDailyReportData.model_validate({**report(context, plan).model_dump(), **merged.model_dump()})
    assert len(combined.summary_items) == 2048
    assert combined.summary_items[-1].text == "材料 2047"


def stage(state, *, text="# 已保存的精确日报\n"):
    context, plan = prepared(state)
    publisher = ReportPublisher(state.root / "reports", state.runs)
    state.runs.stage_report(context.run_id, report(context, plan), text,
        output_path=str(publisher.output_path(str(context.target_date))), publication_fingerprint="c" * 64)
    return context, publisher


def test_staged_record_is_invisible_until_verified_publication_and_repair_is_exact(state):
    context, publisher = stage(state)
    assert state.store.get_report(str(context.target_date)) is None
    path = publisher.publish(context.run_id)
    assert state.store.get_report(str(context.target_date)).schema_version == 2
    expected = path.read_bytes()
    assert b"\r" not in expected and not expected.startswith(b"\xef\xbb\xbf")
    path.unlink()
    assert publisher.publish(context.run_id).read_bytes() == expected
    assert state.runs.calls(context.run_id) == []


def test_first_publication_preserves_unknown_main_file(state):
    context, publisher = stage(state)
    path = publisher.output_path(str(context.target_date))
    path.parent.mkdir(parents=True)
    path.write_bytes(b"user content")
    with pytest.raises(ReportRunError, match="OUTPUT_CONFLICT"):
        publisher.publish(context.run_id)
    assert path.read_bytes() == b"user content" and state.store.get_report(str(context.target_date)) is None
    copies = list((publisher.root / "conflicts").glob("*.md"))
    assert len(copies) == 1 and copies[0].read_text(encoding="utf-8") == "# 已保存的精确日报\n"


def test_crash_after_file_replace_is_repaired_with_zero_generation(state, monkeypatch):
    context, publisher = stage(state)
    original = state.runs.mark_published
    monkeypatch.setattr(state.runs, "mark_published", lambda *_args, **_kwargs: (_ for _ in ()).throw(OSError("DB unavailable")))
    with pytest.raises(OSError):
        publisher.publish(context.run_id)
    assert state.runs.get(context.run_id)["phase"] == "db_committed"
    monkeypatch.setattr(state.runs, "mark_published", original)
    publisher.publish(context.run_id)
    assert state.runs.get(context.run_id)["phase"] == "published"


def test_replacement_keeps_previous_current_until_publication(state):
    first, publisher = stage(state, text="old\n")
    path = publisher.publish(first.run_id)
    second, _ = stage(state, text="new\n")
    assert state.runs.current(str(first.target_date))["run_id"] == first.run_id
    publisher.publish(second.run_id)
    assert path.read_bytes() == b"new\n" and state.runs.current(str(first.target_date))["run_id"] == second.run_id
    backups = list((publisher.root / "conflicts").glob("*.replaced.md"))
    assert len(backups) == 1 and backups[0].read_bytes() == b"old\n"


def test_external_edit_in_replace_race_is_captured_without_advancing_current(state, monkeypatch):
    from src.services.report_runner import publication
    first, publisher = stage(state, text="old\n")
    path = publisher.publish(first.run_id)
    second, _ = stage(state, text="new\n")
    replace = publication._replace_with_backup

    def racing_replace(target, candidate, backup):
        target.write_bytes(b"external edit during replacement")
        replace(target, candidate, backup)

    monkeypatch.setattr(publication, "_replace_with_backup", racing_replace)
    with pytest.raises(ReportRunError, match="CONFLICT_CAPTURED"):
        publisher.publish(second.run_id)
    assert state.runs.current(str(first.target_date))["run_id"] == first.run_id
    assert any(p.read_bytes() == b"external edit during replacement" for p in (publisher.root / "conflicts").glob("*.md"))
    state.runs.resume(second.run_id)
    with pytest.raises(ReportRunError, match="CONFLICT_CAPTURED"):
        publisher.publish(second.run_id)


def test_retention_keeps_current_metadata_and_protects_unfinished_inputs(state):
    from src.models.report_settings import RetentionSettings
    from src.services.report_runner.retention import cleanup_snapshots

    first, publisher = stage(state)
    publisher.publish(first.run_id)
    unfinished, _ = prepared(state)
    state.runs.fail(unfinished.run_id, "MODEL_DAY_BUDGET_EXHAUSTED")
    preview, _ = prepared(state, kind="preview")
    state.runs.finish_preview(preview.run_id, "complete")
    result = cleanup_snapshots(state.runs, state.snapshots, RetentionSettings(), now=NOW + timedelta(days=31))
    assert result.deleted_runs == 1 and result.deleted_files == 2 and result.reclaimed_bytes > 0
    assert state.runs.get(first.run_id)["snapshot_path"] is None
    assert state.runs.store.get_report(str(first.target_date)).sources
    with state.store._get_conn() as connection:
        assert connection.execute("SELECT COUNT(*) FROM report_run_sources WHERE run_id=?", (first.run_id,)).fetchone()[0] == 1
    run = state.runs.get(unfinished.run_id)
    assert state.snapshots.read(SnapshotRef(run["snapshot_path"], run["snapshot_sha256"]))
    publisher.output_path(str(first.target_date)).unlink()
    assert publisher.publish(first.run_id).exists()


def test_retention_rejects_unknown_files_without_deleting_them(state):
    from src.models.report_settings import RetentionSettings
    from src.services.report_runner.retention import cleanup_snapshots

    context, _ = prepared(state, kind="preview")
    state.runs.finish_preview(context.run_id, "complete")
    unexpected = state.snapshots.root / context.run_id / "user-notes.md"
    unexpected.write_text("retain this", encoding="utf-8")
    with pytest.raises(ReportRunError, match="UNRECOGNIZED"):
        cleanup_snapshots(state.runs, state.snapshots, RetentionSettings(), now=NOW + timedelta(days=31))
    assert unexpected.read_text(encoding="utf-8") == "retain this"


def test_sdk_uses_fixed_gateway_zero_hidden_retries_and_no_redirects(monkeypatch):
    import openai
    from src.services.report_runner.model_calls import OpenAITransport

    options = {}
    monkeypatch.setattr(openai, "OpenAI", lambda **kwargs: options.update(kwargs))
    OpenAITransport(base_url="https://gateway.example.invalid", api_key="synthetic-test-value", timeout=120)
    assert options["base_url"] == "https://gateway.example.invalid"
    assert options["max_retries"] == 0 and options["timeout"] == 120
    assert options["http_client"].follow_redirects is False
    options["http_client"].close()
