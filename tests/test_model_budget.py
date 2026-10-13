from concurrent.futures import ThreadPoolExecutor
from datetime import UTC, datetime, timedelta
import json
from threading import Barrier
from types import SimpleNamespace

import pytest

from src.models.model_budget import BudgetReservation, JobBudgetRevision, JobCallsRevision, charged_usage, cost_microunits
from src.services.job_budget_revisions import digest, job_rows, read_revision_chain
from src.models.report_settings import ModelPricing, RecoverySettings
from src.services.report_run_store import ReportRunStore
from src.services.report_runner.context import RunContext
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.snapshots import SnapshotStore
from src.services.sqlite_store import SQLiteStore


NOW = datetime(2026, 10, 12, 15, 59, 59, tzinfo=UTC)
PRICING = ModelPricing(rate_version="synthetic-v1", currency="CNY",
                       input_per_million_microunits=1_000_000, output_per_million_microunits=2_000_000)


@pytest.fixture
def ledger(tmp_path):
    clock = [NOW]
    runs = ReportRunStore(SQLiteStore(tmp_path / "reports.sqlite3"), clock=lambda: clock[0])
    snapshots = SnapshotStore(tmp_path / "snapshots")

    def create():
        context = RunContext.create(now=clock[0])
        runs.create(context)
        reference = snapshots.put(context.run_id, "input", {})
        runs.prepare(context.run_id, reference, "a" * 64, [])
        return context, snapshots.put(context.run_id, "request", {"synthetic": True})

    return SimpleNamespace(runs=runs, snapshots=snapshots, clock=clock, create=create)


def budget(**changes):
    values = dict(input_tokens_upper=100, output_tokens_upper=20, legacy_tokens_upper=120,
                  run_tokens_limit=1000, day_tokens_limit=1000, pricing=PRICING, cost_upper_microunits=140)
    values.update(changes)
    return BudgetReservation.model_validate(values)


def reserve(ledger, context, reference, allocation, step=0):
    return ledger.runs.reserve_call(context.run_id, request_fingerprint=f"{step:064x}",
        logical_step=f"batch_{step:03d}", request=reference, limits=RecoverySettings(), budget=allocation)


@pytest.mark.parametrize("kind, allocation, expected", [
    ("tokens", budget(day_tokens_limit=120), "MODEL_DAY_TOKEN_BUDGET_EXHAUSTED"),
    ("money", budget(day_cost_limit_microunits=140), "MODEL_DAY_COST_BUDGET_EXHAUSTED"),
])
def test_shared_last_budget_slot_is_atomic_across_concurrent_runs(ledger, kind, allocation, expected):
    entries = [ledger.create() for _ in range(4)]
    barrier = Barrier(4)

    def attempt(entry):
        barrier.wait(timeout=5)
        try:
            reserve(ledger, *entry, allocation)
            return "reserved"
        except ReportRunError as error:
            return error.code

    with ThreadPoolExecutor(max_workers=4) as executor:
        outcomes = list(executor.map(attempt, entries))
    assert outcomes.count("reserved") == 1 and outcomes.count(expected) == 3


def test_actual_usage_refunds_only_known_amount_and_preserves_reserved_namespace(ledger):
    context, reference = ledger.create()
    allocation = budget(run_tokens_limit=160, run_cost_limit_microunits=160)
    first = reserve(ledger, context, reference, allocation)
    ledger.runs.finish_call(first["call_id"], status="failed", usage={"prompt_tokens": 10,
        "completion_tokens": 5, "total_tokens": 15, "reserved_budget": {"spoofed": True}})
    stored = json.loads(ledger.runs.calls(context.run_id)[0]["usage_json"])
    assert stored["total_tokens"] == 15 and stored["reserved_budget"] == allocation.model_dump(mode="json")
    second = reserve(ledger, context, reference, allocation, 1)
    ledger.runs.finish_call(second["call_id"], status="unknown")
    ledger.runs.resume(context.run_id)
    with pytest.raises(ReportRunError, match="MODEL_RUN_TOKEN_BUDGET_EXHAUSTED"):
        reserve(ledger, context, reference, allocation, 2)


def test_missing_usage_retains_full_fee_reservation(ledger):
    context, reference = ledger.create()
    allocation = budget(run_cost_limit_microunits=140)
    call = reserve(ledger, context, reference, allocation)
    ledger.runs.finish_call(call["call_id"], status="failed", usage=None)
    with pytest.raises(ReportRunError, match="MODEL_RUN_COST_BUDGET_EXHAUSTED"):
        reserve(ledger, context, reference, allocation, 1)


def test_token_calendar_rolls_over_but_running_job_budget_does_not(ledger):
    context, reference = ledger.create()
    allocation = budget(run_tokens_limit=240, day_tokens_limit=120)
    reserve(ledger, context, reference, allocation)
    ledger.clock[0] += timedelta(seconds=2)
    reserve(ledger, context, reference, allocation, 1)
    assert [row["call_date"] for row in ledger.runs.calls(context.run_id)] == ["2026-10-12", "2026-10-13"]
    with pytest.raises(ReportRunError, match="MODEL_RUN_TOKEN_BUDGET_EXHAUSTED"):
        reserve(ledger, context, reference, allocation, 2)


def test_explicit_job_budget_spans_runs_and_days_and_cannot_change_in_flight(ledger):
    allocation = budget(job_id="synthetic-job", job_tokens_limit=120, job_cost_limit_microunits=140)
    reserve(ledger, *ledger.create(), allocation)
    ledger.clock[0] += timedelta(days=1)
    context, reference = ledger.create()
    with pytest.raises(ReportRunError, match="MODEL_JOB_TOKEN_BUDGET_EXHAUSTED"):
        reserve(ledger, context, reference, allocation)
    with pytest.raises(ReportRunError, match="MODEL_JOB_BUDGET_CHANGED"):
        reserve(ledger, context, reference, allocation.model_copy(update={"job_tokens_limit": 121}))


def test_pre_ledger_unknown_fees_block_money_capped_requests(ledger):
    context, reference = ledger.create()
    ledger.runs.reserve_call(context.run_id, request_fingerprint="a" * 64, logical_step="batch_000",
                             request=reference, limits=RecoverySettings())
    with pytest.raises(ReportRunError, match="MODEL_LEGACY_COST_UNKNOWN"):
        reserve(ledger, context, reference, budget(day_cost_limit_microunits=1000), 1)


def test_job_call_slot_is_atomic_excludes_other_jobs_and_survives_midnight(ledger):
    allocation = budget(job_id="coverage-selected", job_calls_limit=1, job_tokens_limit=1000)
    # An ordinary application call must not consume this job's call slot.
    reserve(ledger, *ledger.create(), budget())
    entries = [ledger.create() for _ in range(3)]
    barrier = Barrier(3)
    def attempt(entry):
        barrier.wait(timeout=5)
        try:
            reserve(ledger, *entry, allocation)
            return "reserved"
        except ReportRunError as error:
            return error.code
    with ThreadPoolExecutor(max_workers=3) as executor:
        outcomes = list(executor.map(attempt, entries))
    assert outcomes.count("reserved") == 1 and outcomes.count("MODEL_JOB_CALL_BUDGET_EXHAUSTED") == 2
    ledger.clock[0] += timedelta(days=1)
    with pytest.raises(ReportRunError, match="MODEL_JOB_CALL_BUDGET_EXHAUSTED"):
        reserve(ledger, *ledger.create(), allocation)
    with pytest.raises(ReportRunError, match="MODEL_JOB_BUDGET_CHANGED"):
        reserve(ledger, *ledger.create(), allocation.model_copy(update={"job_calls_limit": 2}))


def test_rates_and_upper_bounds_are_validated_without_float_rounding():
    assert cost_microunits(1, 0, PRICING) == 1
    fractional = PRICING.model_copy(update={"input_per_million_microunits": 1})
    assert cost_microunits(1, 0, fractional) == 1
    with pytest.raises(ValueError):
        budget(cost_upper_microunits=0)
    with pytest.raises(ValueError):
        RecoverySettings(max_cost_per_run_microunits=100)


def test_unattributed_provider_tokens_never_release_unknown_fees():
    tokens, money = charged_usage({"status": "succeeded"}, budget(),
        {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 500})
    assert tokens == 500 and money >= 140
    assert charged_usage({"status": "unknown"}, budget(),
        {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}) == (120, 140)


def revision_for(ledger, previous, calls, tokens=1000, parent=None):
    with ledger.runs.store._get_conn() as connection:
        rows = job_rows(connection, "revision-job")
    values = dict(job_id="revision-job", plan_sha256="a" * 64,
        previous_calls_limit=previous, calls_limit=calls, tokens_limit=tokens,
        cost_limit_microunits=None, pricing=PRICING,
        retained_call_ids=[row["call_id"] for row in rows], retained_calls_sha256=digest(rows),
        authorization_note="Explicit synthetic approval")
    if parent:
        return JobBudgetRevision(**values, previous_revision_sha256=parent, previous_tokens_limit=1000)
    return JobCallsRevision(**values)


def seed_revision_chain(ledger):
    old = budget(job_id="revision-job", job_calls_limit=30, job_tokens_limit=1000)
    first_context, reference = ledger.create()
    first = reserve(ledger, first_context, reference, old)
    ledger.runs.finish_call(first["call_id"], status="succeeded", response=reference,
        usage={"total_tokens": 75})
    first_revision = revision_for(ledger, 30, 40)
    first_hash = ledger.runs.register_job_calls_revision(first_revision)
    middle = old.model_copy(update={"job_calls_limit": 40,
        "request_audit": {"job_calls_revision_sha256": first_hash}})
    context, reference = ledger.create()
    second = reserve(ledger, context, reference, middle)
    ledger.runs.finish_call(second["call_id"], status="failed")
    return first_context, first_revision, first_hash, middle


def test_budget_chain_retains_original_rows_and_charges_all_versions(ledger):
    first_context, first_revision, first_hash, middle = seed_revision_chain(ledger)
    with ledger.runs.store._get_conn() as connection:
        retained = job_rows(connection, "revision-job")
        anchor = connection.execute("SELECT value FROM report_metadata WHERE key=?",
            ("job_calls_revision:revision-job",)).fetchone()[0]
    revision = revision_for(ledger, 40, 200, 1_000_000_000, first_hash)
    identity = ledger.runs.register_job_calls_revision(revision)
    assert ledger.runs.register_job_calls_revision(revision) == identity
    assert ledger.runs.register_job_calls_revision(first_revision) == first_hash
    current = middle.model_copy(update={"job_calls_limit": 200, "job_tokens_limit": 1_000_000_000,
        "request_audit": {"job_calls_revision_sha256": identity}})
    with pytest.raises(ReportRunError, match="MODEL_JOB_BUDGET_CHANGED"):
        reserve(ledger, *ledger.create(), middle)
    # Old successful usage 75 plus a failed unknown-usage reservation 120 is still charged.
    with pytest.raises(ReportRunError, match="MODEL_DAY_TOKEN_BUDGET_EXHAUSTED"):
        reserve(ledger, *ledger.create(), current.model_copy(update={"day_tokens_limit": 314}))
    reserve(ledger, *ledger.create(), current)
    ledger.clock[0] += timedelta(days=1)
    reserve(ledger, *ledger.create(), current)
    with ledger.runs.store._get_conn() as connection:
        assert job_rows(connection, "revision-job")[:2] == retained
        assert connection.execute("SELECT value FROM report_metadata WHERE key=?",
            ("job_calls_revision:revision-job",)).fetchone()[0] == anchor
        assert len(read_revision_chain(connection, "revision-job")) == 2


@pytest.mark.parametrize("change", ["parent", "tokens", "pricing", "retained", "unknown"])
def test_budget_revision_rejects_scope_drift_and_unresolved_calls(ledger, change):
    _, _, first_hash, middle = seed_revision_chain(ledger)
    if change == "unknown":
        reserve(ledger, *ledger.create(), middle)
    revision = revision_for(ledger, 40, 200, 1_000_000_000, first_hash)
    updates = {
        "parent": {"previous_revision_sha256": "b" * 64},
        "tokens": {"previous_tokens_limit": 999},
        "pricing": {"pricing": PRICING.model_copy(update={"rate_version": "changed"})},
        "retained": {"retained_calls_sha256": "b" * 64}, "unknown": {},
    }
    with pytest.raises(ReportRunError, match="MODEL_JOB_CALL_REVISION_CHANGED"):
        ledger.runs.register_job_calls_revision(revision.model_copy(update=updates[change]))


@pytest.mark.parametrize("target", ["anchor", "revision", "head", "retained"])
def test_budget_revision_detects_tampering_before_reservation(ledger, target):
    _, _, first_hash, middle = seed_revision_chain(ledger)
    identity = ledger.runs.register_job_calls_revision(revision_for(ledger, 40, 200, 1_000_000_000, first_hash))
    current = middle.model_copy(update={"job_calls_limit": 200, "job_tokens_limit": 1_000_000_000,
        "request_audit": {"job_calls_revision_sha256": identity}})
    keys = {"anchor": "job_calls_revision:revision-job",
        "revision": "job_budget_revision:revision-job:" + identity,
        "head": "job_budget_revision_head:revision-job"}
    with ledger.runs.store._get_conn() as connection:
        if target == "retained":
            connection.execute("UPDATE report_model_calls SET error_code='tampered' WHERE call_id=1")
        else:
            connection.execute("UPDATE report_metadata SET value='{}' WHERE key=?", (keys[target],))
    with pytest.raises(ReportRunError, match="MODEL_JOB_BUDGET_CHANGED"):
        reserve(ledger, *ledger.create(), current)


def test_explicit_disabled_budgets_preserve_usage_and_bound_transport_retries(ledger):
    current, reference = ledger.create()
    allocation = budget(run_tokens_limit=1, day_tokens_limit=1, job_id="unlimited-job",
        job_tokens_limit=1, job_calls_limit=1, run_cost_limit_microunits=0, day_cost_limit_microunits=0)
    limits = RecoverySettings(enforce_model_budgets=False, max_calls_per_run=1, max_calls_per_calendar_day=1)
    for step in range(4):
        call = ledger.runs.reserve_call(current.run_id, request_fingerprint=f"{step:064x}",
            logical_step=f"batch_{step:03d}", request=reference, limits=limits, budget=allocation)
        ledger.runs.finish_call(call["call_id"], status="failed", usage={"total_tokens": 50})
    calls = ledger.runs.calls(current.run_id)
    assert len(calls) == 4
    assert all(json.loads(row["usage_json"])["budget_limits_enforced"] is False for row in calls)
    assert sum(json.loads(row["usage_json"])["total_tokens"] for row in calls) == 200
    with pytest.raises(ReportRunError, match="MODEL_STEP_BUDGET_EXHAUSTED"):
        ledger.runs.reserve_call(current.run_id, request_fingerprint="0" * 64, logical_step="batch_000",
            request=reference, limits=limits, budget=allocation, max_attempts=1)
