"""Free requests keep job caps without inventing prices for older application calls."""
import pytest

from test_model_budget import ledger, PRICING
from test_evidence_daily_service import config as config_fixture, context, source
from src.models.report_settings import ModelPricing, RecoverySettings
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.model_calls import AuditedModel, Deadline
from src.services.report_runner.model_plan import plan_evidence

FREE = ModelPricing(rate_version="synthetic-free", currency="CNY",
    input_per_million_microunits=0, output_per_million_microunits=0)


def allocation(config, limits):
    batch = plan_evidence(context(), [source(text="Synthetic input")], user_input="",
        system_prompt="Return JSON summary_items and verification_items.",
        llm_config=config.llm_config, settings=config.model_request_settings).batches[0]
    client = AuditedModel(None, None, transport=None, settings=config.model_request_settings,
        limits=limits, deadline=Deadline(60), job_budget={"job_id": "synthetic-free-job",
            "job_calls_limit": 2, "job_tokens_limit": 1000000, "job_cost_limit_microunits": 0})
    return client._budget(batch, None)


def legacy_call(ledger):
    old, reference = ledger.create()
    call = ledger.runs.reserve_call(old.run_id, request_fingerprint="a" * 64, logical_step="batch_000",
        request=reference, limits=RecoverySettings())
    response = ledger.snapshots.put(old.run_id, "response", {"synthetic": True})
    ledger.runs.finish_call(call["call_id"], status="succeeded", response=response,
        usage={"total_tokens": 10})
    return old, ledger.runs.calls(old.run_id)


def test_free_uncapped_day_preserves_unknown_legacy_fees_and_job_call_cap(ledger, config_fixture):
    old, before = legacy_call(ledger)
    limits = RecoverySettings(model_pricing=FREE, max_calls_per_calendar_day=488)
    budget = allocation(config_fixture, limits)
    assert budget.run_cost_limit_microunits is None and budget.day_cost_limit_microunits is None
    assert budget.job_cost_limit_microunits == budget.cost_upper_microunits == 0
    for _ in range(2):
        current, reference = ledger.create()
        ledger.runs.reserve_call(current.run_id, request_fingerprint="b" * 64, logical_step="batch_000",
            request=reference, limits=limits, budget=budget)
    current, reference = ledger.create()
    with pytest.raises(ReportRunError, match="MODEL_JOB_CALL_BUDGET_EXHAUSTED"):
        ledger.runs.reserve_call(current.run_id, request_fingerprint="c" * 64, logical_step="batch_000",
            request=reference, limits=limits, budget=budget)
    assert ledger.runs.calls(old.run_id) == before


def test_explicit_zero_day_cost_still_blocks_unknown_legacy_fees(ledger, config_fixture):
    legacy_call(ledger)
    limits = RecoverySettings(model_pricing=FREE, max_cost_per_calendar_day_microunits=0)
    budget = allocation(config_fixture, limits)
    assert budget.day_cost_limit_microunits == 0
    current, reference = ledger.create()
    with pytest.raises(ReportRunError, match="MODEL_LEGACY_COST_UNKNOWN"):
        ledger.runs.reserve_call(current.run_id, request_fingerprint="b" * 64, logical_step="batch_000",
            request=reference, limits=limits, budget=budget)


def test_paid_automatic_money_limits_remain_enforced(config_fixture):
    limits = RecoverySettings(model_pricing=PRICING)
    budget = allocation(config_fixture, limits)
    assert budget.run_cost_limit_microunits > 0 and budget.day_cost_limit_microunits > 0
