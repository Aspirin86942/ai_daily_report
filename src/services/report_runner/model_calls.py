"""Single retry owner with persisted budgets, bounded requests and reusable validated responses."""

from __future__ import annotations

from datetime import UTC, datetime
from email.utils import parsedate_to_datetime
import json
import os
from time import monotonic, sleep
from typing import Callable, Protocol

from pydantic import BaseModel, ValidationError

from ...models.evidence_report import EvidenceSummaryData
from ...models.report_settings import ModelPricing, ModelRequestSettings, RecoverySettings
from ...models.model_budget import BudgetReservation, cost_microunits
from ...models.model_recovery import FrozenModelCall
from ..report_run_store import ReportRunStore
from .errors import ReportRunError
from .model_plan import PlannedBatch, check_capabilities, reduce_truncated_batch
from .snapshots import SnapshotRef, SnapshotStore, fingerprint
from .model_slicing import batch_entries, batch_ranges, is_prefix_revision

VALIDATION_FAILURES = {"MODEL_RESPONSE_INVALID", "MODEL_OUTPUT_TRUNCATED",
    "MODEL_SOURCE_REFERENCE_INVALID"}
RETRYABLE_FAILURES = VALIDATION_FAILURES | {
    "MODEL_TIMEOUT", "MODEL_CONNECTION_FAILED", "MODEL_HTTP_408", "MODEL_HTTP_429"}


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
        sdk_request = dict(request)
        thinking = sdk_request.pop("thinking", None)
        nested_body = sdk_request.pop("extra_body", None)
        vendor_fields = {}
        if thinking is not None:
            vendor_fields["thinking"] = thinking
        if nested_body is not None:
            vendor_fields["extra_body"] = nested_body
        if vendor_fields:
            sdk_request["extra_body"] = vendor_fields
        response = self.client.chat.completions.create(**sdk_request, timeout=timeout)
        return response.model_dump(mode="json")


def _classify_failure(error: Exception) -> tuple[str, bool]:
    if isinstance(error, ReportRunError):
        return error.code, error.code in VALIDATION_FAILURES
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


def freeze_model_call(run: dict, call: dict) -> FrozenModelCall:
    return FrozenModelCall.from_record(run, call, row_sha256=fingerprint(call))


class AuditedModel:
    def __init__(self, runs: ReportRunStore, snapshots: SnapshotStore, *, transport: ModelTransport,
                 settings: ModelRequestSettings, limits: RecoverySettings, deadline: Deadline,
                 sleeper: Callable[[float], None] = sleep, output_tokens_upper: int | None = None,
                 frozen_pricing: ModelPricing | None = None, job_budget: dict | None = None,
                 reused_calls: dict[str, dict] | None = None, prior_calls: list[dict] | None = None):
        self.runs = runs
        self.snapshots = snapshots
        self.transport = transport
        self.settings = settings
        self.limits = limits
        self.deadline = deadline
        self.sleeper = sleeper
        self.output_tokens_upper = output_tokens_upper
        self.pricing = frozen_pricing if frozen_pricing is not None else limits.model_pricing
        self.job_budget = job_budget or {}
        try:
            self.reused_calls = {key: FrozenModelCall.model_validate(value) for key, value in (reused_calls or {}).items()}
            self.prior_calls = [FrozenModelCall.model_validate(value) for value in prior_calls or []]
            if any(key != value.logical_step for key, value in self.reused_calls.items()):
                raise ValueError("reuse map must retain the logical step")
        except (ValidationError, ValueError, TypeError):
            raise ReportRunError("MODEL_REUSE_REFERENCE_INVALID") from None

    def _pinned_call(self, run_id: str, pin: FrozenModelCall) -> tuple[dict, dict]:
        current = self.runs.get(run_id)
        parent = self.runs.get(pin.run_id)
        prior = next((row for row in self.runs.calls(pin.run_id) if row["call_id"] == pin.call_id), None)
        if (prior is None or fingerprint(prior) != pin.row_sha256
                or parent["generation_id"] != pin.generation_id
                or parent["config_fingerprint"] != pin.config_fingerprint
                or current["scope_id"] != parent["scope_id"] or current["target_date"] != parent["target_date"]
                or any(prior[key] != getattr(pin, key) for key in ("run_id", "generation_id", "logical_step", "status",
                    "request_fingerprint", "request_path", "request_sha256", "response_path", "response_sha256"))):
            raise ReportRunError("MODEL_REUSE_CALL_MISMATCH")
        prior_job = json.loads(prior["usage_json"] or "{}").get("reserved_budget", {}).get("job_id")
        if prior_job != self.job_budget.get("job_id"):
            raise ReportRunError("MODEL_REUSE_JOB_MISMATCH")
        return parent, prior

    def audited_calls(self, run_id: str) -> list[dict]:
        rows = self.runs.calls(run_id)
        seen = {row["call_id"] for row in rows}
        for pin in [*self.reused_calls.values(), *self.prior_calls]:
            _, prior = self._pinned_call(run_id, pin)
            if prior["call_id"] not in seen:
                rows.append(prior)
                seen.add(prior["call_id"])
        return sorted(rows, key=lambda row: row["call_id"])

    @staticmethod
    def _request_fingerprint(run: dict, batch: PlannedBatch) -> str:
        identity = {"generation_id": run["generation_id"], "request": batch.request(),
                    "config_fingerprint": run["config_fingerprint"]}
        if batch.response_schema_version == 2:
            identity["response_schema_sha256"] = batch.response_schema_sha256
        return fingerprint(identity)

    def effective_batch(self, run_id: str, root: PlannedBatch) -> tuple[PlannedBatch, SnapshotRef | None]:
        """Recover a frozen revision from its reservation; never rewrite the root input."""
        if root.response_schema_version == 1 or root.revision:
            return root, None
        run = self.runs.get(run_id)
        parent = self._request_fingerprint(run, root)
        for call in reversed(self.runs.calls(run_id, root.logical_step)):
            audit = json.loads(call["usage_json"] or "{}").get("reserved_budget", {}).get("request_audit", {})
            if audit.get("revision") != 1:
                continue
            reference = SnapshotRef(**audit["plan_revision"])
            value = self.snapshots.read(reference)
            batch = PlannedBatch.model_validate(value["batch"])
            if (value.get("version") != 1 or batch.parent_request_fingerprint != parent
                    or batch.logical_step != root.logical_step or batch.model != root.model
                    or batch.temperature != root.temperature or batch.max_tokens != root.max_tokens
                    or batch.thinking_mode != root.thinking_mode
                    or batch.thinking_parameter_location != root.thinking_parameter_location
                    or batch.revision != 1 or batch.message_bytes > root.message_bytes // 2
                    or self._request_fingerprint(run, batch) != call["request_fingerprint"]):
                raise ReportRunError("MODEL_PLAN_REVISION_MISMATCH")
            original = batch_entries(root)
            revised = batch_entries(batch)
            if batch.input_range_version != root.input_range_version or any(
                    not any(is_prefix_revision(item, prior) for prior in original) for item in revised):
                raise ReportRunError("MODEL_PLAN_REVISION_MISMATCH")
            return batch, reference
        return root, None

    def _revised_batch(self, run_id: str, root: PlannedBatch) -> tuple[PlannedBatch, SnapshotRef]:
        if self.settings.truncation_policy != "reduce_once" or self.settings.max_strategy_changes == 0:
            raise ReportRunError("MODEL_OUTPUT_TRUNCATED")
        batch = reduce_truncated_batch(root, parent_request_fingerprint=self._request_fingerprint(self.runs.get(run_id), root))
        reference = self.snapshots.put(run_id, "plan-revision", {"version": 1, "strategy": "reduce_once_v1",
            "failure_code": "MODEL_OUTPUT_TRUNCATED", "batch": batch.model_dump(mode="json")})
        return batch, reference

    def _budget(self, batch: PlannedBatch, revision: SnapshotRef | None) -> BudgetReservation:
        input_upper = batch.message_bytes + 1024
        output_upper = batch.max_tokens
        legacy_upper = self.settings.max_request_utf8_bytes + 1024 + max(output_upper, self.output_tokens_upper or 0)
        worst_cost = cost_microunits(self.settings.max_request_utf8_bytes + 1024,
                                   max(output_upper, self.output_tokens_upper or 0), self.pricing) if self.pricing else None
        return BudgetReservation(input_tokens_upper=input_upper, output_tokens_upper=output_upper,
            legacy_tokens_upper=legacy_upper, pricing=self.pricing,
            cost_upper_microunits=cost_microunits(input_upper, output_upper, self.pricing) if self.pricing else None,
            run_tokens_limit=self.limits.max_tokens_per_run or self.limits.max_calls_per_run * legacy_upper,
            day_tokens_limit=self.limits.max_tokens_per_calendar_day or self.limits.max_calls_per_calendar_day * legacy_upper,
            run_cost_limit_microunits=(self.limits.max_cost_per_run_microunits
                if self.limits.max_cost_per_run_microunits is not None else self.limits.max_calls_per_run * worst_cost
                if worst_cost is not None and worst_cost > 0 else None),
            day_cost_limit_microunits=(self.limits.max_cost_per_calendar_day_microunits
                if self.limits.max_cost_per_calendar_day_microunits is not None else self.limits.max_calls_per_calendar_day * worst_cost
                if worst_cost is not None and worst_cost > 0 else None),
            request_audit={"response_schema_version": batch.response_schema_version,
                "input_range_version": batch.input_range_version,
                "source_ranges": batch_ranges(batch),
                "response_schema_sha256": batch.response_schema_sha256, "revision": batch.revision,
                "parent_request_fingerprint": batch.parent_request_fingerprint,
                "plan_revision": {"path": revision.path, "sha256": revision.sha256} if revision else None,
                **({"job_calls_revision_sha256": self.job_budget["job_calls_revision_sha256"]}
                    if self.job_budget.get("job_calls_revision_sha256") is not None else {})},
            **{key: value for key, value in self.job_budget.items() if key != "job_calls_revision_sha256"})

    @staticmethod
    def _response_usage(response: dict | None, usage: dict | None, batch: PlannedBatch) -> dict:
        result = dict(usage or {})
        choices = response.get("choices") if isinstance(response, dict) else None
        choice = choices[0] if isinstance(choices, list) and choices else None
        finish = choice.get("finish_reason") if isinstance(choice, dict) else None
        result["response_audit"] = {"version": 1, "finish_reason": finish if isinstance(finish, str) and finish in {
            "stop", "length", "content_filter", "tool_calls", "function_call"} else None,
            "message_bytes": batch.message_bytes, "max_tokens": batch.max_tokens,
            "response_schema_version": batch.response_schema_version, "revision": batch.revision}
        return result

    def cached_result(self, run_id: str, batch: PlannedBatch, *,
                      response_model: type[BaseModel] | None = None) -> BaseModel | None:
        """Validate frozen local evidence before touching transport or send budgets."""
        run = self.runs.get(run_id)
        batch, _ = self.effective_batch(run_id, batch)
        request = batch.request()
        pin = self.reused_calls.get(batch.logical_step)
        if pin is not None:
            parent, prior = self._pinned_call(run_id, pin)
            if (prior["status"] != "succeeded" or not prior["response_path"]
                    or prior["request_fingerprint"] != self._request_fingerprint(parent, batch)):
                raise ReportRunError("MODEL_REUSE_NOT_VALIDATED")
            frozen_request = self.snapshots.read(SnapshotRef(prior["request_path"], prior["request_sha256"]))
            audit = json.loads(prior["usage_json"] or "{}").get("reserved_budget", {}).get("request_audit", {})
            if (frozen_request != request or audit.get("source_ranges") != batch_ranges(batch)
                    or audit.get("response_schema_sha256") != batch.response_schema_sha256):
                raise ReportRunError("MODEL_REUSE_REQUEST_MISMATCH")
            stored = self.snapshots.read(SnapshotRef(prior["response_path"], prior["response_sha256"]))
            if not isinstance(stored, dict) or not isinstance(stored.get("response"), dict):
                raise ReportRunError("SNAPSHOT_RESPONSE_INVALID")
            return validate_response(stored["response"], set(batch.source_ids), response_model or batch.response_model)
        request_hash = self._request_fingerprint(run, batch)
        prior = self.runs.successful_call(run["generation_id"], request_hash)
        if not prior or prior["logical_step"] != batch.logical_step:
            return None
        frozen_request = self.snapshots.read(SnapshotRef(prior["request_path"], prior["request_sha256"]))
        if frozen_request != request:
            raise ReportRunError("SNAPSHOT_REQUEST_MISMATCH")
        stored = self.snapshots.read(SnapshotRef(prior["response_path"], prior["response_sha256"]))
        if not isinstance(stored, dict) or not isinstance(stored.get("response"), dict):
            raise ReportRunError("SNAPSHOT_RESPONSE_INVALID")
        return validate_response(stored["response"], set(batch.source_ids), response_model or batch.response_model)

    def check_batch(self, batch: PlannedBatch) -> None:
        check_capabilities(self.settings, message_bytes=batch.request_bytes, max_tokens=batch.max_tokens)

    def generate(self, run_id: str, batch: PlannedBatch, *, synthetic_check: bool = False,
                 response_model: type[BaseModel] | None = None) -> BaseModel:
        self.deadline.remaining()
        run = self.runs.get(run_id)
        if synthetic_check and run["run_kind"] != "model_check":
            raise ReportRunError("SYNTHETIC_CHECK_RUN_REQUIRED")
        cached = self.cached_result(run_id, batch, response_model=response_model)
        if cached is not None:
            return cached
        root = batch
        batch, revision = self.effective_batch(run_id, root)
        if not batch.revision and any(call["error_code"] == "MODEL_OUTPUT_TRUNCATED"
                                      for call in self.runs.calls(run_id, batch.logical_step)):
            batch, revision = self._revised_batch(run_id, root)
        if not synthetic_check:
            self.check_batch(batch)
        elif batch.request_bytes > self.settings.max_request_utf8_bytes:
            raise ReportRunError("MODEL_REQUEST_BYTES_EXCEEDED")
        request = batch.request()
        request_hash = self._request_fingerprint(run, batch)
        reference = self.snapshots.put(run_id, "request", request)
        while True:
            self.deadline.remaining()
            previous = self.runs.calls(run_id, batch.logical_step)
            terminal_codes = {row["error_code"] for row in previous if row["status"] == "failed"}
            validation_failures = sum(row["error_code"] in VALIDATION_FAILURES for row in previous)
            if validation_failures >= 2:
                raise ReportRunError("MODEL_VALIDATION_BUDGET_EXHAUSTED")
            if terminal_codes - RETRYABLE_FAILURES:
                if any(not (code and code.startswith("MODEL_HTTP_5")) for code in terminal_codes
                       if code not in RETRYABLE_FAILURES):
                    raise ReportRunError("MODEL_PERMANENT_FAILURE_REQUIRES_NEW_RUN")
            prepare = getattr(self.transport, "prepare", None)
            if prepare is not None:
                prepare()
            call = self.runs.reserve_call(run_id, request_fingerprint=request_hash, logical_step=batch.logical_step,
                                         request=reference, limits=self.limits, max_attempts=self.settings.max_retries,
                                         budget=self._budget(batch, revision))
            started = monotonic()
            response = None
            usage = None
            try:
                response = self.transport.send(request, timeout=min(self.settings.request_timeout_seconds, self.deadline.remaining()))
                if isinstance(response, dict) and isinstance(response.get("usage"), dict):
                    usage = response["usage"]
                result = validate_response(response, set(batch.source_ids), response_model or batch.response_model)
            except KeyboardInterrupt:
                self.runs.finish_call(call["call_id"], status="unknown", error_code="CALL_INTERRUPTED",
                                      duration_ms=int((monotonic() - started) * 1000))
                raise
            except Exception as error:
                code, retryable = _classify_failure(error)
                failed_ref = self.snapshots.put(run_id, "failed-response", {"response": response}) if response is not None else None
                self.runs.finish_call(call["call_id"], status="failed", response=failed_ref,
                                      usage=self._response_usage(response, usage, batch), error_code=code,
                                      duration_ms=int((monotonic() - started) * 1000))
                if not retryable or call["attempt"] >= self.settings.max_retries:
                    raise ReportRunError(code) from None
                if code in VALIDATION_FAILURES and validation_failures >= 1:
                    raise ReportRunError(code) from None
                if code == "MODEL_OUTPUT_TRUNCATED":
                    batch, revision = self._revised_batch(run_id, batch)
                    if not synthetic_check:
                        self.check_batch(batch)
                    request = batch.request()
                    request_hash = self._request_fingerprint(run, batch)
                    reference = self.snapshots.put(run_id, "request", request)
                    continue
                delay = _retry_delay(error, call["attempt"], datetime.now(UTC))
                if delay >= self.deadline.remaining():
                    raise ReportRunError("RUN_TIME_BUDGET_EXHAUSTED") from None
                self.sleeper(delay)
                continue
            # If persistence fails, the reservation remains unknown for recovery.
            # A database error must never retry a request that already succeeded.
            response_ref = self.snapshots.put(run_id, "response", {"response": response})
            self.runs.finish_call(call["call_id"], status="succeeded", response=response_ref,
                                  usage=self._response_usage(response, usage, batch),
                                  duration_ms=int((monotonic() - started) * 1000))
            return result
