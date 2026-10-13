import json

import httpx
from openai import OpenAI
import pytest

from test_evidence_daily_service import config as config_fixture, context, source
from src.models.report_settings import ModelRequestSettings
from src.services.report_runner.model_calls import OpenAITransport
from src.services.report_runner.model_plan import plan_evidence, reduce_truncated_batch, request_payload


def test_gateway_nested_parameter_matches_actual_sdk_wire_and_preserves_frozen_body(monkeypatch):
    monkeypatch.delenv("AI_DAILY_TEST_FORBID_LLM", raising=False)
    request = request_payload("deepseek-flash", [{"role": "user", "content": "合成输入 🙂"}],
        0.2, 8192, "disabled", "extra_body")
    frozen = json.dumps(request, sort_keys=True, ensure_ascii=False)
    sent = []

    def handle(incoming):
        sent.append(json.loads(incoming.content))
        assert b'"extra_body":{"thinking":{"type":"disabled"}}' in incoming.content
        return httpx.Response(200, json={"id": "synthetic", "object": "chat.completion", "created": 0,
            "model": "deepseek-flash", "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "{}"}}]})

    transport = OpenAITransport(base_url="https://gateway.invalid/v1", api_key="synthetic-key", timeout=120)
    transport.client.close()
    transport.client = OpenAI(api_key="synthetic-key", base_url="https://gateway.invalid/v1", max_retries=0,
        http_client=httpx.Client(transport=httpx.MockTransport(handle)))
    try:
        transport.send(request, timeout=120)
    finally:
        transport.client.close()
    assert sent == [request]
    assert json.dumps(request, sort_keys=True, ensure_ascii=False) == frozen


def test_initial_and_smaller_requests_count_nested_body_bytes(config_fixture):
    config = config_fixture
    settings = ModelRequestSettings(thinking_mode="disabled", thinking_parameter_location="extra_body",
        max_request_utf8_bytes=4096)
    plan = plan_evidence(context(), [source(text=("限制条件及完整金额行必须保留 🙂。\n" * 600))],
        user_input="", system_prompt="Return JSON summary_items and verification_items.",
        llm_config=config.llm_config, settings=settings)
    batch = plan.batches[0]
    assert batch.request_bytes <= 4096
    assert batch.request()["extra_body"] == {"thinking": {"type": "disabled"}}
    reduced = reduce_truncated_batch(batch, parent_request_fingerprint="a" * 64)
    assert reduced.request_bytes <= batch.request_bytes // 2
    assert reduced.request()["extra_body"] == batch.request()["extra_body"]


def test_unconfigured_location_preserves_old_request_and_settings_identity():
    settings = ModelRequestSettings(thinking_mode="disabled")
    old = settings.model_dump(exclude={"thinking_parameter_location"})
    assert settings.fingerprint_data() == old
    request = request_payload("deepseek-flash", [{"role": "user", "content": "合成输入"}], 0.2, 8192, "disabled")
    assert request["thinking"] == {"type": "disabled"} and "extra_body" not in request


@pytest.mark.parametrize("location", ["top_level", "extra_body"])
def test_explicit_location_requires_a_thinking_mode(location):
    with pytest.raises(ValueError, match="requires a thinking mode"):
        ModelRequestSettings(thinking_parameter_location=location)
