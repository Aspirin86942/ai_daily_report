"""Conservative model reservations, using integer tokens and currency microunits."""

from __future__ import annotations

from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator

from .report_settings import ModelPricing


def cost_microunits(input_tokens: int, output_tokens: int, pricing: ModelPricing) -> int:
    numerator = (input_tokens * pricing.input_per_million_microunits
                 + output_tokens * pricing.output_per_million_microunits)
    return (numerator + 999_999) // 1_000_000


def usage_tokens(usage: dict | None) -> tuple[int | None, int | None, int | None]:
    def number(key, alias=None):
        value = (usage or {}).get(key, (usage or {}).get(alias) if alias else None)
        return value if type(value) is int and value >= 0 else None

    incoming, outgoing = number("prompt_tokens", "input_tokens"), number("completion_tokens", "output_tokens")
    total = number("total_tokens")
    if incoming is not None and outgoing is not None:
        total = max(total or 0, incoming + outgoing)
    return incoming, outgoing, total


class BudgetReservation(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True, frozen=True)
    version: Literal[1] = 1
    input_tokens_upper: int = Field(ge=0)
    output_tokens_upper: int = Field(ge=1)
    legacy_tokens_upper: int = Field(ge=1)
    pricing: ModelPricing | None = None
    cost_upper_microunits: int | None = Field(default=None, ge=0)
    run_tokens_limit: int = Field(ge=1)
    day_tokens_limit: int = Field(ge=1)
    run_cost_limit_microunits: int | None = Field(default=None, ge=0)
    day_cost_limit_microunits: int | None = Field(default=None, ge=0)
    job_id: str | None = Field(default=None, pattern=r"^[a-zA-Z0-9_-]{1,128}$")
    job_tokens_limit: int | None = Field(default=None, ge=1)
    job_calls_limit: int | None = Field(default=None, ge=0)
    job_cost_limit_microunits: int | None = Field(default=None, ge=0)
    request_audit: dict[str, Any] = Field(default_factory=dict)

    @property
    def token_upper(self) -> int:
        return self.input_tokens_upper + self.output_tokens_upper

    @model_validator(mode="after")
    def validate_cost_and_scope(self):
        if self.legacy_tokens_upper < self.token_upper:
            raise ValueError("legacy upper bound cannot be smaller than this request")
        expected = cost_microunits(self.input_tokens_upper, self.output_tokens_upper, self.pricing) if self.pricing else None
        if self.cost_upper_microunits != expected:
            raise ValueError("reservation cost must match frozen rates")
        if self.pricing is None and any(value is not None for value in (
                self.run_cost_limit_microunits, self.day_cost_limit_microunits, self.job_cost_limit_microunits)):
            raise ValueError("money reservations require pricing")
        if self.job_id is None and (self.job_tokens_limit is not None or self.job_cost_limit_microunits is not None
                or self.job_calls_limit is not None or self.request_audit.get("job_calls_revision_sha256") is not None):
            raise ValueError("job budgets require an explicit job identity")
        return self


class JobCallsRevision(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True, frozen=True)
    version: Literal[1] = 1
    job_id: str = Field(pattern=r"^[a-zA-Z0-9_-]{1,128}$")
    plan_sha256: str = Field(pattern=r"^[0-9a-f]{64}$")
    previous_calls_limit: int = Field(ge=1)
    calls_limit: int = Field(ge=1)
    tokens_limit: int = Field(ge=1)
    cost_limit_microunits: int | None = Field(ge=0)
    pricing: ModelPricing | None
    retained_call_ids: list[int] = Field(min_length=1, max_length=10000)
    retained_calls_sha256: str = Field(pattern=r"^[0-9a-f]{64}$")
    authorization_note: str = Field(min_length=1, max_length=1000)

    @model_validator(mode="after")
    def validate_increase(self):
        if self.calls_limit <= self.previous_calls_limit or len(set(self.retained_call_ids)) != len(self.retained_call_ids):
            raise ValueError("a call revision must explicitly increase the limit and retain distinct calls")
        return self


class JobBudgetRevision(JobCallsRevision):
    """An authorized append to a registered budget, retaining the original chain."""
    version: Literal[2] = 2
    previous_revision_sha256: str = Field(pattern=r"^[0-9a-f]{64}$")
    previous_tokens_limit: int = Field(ge=1)

    @model_validator(mode="after")
    def validate_tokens_increase(self):
        if self.tokens_limit < self.previous_tokens_limit:
            raise ValueError("an appended budget cannot reduce its token limit")
        return self


def charged_usage(row: dict, reservation: BudgetReservation, usage: dict) -> tuple[int, int | None]:
    """Unknown/in-flight calls retain their upper bound even if partial usage exists."""
    incoming, outgoing, total = usage_tokens(usage)
    known = row["status"] in {"succeeded", "failed"}
    tokens = total if known and total is not None else reservation.token_upper
    money = reservation.cost_upper_microunits
    if known and incoming is not None and outgoing is not None and reservation.pricing is not None:
        money = cost_microunits(incoming, outgoing, reservation.pricing)
        if total is not None and total > incoming + outgoing:
            extra = total - incoming - outgoing
            highest_rate = max(reservation.pricing.input_per_million_microunits,
                               reservation.pricing.output_per_million_microunits)
            money = max(reservation.cost_upper_microunits,
                        money + (extra * highest_rate + 999_999) // 1_000_000)
    return tokens, money
