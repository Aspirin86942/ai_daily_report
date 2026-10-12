"""Single retry owner with persisted budgets, bounded requests and reusable validated responses."""

from __future__ import annotations

from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
import os
from time import monotonic, sleep
from typing import Callable, Protocol

from pydantic import BaseModel, ValidationError

from ...models.evidence_report import EvidenceSummaryData
from ...models.report_settings import ModelRequestSettings, RecoverySettings
from ..report_run_store import ReportRunStore
from .errors import ReportRunError
from .model_plan import PlannedBatch, check_capabilities
from .snapshots import SnapshotRef, SnapshotStore, fingerprint


class Deadline:
    def __init__(self, seconds: float, *, clock: Callable[[], float] = monotonic):
        self.clock = clock
        self.ends_at = clock() + seconds

    def remaining(self) -> float:
        remaining = self.ends_at - self.clock()
        if remaining <= 0:
            raise ReportRunError("RUN_TIME_BUDGET_EXHAUSTED")
        return remaining


class ModelTransport(Protocol):
    def send(self, request: dict, *, timeout: float) -> dict: ...


class LazyModelTransport:
    def __init__(self, factory: Callable[[], ModelTransport]):
        self.factory = factory
        self.instance = None

    def prepare(self) -> None:
        if self.instance is None:
            self.instance = self.factory()
        prepare = getattr(self.instance, "prepare", None)
        if prepare is not None:
            prepare()

    def send(self, request: dict, *, timeout: float) -> dict:
        if self.instance is None:
            self.prepare()
        return self.instance.send(request, timeout=timeout)


class OpenAITransport:
    def __init__(self, *, base_url: str, api_key: str, timeout: float):
        from openai import DefaultHttpxClient, OpenAI
        # Only explicit application attempts are allowed to reach the gateway.
        self.client = OpenAI(base_url=base_url, api_key=api_key, max_retries=0, timeout=timeout,
                             http_client=DefaultHttpxClient(follow_redirects=False))

    def prepare(self) -> None:
        if os.environ.get("AI_DAILY_TEST_FORBID_LLM") == "1":
            raise ReportRunError("MODEL_CALLS_FORBIDDEN")

    def send(self, request: dict, *, timeout: float) -> dict:
        self.prepare()
        response = self.client.chat.completions.create(**request, timeout=timeout)
        return response.model_dump(mode="json")


def _classify_failure(error: Exception) -> tuple[str, bool]:
    if isinstance(error, ReportRunError):
        return error.code, error.code in {"MODEL_RESPONSE_INVALID", "MODEL_OUTPUT_TRUNCATED", "MODEL_SOURCE_REFERENCE_INVALID"}
    status = getattr(error, "status_code", None)
    if isinstance(status, int):
        return f"MODEL_HTTP_{status}", status in {408, 429} or 500 <= status < 600
    # Use SDK exception types, never inspect or log its request/response content.
    from openai import APIConnectionError, APITimeoutError
    if isinstance(error, APITimeoutError):
        return "MODEL_TIMEOUT", True
    if isinstance(error, APIConnectionError):
        return "MODEL_CONNECTION_FAILED", True
    return "MODEL_CALL_FAILED", False


def _retry_delay(error: Exception, attempt: int, now: datetime) -> float:
    headers = getattr(getattr(error, "response", None), "headers", {})
    value = headers.get("retry-after") if headers else None
    try:
        if value is not None:
            try:
                return max(0.0, float(value))
            except ValueError:
                when = parsedate_to_datetime(value)
                if when.utcoffset() is not None:
                    return max(0.0, (when - now).total_seconds())
    except (TypeError, ValueError, OverflowError):
        pass
    return float(2 ** (attempt - 1))


def validate_response(raw: dict, source_ids: set[str], response_model: type[BaseModel] = EvidenceSummaryData) -> BaseModel:
    try:
        choice = raw["choices"][0]
        if choice["finish_reason"] == "length":
            raise ReportRunError("MODEL_OUTPUT_TRUNCATED")
        if choice["finish_reason"] != "stop" or choice["message"].get("refusal"):
            raise ReportRunError("MODEL_RESPONSE_INVALID")
        text = choice["message"]["content"]
        if not isinstance(text, str) or len(text.encode("utf-8")) > 128 * 1024:
            raise ReportRunError("MODEL_RESPONSE_INVALID")
        result = response_model.model_validate_json(text)
    except (KeyError, IndexError, TypeError, ValidationError):
        raise ReportRunError("MODEL_RESPONSE_INVALID") from None
    if isinstance(result, EvidenceSummaryData):
        try:
            result.validate_source_ids(source_ids)
        except ValueError:
            raise ReportRunError("MODEL_SOURCE_REFERENCE_INVALID") from None
        if not result.summary_items and not result.verification_items:
            raise ReportRunError("MODEL_RESPONSE_INVALID")
    return result


class AuditedModel:
    def __init__(self, runs: ReportRunStore, snapshots: SnapshotStore, *, transport: ModelTransport,
                 settings: ModelRequestSettings, limits: RecoverySettings, deadline: Deadline,
                 sleeper: Callable[[float], None] = sleep):
        self.runs = runs
        self.snapshots = snapshots
        self.transport = transport
        self.settings = settings
        self.limits = limits
        self.deadline = deadline
        self.sleeper = sleeper

    def generate(self, run_id: str, batch: PlannedBatch, *, synthetic_check: bool = False,
                 response_model: type[BaseModel] = EvidenceSummaryData) -> BaseModel:
        self.deadline.remaining()
        if not synthetic_check:
            check_capabilities(self.settings, message_bytes=batch.message_bytes, max_tokens=batch.max_tokens)
        elif batch.message_bytes > self.settings.max_request_utf8_bytes:
            raise ReportRunError("MODEL_REQUEST_BYTES_EXCEEDED")
        run = self.runs.get(run_id)
        if synthetic_check and run["run_kind"] != "model_check":
            raise ReportRunError("SYNTHETIC_CHECK_RUN_REQUIRED")
        request = batch.request()
        request_hash = fingerprint({"generation_id": run["generation_id"], "request": request,
                                    "config_fingerprint": run["config_fingerprint"]})
        prior = self.runs.successful_call(run["generation_id"], request_hash)
        if prior:
            stored = self.snapshots.read(SnapshotRef(prior["response_path"], prior["response_sha256"]))
            return validate_response(stored["response"], set(batch.source_ids), response_model)
        reference = self.snapshots.put(run_id, "request", request)
        while True:
            self.deadline.remaining()
            previous = self.runs.calls(run_id, batch.logical_step)
            terminal_codes = {row["error_code"] for row in previous if row["status"] == "failed"}
            validation_failures = sum(row["error_code"] in {"MODEL_RESPONSE_INVALID", "MODEL_OUTPUT_TRUNCATED",
                                      "MODEL_SOURCE_REFERENCE_INVALID"} for row in previous)
            if validation_failures >= 2:
                raise ReportRunError("MODEL_VALIDATION_BUDGET_EXHAUSTED")
            if terminal_codes - {"MODEL_RESPONSE_INVALID", "MODEL_OUTPUT_TRUNCATED", "MODEL_SOURCE_REFERENCE_INVALID",
                                  "MODEL_TIMEOUT", "MODEL_CONNECTION_FAILED", "MODEL_HTTP_408", "MODEL_HTTP_429"}:
                if any(not (code and code.startswith("MODEL_HTTP_5")) for code in terminal_codes
                       if code not in {"MODEL_RESPONSE_INVALID", "MODEL_OUTPUT_TRUNCATED", "MODEL_SOURCE_REFERENCE_INVALID",
                                       "MODEL_TIMEOUT", "MODEL_CONNECTION_FAILED", "MODEL_HTTP_408", "MODEL_HTTP_429"}):
                    raise ReportRunError("MODEL_PERMANENT_FAILURE_REQUIRES_NEW_RUN")
            prepare = getattr(self.transport, "prepare", None)
            if prepare is not None:
                prepare()
            call = self.runs.reserve_call(run_id, request_fingerprint=request_hash, logical_step=batch.logical_step,
                                         request=reference, limits=self.limits, max_attempts=self.settings.max_retries)
            started = monotonic()
            response = None
            usage = None
            try:
                response = self.transport.send(request, timeout=min(self.settings.request_timeout_seconds, self.deadline.remaining()))
                if isinstance(response.get("usage"), dict):
                    usage = response["usage"]
                result = validate_response(response, set(batch.source_ids), response_model)
            except KeyboardInterrupt:
                self.runs.finish_call(call["call_id"], status="unknown", error_code="CALL_INTERRUPTED",
                                      duration_ms=int((monotonic() - started) * 1000))
                raise
            except Exception as error:
                code, retryable = _classify_failure(error)
                self.runs.finish_call(call["call_id"], status="failed", usage=usage, error_code=code,
                                      duration_ms=int((monotonic() - started) * 1000))
                if not retryable or call["attempt"] >= self.settings.max_retries:
                    raise ReportRunError(code) from None
                if code in {"MODEL_RESPONSE_INVALID", "MODEL_OUTPUT_TRUNCATED", "MODEL_SOURCE_REFERENCE_INVALID"} and validation_failures >= 1:
                    raise ReportRunError(code) from None
                delay = _retry_delay(error, call["attempt"], datetime.now(UTC))
                if delay >= self.deadline.remaining():
                    raise ReportRunError("RUN_TIME_BUDGET_EXHAUSTED") from None
                self.sleeper(delay)
                continue
            # If persistence fails, the reservation remains unknown for recovery.
            # A database error must never retry a request that already succeeded.
            response_ref = self.snapshots.put(run_id, "response", {"response": response})
            self.runs.finish_call(call["call_id"], status="succeeded", response=response_ref, usage=usage,
                                  duration_ms=int((monotonic() - started) * 1000))
            return result
