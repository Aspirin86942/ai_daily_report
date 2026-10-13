"""Runtime revisions retain frozen bodies, generation identities and previous consumption."""
import json

import pytest

from test_evidence_daily_service import config as config_fixture, Transport
from test_coverage_execution_recovery import partial_job, prepare
import src.services.coverage_execution_recovery as recovery_module
import src.services.coverage_repair as repair_module
from src.services.coverage_repair import _job_calls, load_plan, generate_candidates
from src.services.report_runner.errors import ReportRunError


def revised_job(config, monkeypatch):
    config, app, baseline, path, plan, old_calls = partial_job(config)
    base_path = prepare(config, path)
    base_bytes = base_path.read_bytes()
    base = json.loads(base_bytes)["recovery"]
    monkeypatch.setattr(recovery_module, "algorithm_fingerprint", lambda _: "b" * 64)
    monkeypatch.setattr(repair_module, "algorithm_fingerprint", lambda _: "b" * 64)
    revision = recovery_module.revise_execution_runtime(config, path, reason="Synthetic free-budget correction",
        authorization_note="Synthetic temporary job allows this runtime revision")
    return config, app, path, plan, old_calls, base_path, base_bytes, base, revision


def test_runtime_revision_preserves_inputs_calls_and_idempotency(config_fixture, monkeypatch):
    config, app, path, plan, old, base_path, base_bytes, base, revision = revised_job(config_fixture, monkeypatch)
    assert base_path.read_bytes() == base_bytes and _job_calls(plan) == old
    assert json.loads(revision.read_text())["revision"]["prepared_runs_sha256"] == recovery_module.fingerprint(base["prepared_runs"])
    events = (path.parent / "events.jsonl").read_bytes()
    runs = app.runs.list_runs()
    assert recovery_module.revise_execution_runtime(config, path, reason="Repeat", authorization_note="Same authority") == revision
    assert (path.parent / "events.jsonl").read_bytes() == events and app.runs.list_runs() == runs
    load_plan(config, path)
    transport = Transport()
    generate_candidates(config, path, transport_factory=lambda: transport)
    assert _job_calls(plan)[:len(old)] == old
    assert len(transport.requests) > 0


def test_runtime_revision_tampering_blocks_before_send(config_fixture, monkeypatch):
    config, app, path, plan, old, base_path, base_bytes, base, revision = revised_job(config_fixture, monkeypatch)
    raw = json.loads(revision.read_text())
    raw["revision"]["prepared_runs_sha256"] = "c" * 64
    raw["sha256"] = recovery_module.fingerprint(raw["revision"])
    revision.write_text(json.dumps(raw), encoding="utf-8")
    with pytest.raises(ReportRunError, match="COVERAGE_EXECUTION_RECOVERY_INVALID"):
        load_plan(config, path)
    assert _job_calls(plan) == old and base_path.read_bytes() == base_bytes


def test_runtime_revision_rejects_another_algorithm_change(config_fixture, monkeypatch):
    config, app, path, plan, old, base_path, base_bytes, base, revision = revised_job(config_fixture, monkeypatch)
    monkeypatch.setattr(recovery_module, "algorithm_fingerprint", lambda _: "c" * 64)
    with pytest.raises(ReportRunError, match="COVERAGE_EXECUTION_ALREADY_REVISED"):
        recovery_module.revise_execution_runtime(config, path, reason="Another change", authorization_note="Original authority")
    assert _job_calls(plan) == old


def test_runtime_revision_cannot_reset_new_model_consumption(config_fixture, monkeypatch):
    config, app, baseline, path, plan, old = partial_job(config_fixture)
    prepare(config, path)
    generate_candidates(config, path, transport_factory=lambda: Transport())
    before = _job_calls(plan)
    monkeypatch.setattr(recovery_module, "algorithm_fingerprint", lambda _: "b" * 64)
    with pytest.raises(ReportRunError, match="COVERAGE_RUNTIME_NEW_CALLS_PRESENT"):
        recovery_module.revise_execution_runtime(config, path, reason="Too late", authorization_note="Original authority")
    assert _job_calls(plan) == before and not (path.parent / "execution-runtime-revision.json").exists()
