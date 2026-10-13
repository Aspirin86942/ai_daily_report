from hashlib import sha256
from datetime import date, timedelta
import json
import sqlite3

import pytest

from test_evidence_daily_service import (
    config as config_fixture, context, source, Scanner, scan_result, service, Transport,
)
from src.models.report_settings import ModelPricing
from src.services.coverage_execution_recovery import prepare_execution_recovery
from src.services.coverage_repair import _job_calls, load_plan, make_plan, generate_candidates
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.context import RunContext
from src.services.report_runner.snapshots import SnapshotRef


class PartialTransport(Transport):
    def __init__(self, reduced_first=False):
        super().__init__()
        self.reduced_first = reduced_first

    def send(self, request, *, timeout):
        result = super().send(request, timeout=timeout)
        if self.reduced_first and len(self.requests) == 1:
            result["choices"][0]["finish_reason"] = "length"
        elif len(self.requests) > (2 if self.reduced_first else 1):
            result["choices"][0]["message"]["content"] = "{}"
        return result


def partial_job(config, *, calls_limit=16, reduced_first=False, include_pending=False):
    config.model_request_settings = config.model_request_settings.model_copy(update={
        "max_request_utf8_bytes": 16000 if reduced_first else 7000, "thinking_mode": "disabled"})
    config.recovery_settings = config.recovery_settings.model_copy(update={
        "model_pricing": ModelPricing(rate_version="synthetic-free", currency="CNY",
            input_per_million_microunits=0, output_per_million_microunits=0)})
    path = config.work_dir / "work.md"
    text = "\n".join(f"第{n}项：材料记载金额1234.56元，计划执行，完成状态待确认。"
        for n in range(500 if reduced_first else 150))
    path.write_text(text, encoding="utf-8")
    full = source(text=text, size_bytes=path.stat().st_size,
        content_sha256=sha256(path.read_bytes()).hexdigest())
    original = source(text=text[:8], size_bytes=path.stat().st_size,
        content_sha256=full.content_sha256, parse_truncated=True)
    app = service(config, Scanner(scan_result(original)), Transport())
    baseline = app.run(context())
    dates = [baseline.target_date]
    if include_pending:
        target = date.fromisoformat(baseline.target_date) + timedelta(days=1)
        app.run(RunContext.create(now=context().started_at + timedelta(days=2), target_date=target))
        dates.append(target.isoformat())
    scanner = lambda _: Scanner(scan_result(full))
    plan_path = make_plan(config, dates=dates, gap_codes=["PARSE_BOUNDED"],
        approved_root=config.work_dir, calls_limit=calls_limit, tokens_limit=1000000,
        cost_limit_microunits=0, refresh=True, scanner_factory=scanner)
    transport = PartialTransport(reduced_first)
    with pytest.raises(ReportRunError, match="MODEL_RESPONSE_INVALID"):
        generate_candidates(config, plan_path, scanner_factory=scanner,
            transport_factory=lambda: transport)
    assert len(transport.requests) == (4 if reduced_first else 3)
    plan, _, _ = load_plan(config, plan_path)
    old_calls = _job_calls(plan)
    assert [row["status"] for row in old_calls] == (["failed", "succeeded", "failed", "failed"]
        if reduced_first else ["succeeded", "failed", "failed"])
    config.model_request_settings = config.model_request_settings.model_copy(update={
        "thinking_parameter_location": "extra_body"})
    return config, app, baseline, plan_path, plan, old_calls


def prepare(config, path, **kwargs):
    return prepare_execution_recovery(config, path, reason="Synthetic gateway request recovery",
        authorization_note="Synthetic test authorizes only this temporary job", **kwargs)


def test_partial_job_recovery_reuses_success_and_preserves_all_old_consumption(config_fixture):
    config, app, baseline, path, plan, old_calls = partial_job(config_fixture)
    original_plan, original_report = path.read_bytes(), baseline.output_path.read_bytes()
    recovery_path = prepare(config, path)
    envelope = json.loads(recovery_path.read_text(encoding="utf-8"))
    recovery = envelope["recovery"]
    assert recovery["calls_limit"] == 16
    assert recovery["retained_call_ids"] == [row["call_id"] for row in old_calls]
    assert _job_calls(plan) == old_calls
    prepared = recovery["prepared_runs"][baseline.target_date]
    run = app.runs.get(prepared["run_id"])
    payload = app.snapshots.read(SnapshotRef(run["snapshot_path"], run["snapshot_sha256"]))
    assert payload["job_budget"]["job_id"] == plan.job_id
    assert payload["job_budget"]["job_tokens_limit"] == plan.tokens_limit
    assert payload["job_budget"]["job_cost_limit_microunits"] == 0
    assert len(payload["prior_model_calls"]) == 3
    assert len(payload["reused_model_calls"]) == 1
    assert payload["batches"][0]["thinking_parameter_location"] is None
    assert all(batch["thinking_parameter_location"] == "extra_body" for batch in payload["batches"][1:])
    assert all(batch["max_tokens"] == 2048 for batch in payload["batches"])
    assert prepared["calls_before_run"] == 3

    # Idempotent preparation must not reset progress or create another preview.
    runs = app.runs.list_runs()
    events = (path.parent / "events.jsonl").read_bytes()
    assert prepare(config, path) == recovery_path
    assert app.runs.list_runs() == runs
    assert (path.parent / "events.jsonl").read_bytes() == events
    transport = Transport()
    manifest_path = generate_candidates(config, path,
        scanner_factory=lambda _: pytest.fail("prepared recovery must not rescan"),
        transport_factory=lambda: transport)
    assert len(transport.requests) == len(payload["batches"]) - 1
    assert all(request["extra_body"] == {"thinking": {"type": "disabled"}} for request in transport.requests)
    assert _job_calls(plan)[:3] == old_calls
    assert path.read_bytes() == original_plan
    assert baseline.output_path.read_bytes() == original_report
    assert app.runs.current(baseline.target_date)["run_id"] == baseline.run_id
    state = json.loads((path.parent / "state.json").read_text(encoding="utf-8"))
    assert state["calls_used"] == 3 + len(transport.requests)
    candidate = json.loads((path.parent / (baseline.target_date + ".candidate.json")).read_text(encoding="utf-8"))["candidate"]
    assert candidate["execution_recovery_sha256"] == envelope["sha256"]
    assert candidate["execution_calls_limit"] == 16
    audit = candidate["report"]["sources"][0]["coverage_limits"]["evidence_audit"]
    assert audit["attempted_model_chars"] >= candidate["report"]["sources"][0]["model_chars"] > 0
    assert {range_["logical_step"] for range_ in audit["model_ranges"]} == {batch["logical_step"] for batch in payload["batches"]}
    assert generate_candidates(config, path,
        scanner_factory=lambda _: pytest.fail("completed recovery must not scan"),
        transport_factory=lambda: pytest.fail("completed recovery must not send")) == manifest_path
    assert prepare(config, path) == recovery_path
    assert json.loads((path.parent / "state.json").read_text(encoding="utf-8"))["dates"][baseline.target_date]["status"] == "candidate"


def test_partial_job_original_exhausted_limit_cannot_be_reset(config_fixture):
    config, _, _, path, plan, old_calls = partial_job(config_fixture, calls_limit=3)
    prepare(config, path)
    transport = Transport()
    with pytest.raises(ReportRunError, match="MODEL_JOB_CALL_BUDGET_EXHAUSTED"):
        generate_candidates(config, path, transport_factory=lambda: transport)
    assert transport.requests == [] and _job_calls(plan) == old_calls


def test_reused_smaller_success_gets_a_separate_full_original_input(config_fixture):
    config, app, baseline, path, plan, old_calls = partial_job(config_fixture, reduced_first=True)
    parent = app.runs.get(old_calls[0]["run_id"])
    old_input = app.snapshots.read(SnapshotRef(parent["snapshot_path"], parent["snapshot_sha256"]))
    recovery_path = prepare(config, path)
    prepared = json.loads(recovery_path.read_text(encoding="utf-8"))["recovery"]["prepared_runs"][baseline.target_date]
    payload = app.snapshots.read(SnapshotRef(prepared["snapshot_path"], prepared["snapshot_sha256"]))
    assert len(payload["batches"]) == len(old_input["batches"]) + 1
    assert payload["batches"][0]["revision"] == 1
    extra = payload["batches"][-1]
    assert extra["revision"] == 0 and extra["logical_step"] != old_input["batches"][0]["logical_step"]
    assert extra["source_chars"] == old_input["batches"][0]["source_chars"]
    assert extra["messages"][1] == old_input["batches"][0]["messages"][1]
    assert payload["supplemental_model_batches"][0]["parent_logical_step"] == "batch_000"
    assert payload["supplemental_model_batches"][0]["reused_call_id"] == old_calls[1]["call_id"]
    transport = Transport()
    generate_candidates(config, path, transport_factory=lambda: transport)
    assert len(transport.requests) == len(payload["batches"]) - 1
    candidate = json.loads((path.parent / (baseline.target_date + ".candidate.json")).read_text(encoding="utf-8"))["candidate"]
    assert candidate["report"]["sources"][0]["model_chars"] >= sum(old_input["batches"][0]["source_chars"].values())
    assert _job_calls(plan)[:4] == old_calls


def test_explicit_revision_increases_only_the_same_job_call_limit(config_fixture):
    config, _, _, path, plan, old_calls = partial_job(config_fixture, calls_limit=3)
    prepare(config, path, calls_limit=16)
    current, _, _ = load_plan(config, path)
    assert current.job_id == plan.job_id and current.calls_limit == 3
    transport = Transport()
    generate_candidates(config, path, transport_factory=lambda: transport)
    assert transport.requests and _job_calls(plan)[:3] == old_calls
    with pytest.raises(ReportRunError, match="COVERAGE_EXECUTION_ALREADY_RECOVERED"):
        prepare(config, path, calls_limit=17)


def test_recovery_freezes_unstarted_dates_without_sending_then_uses_the_same_job(config_fixture):
    config, app, baseline, path, plan, old_calls = partial_job(config_fixture,
        calls_limit=30, include_pending=True)
    text = (config.work_dir / "work.md").read_text(encoding="utf-8")
    source_path = config.work_dir / "work.md"
    scanner = lambda _: Scanner(scan_result(source(text=text,
        content_sha256=sha256(source_path.read_bytes()).hexdigest(), size_bytes=source_path.stat().st_size)))
    recovery_path = prepare(config, path, scanner_factory=scanner)
    recovery = json.loads(recovery_path.read_text(encoding="utf-8"))["recovery"]
    pending_date = next(item.date for item in plan.dates if item.date != baseline.target_date)
    pending = recovery["prepared_runs"][pending_date]
    assert pending["parent_run_id"] is None and pending["calls_before_run"] == 0
    assert pending["reused_call_ids"] == []
    payload = app.snapshots.read(SnapshotRef(pending["snapshot_path"], pending["snapshot_sha256"]))
    assert payload["execution_recovery_pending"]
    assert payload["job_budget"]["job_id"] == plan.job_id
    assert all(batch["thinking_parameter_location"] == "extra_body" for batch in payload["batches"])
    assert _job_calls(plan) == old_calls
    transport = Transport()
    manifest_path = generate_candidates(config, path,
        scanner_factory=lambda _: pytest.fail("all dates were frozen during preparation"),
        transport_factory=lambda: transport)
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))["manifest"]
    assert {item["date"] for item in manifest["candidates"]} == {item.date for item in plan.dates}
    assert _job_calls(plan)[:3] == old_calls
    assert json.loads((path.parent / "state.json").read_text(encoding="utf-8"))["calls_used"] == 3 + len(transport.requests)


def test_unknown_old_call_blocks_preparation(config_fixture):
    config, _, _, path, plan, old_calls = partial_job(config_fixture)
    with sqlite3.connect(plan.db_path) as connection:
        connection.execute("UPDATE report_model_calls SET status='unknown' WHERE call_id=?", (old_calls[-1]["call_id"],))
    with pytest.raises(ReportRunError, match="COVERAGE_RECOVERY_UNKNOWN_CALLS"):
        prepare(config, path)
    assert not (path.parent / "execution-recovery.json").exists()
    assert len(_job_calls(plan)) == 3


@pytest.mark.parametrize("target", ["ledger", "response", "prepared_run", "budget_revision"])
def test_tampered_partial_recovery_blocks_before_send(config_fixture, target):
    config, app, baseline, path, plan, old_calls = partial_job(config_fixture)
    recovery_path = prepare(config, path, calls_limit=17 if target == "budget_revision" else 16)
    if target == "response":
        call = next(call for call in app.runs.calls(old_calls[0]["run_id"]) if call["call_id"] == old_calls[0]["call_id"])
        (app.snapshots.root / call["response_path"]).write_text("{}", encoding="utf-8")
    else:
        with sqlite3.connect(plan.db_path) as connection:
            if target == "ledger":
                connection.execute("UPDATE report_model_calls SET error_code='tampered' WHERE call_id=?", (old_calls[-1]["call_id"],))
            elif target == "prepared_run":
                prepared = json.loads(recovery_path.read_text(encoding="utf-8"))["recovery"]["prepared_runs"][baseline.target_date]
                connection.execute("UPDATE report_runs SET input_fingerprint=? WHERE run_id=?", ("a" * 64, prepared["run_id"]))
            else:
                connection.execute("UPDATE report_metadata SET value='{}' WHERE key=?", ("job_calls_revision:" + plan.job_id,))
    transport = Transport()
    with pytest.raises(ReportRunError, match="(RECOVERY_INVALID|HASH_MISMATCH)"):
        generate_candidates(config, path, transport_factory=lambda: transport)
    assert transport.requests == [] and len(_job_calls(plan)) == 3
