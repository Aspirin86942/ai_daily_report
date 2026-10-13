import json
from pathlib import Path

import pytest

from test_coverage_repair import scenario, plan_for
from test_evidence_daily_service import config as config_fixture, Transport, context, source
from src.services.coverage_repair import amend_failed_execution, generate_candidates, load_plan
from src.services.coverage_repair_events import RepairEvents
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.model_plan import plan_evidence, reduce_truncated_batch


class InvalidTransport(Transport):
    def send(self, request, *, timeout):
        response = super().send(request, timeout=timeout)
        response['choices'][0]['message']['content'] = '{}'
        return response


def failed_execution(scenario, *, calls):
    path = plan_for(scenario, calls=calls)
    transport = InvalidTransport()
    with pytest.raises(ReportRunError, match='MODEL_RESPONSE_INVALID'):
        generate_candidates(scenario[0], path, transport_factory=lambda: transport)
    assert len(transport.requests) == 2
    scenario[0].llm_config['temperature'] = 0.3
    return path


def test_failed_restart_preserves_plan_calls_formal_report_and_idempotency(scenario):
    config, _, app, outcome = scenario
    path = failed_execution(scenario, calls=3)
    original_plan = path.read_bytes()
    original_report = outcome.output_path.read_bytes()
    receipt = amend_failed_execution(config, path, reason='Correct a demonstrated response failure')
    plan, directory, identity = load_plan(config, path)
    receipt_before = receipt.read_bytes()
    transport = Transport()
    manifest = generate_candidates(config, path, transport_factory=lambda: transport)
    assert len(transport.requests) == 1
    assert path.read_bytes() == original_plan and outcome.output_path.read_bytes() == original_report
    journal = RepairEvents(directory, plan.job_id, identity)
    assert journal.state['calls_used'] == 3
    assert len(journal.state['dates']) == 1
    prior = json.loads(receipt.read_text(encoding='utf-8'))['amendment']
    assert len(prior['retained_call_ids']) == 2
    entry = json.loads(manifest.read_text(encoding='utf-8'))['manifest']['candidates'][0]
    assert len(app.runs.calls(entry['run_id'])) == 1
    event_count = len(journal.events)
    assert amend_failed_execution(config, path, reason='Repeated request') == receipt
    assert receipt.read_bytes() == receipt_before
    assert len(RepairEvents(directory, plan.job_id, identity).events) == event_count


def test_failed_restart_cannot_reset_the_shared_job_call_limit(scenario):
    config = scenario[0]
    path = failed_execution(scenario, calls=2)
    amend_failed_execution(config, path, reason='Preserve spent budget')
    transport = Transport()
    with pytest.raises(ReportRunError, match='MODEL_JOB_CALL_BUDGET_EXHAUSTED'):
        generate_candidates(config, path, transport_factory=lambda: transport)
    assert not transport.requests
    plan, directory, identity = load_plan(config, path)
    assert RepairEvents(directory, plan.job_id, identity).state['calls_used'] == 2


def test_unknown_execution_cannot_be_restarted(scenario):
    class InterruptedTransport(Transport):
        def send(self, request, *, timeout):
            raise KeyboardInterrupt()

    config = scenario[0]
    path = plan_for(scenario)
    with pytest.raises(KeyboardInterrupt):
        generate_candidates(config, path, transport_factory=InterruptedTransport)
    config.llm_config['temperature'] = 0.3
    with pytest.raises(ReportRunError, match='COVERAGE_EXECUTION_NOT_FAILED_ONLY'):
        amend_failed_execution(config, path, reason='Unknown request must remain charged')
    assert not (path.parent / 'execution-amendment.json').exists()


def test_successful_execution_cannot_be_restarted(scenario):
    config = scenario[0]
    path = plan_for(scenario)
    generate_candidates(config, path, transport_factory=Transport)
    config.llm_config['temperature'] = 0.3
    with pytest.raises(ReportRunError, match='COVERAGE_EXECUTION_NOT_FAILED_ONLY'):
        amend_failed_execution(config, path, reason='Do not select among successful answers')


def test_thinking_mode_is_frozen_in_both_initial_and_reduced_requests(config_fixture):
    config = config_fixture
    settings = config.model_request_settings.model_copy(update={'thinking_mode': 'disabled'})
    batch = plan_evidence(context(), [source(text=('限定条件完整保留。\n' * 500))], user_input='',
        system_prompt='Return JSON summary_items and verification_items.', llm_config=config.llm_config,
        settings=settings).batches[0]
    assert batch.request()['thinking'] == {'type': 'disabled'}
    assert batch.request_bytes <= settings.max_request_utf8_bytes
    reduced = reduce_truncated_batch(batch, parent_request_fingerprint='a' * 64)
    assert reduced.request()['thinking'] == batch.request()['thinking']
    assert reduced.request_bytes <= batch.request_bytes // 2
    legacy_settings = settings.model_copy(update={'thinking_mode': None})
    legacy = plan_evidence(context(), [source()], user_input='', system_prompt='Return JSON',
        llm_config=config.llm_config, settings=legacy_settings).batches[0]
    assert 'thinking' not in legacy.request()


def test_sdk_sends_the_exact_frozen_thinking_body_without_mutating_request(monkeypatch):
    import httpx
    from openai import OpenAI
    from src.services.report_runner.model_calls import OpenAITransport
    from src.services.report_runner.model_plan import request_payload

    monkeypatch.delenv('AI_DAILY_TEST_FORBID_LLM', raising=False)
    frozen = request_payload('deepseek-flash', [{'role': 'user', 'content': 'Return JSON.'}], 0.2, 8192, 'disabled')
    before = json.dumps(frozen, sort_keys=True)
    sent = []

    def handle(request):
        sent.append(json.loads(request.content))
        return httpx.Response(200, json={'id': 'synthetic', 'object': 'chat.completion', 'created': 0,
            'model': 'deepseek-flash', 'choices': [{'index': 0, 'finish_reason': 'stop',
                'message': {'role': 'assistant', 'content': '{}'}}]})

    transport = OpenAITransport(base_url='https://gateway.example.invalid', api_key='synthetic-test-value', timeout=120)
    transport.client.close()
    transport.client = OpenAI(base_url='https://gateway.example.invalid', api_key='synthetic-test-value',
        max_retries=0, http_client=httpx.Client(transport=httpx.MockTransport(handle)))
    try:
        transport.send(frozen, timeout=120)
    finally:
        transport.client.close()
    assert sent == [frozen]
    assert json.dumps(frozen, sort_keys=True) == before
