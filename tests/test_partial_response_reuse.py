import pytest

from test_report_recovery import state, prepared, model, response, FakeTransport
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.model_calls import freeze_model_call
from src.services.report_runner.snapshots import SnapshotRef


class OfflineTransport:
    def prepare(self):
        raise AssertionError("offline reuse must not prepare network transport")

    def send(self, request, *, timeout):
        raise AssertionError("offline reuse must not send a request")


def successful_parent(state):
    parent, plan = prepared(state, kind="preview", text="完整来源首片。\n" * 400)
    transport = FakeTransport(response())
    result = model(state, transport).generate(parent.run_id, plan.batches[0])
    call = state.runs.calls(parent.run_id)[0]
    pin = freeze_model_call(state.runs.get(parent.run_id), call).model_dump(mode="json")
    state.runs.fail(parent.run_id, "MODEL_OUTPUT_TRUNCATED")
    current, _ = prepared(state, kind="preview")
    return parent, current, plan.batches[0], call, pin, result


def test_partial_success_reuses_original_response_without_reservation_or_transport(state):
    parent, current, batch, call, pin, result = successful_parent(state)
    before = state.runs.calls(parent.run_id)
    client = model(state, OfflineTransport(), reused_calls={batch.logical_step: pin}, prior_calls=[pin])
    assert client.generate(current.run_id, batch) == result
    assert client.generate(current.run_id, batch) == result
    assert state.runs.calls(current.run_id) == []
    assert state.runs.calls(parent.run_id) == before
    assert [row["call_id"] for row in client.audited_calls(current.run_id)] == [call["call_id"]]


def test_smaller_success_is_reused_with_its_original_range_and_contract(state):
    parent, plan = prepared(state, kind="preview", text="完整来源首片。\n" * 400)
    transport = FakeTransport(response(finish="length"), response())
    original_client = model(state, transport)
    original_client.generate(parent.run_id, plan.batches[0])
    reduced, revision = original_client.effective_batch(parent.run_id, plan.batches[0])
    assert reduced.revision == 1 and revision is not None
    calls = state.runs.calls(parent.run_id)
    pin = freeze_model_call(state.runs.get(parent.run_id), calls[-1]).model_dump(mode="json")
    current, _ = prepared(state, kind="preview")
    prior = [freeze_model_call(state.runs.get(parent.run_id), call).model_dump(mode="json") for call in calls]
    client = model(state, OfflineTransport(), reused_calls={reduced.logical_step: pin}, prior_calls=prior)
    assert client.generate(current.run_id, reduced).summary_items
    with pytest.raises(ReportRunError, match="MODEL_REUSE_NOT_VALIDATED"):
        client.generate(current.run_id, plan.batches[0])
    assert state.runs.calls(current.run_id) == []
    assert [row["status"] for row in client.audited_calls(current.run_id)] == ["failed", "succeeded"]


def test_reuse_rejects_a_changed_ledger_before_any_network_preparation(state):
    parent, current, batch, call, pin, _ = successful_parent(state)
    with state.store._get_conn() as connection:
        connection.execute("UPDATE report_model_calls SET status='unknown' WHERE call_id=?", (call["call_id"],))
    client = model(state, OfflineTransport(), reused_calls={batch.logical_step: pin})
    with pytest.raises(ReportRunError, match="MODEL_REUSE_CALL_MISMATCH"):
        client.generate(current.run_id, batch)
    assert state.runs.calls(current.run_id) == []


def test_reuse_rejects_a_changed_response_snapshot_before_any_send(state):
    parent, current, batch, call, pin, _ = successful_parent(state)
    reference = SnapshotRef(call["response_path"], call["response_sha256"])
    (state.snapshots.root / reference.path).write_bytes(b"{}")
    client = model(state, OfflineTransport(), reused_calls={batch.logical_step: pin})
    with pytest.raises(ReportRunError, match="SNAPSHOT_HASH_MISMATCH"):
        client.generate(current.run_id, batch)
    assert state.runs.calls(current.run_id) == []


def test_reuse_cannot_move_a_success_to_a_new_job_budget(state):
    parent, current, batch, call, pin, _ = successful_parent(state)
    client = model(state, OfflineTransport(), reused_calls={batch.logical_step: pin},
        job_budget={"job_id": "different_job", "job_calls_limit": 40, "job_tokens_limit": 1000000})
    with pytest.raises(ReportRunError, match="MODEL_REUSE_JOB_MISMATCH"):
        client.generate(current.run_id, batch)
    assert state.runs.calls(current.run_id) == []


def test_reuse_cannot_move_a_success_to_another_business_date(state):
    parent, current, batch, call, pin, _ = successful_parent(state)
    with state.store._get_conn() as connection:
        connection.execute("UPDATE report_runs SET target_date='2026-10-10' WHERE run_id=?", (current.run_id,))
    client = model(state, OfflineTransport(), reused_calls={batch.logical_step: pin})
    with pytest.raises(ReportRunError, match="MODEL_REUSE_CALL_MISMATCH"):
        client.generate(current.run_id, batch)
    assert state.runs.calls(current.run_id) == []


def test_failed_response_cannot_be_relabelled_as_offline_success(state):
    parent, plan = prepared(state, kind="preview")
    with pytest.raises(ReportRunError, match="MODEL_RESPONSE_INVALID"):
        model(state, FakeTransport(response(text="x" * 181), response(text="x" * 181))).generate(parent.run_id, plan.batches[0])
    failed = state.runs.calls(parent.run_id)[0]
    pin = freeze_model_call(state.runs.get(parent.run_id), failed).model_dump(mode="json")
    current, _ = prepared(state, kind="preview")
    client = model(state, OfflineTransport(), reused_calls={plan.batches[0].logical_step: pin})
    with pytest.raises(ReportRunError, match="MODEL_REUSE_NOT_VALIDATED"):
        client.generate(current.run_id, plan.batches[0])
    assert state.runs.calls(current.run_id) == []
