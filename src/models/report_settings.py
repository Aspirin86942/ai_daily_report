"""Validated settings for scheduled evidence reports; credentials stay outside these models."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, ConfigDict, Field, model_validator


class SettingsModel(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True, str_strip_whitespace=True)


class ReportSettings(SettingsModel):
    scope_id: Literal["personal_work"] = "personal_work"
    root_alias: str = Field(default="work", pattern=r"^[a-zA-Z0-9_-]{1,64}$")
    timezone: Literal["Asia/Shanghai"] = "Asia/Shanghai"
    default_target: Literal["previous_calendar_day"] = "previous_calendar_day"
    notes_dir: str = "data/report_notes"


class ModelRequestSettings(SettingsModel):
    max_retries: int = Field(default=3, ge=1, le=3, strict=True)
    request_timeout_seconds: float = Field(default=120.0, gt=0, le=900)
    response_format: Literal["json_object"] = "json_object"
    thinking_mode: Literal["enabled", "disabled"] | None = None
    thinking_parameter_location: Literal["top_level", "extra_body"] | None = None
    max_request_utf8_bytes: int = Field(default=16000, ge=2048)
    max_sources_per_batch: int = Field(default=8, ge=1)
    max_batches: int = Field(default=8, ge=1)
    context_window_tokens: int | None = Field(default=None, ge=2048)
    verified_max_output_tokens: int | None = Field(default=None, ge=1)
    truncation_policy: Literal["reduce_once", "stop"] = "reduce_once"
    max_strategy_changes: int = Field(default=1, ge=0, le=1, strict=True)

    @model_validator(mode="after")
    def validate_thinking_location(self):
        if self.thinking_parameter_location is not None and self.thinking_mode is None:
            raise ValueError("an explicit thinking parameter location requires a thinking mode")
        return self

    def fingerprint_data(self, *, exclude: set[str] | None = None) -> dict:
        values = self.model_dump(exclude=exclude or set())
        if self.thinking_parameter_location is None:
            values.pop("thinking_parameter_location", None)
        return values


class ModelPricing(SettingsModel):
    rate_version: str = Field(min_length=1, max_length=128)
    currency: Literal["CNY", "USD"]
    input_per_million_microunits: int = Field(ge=0, strict=True)
    output_per_million_microunits: int = Field(ge=0, strict=True)


class RecoverySettings(SettingsModel):
    enforce_model_budgets: bool = True
    missed_days_policy: Literal["latest_due_day_with_gap_notice"] = (
        "latest_due_day_with_gap_notice"
    )
    max_calls_per_run: int = Field(default=12, ge=1)
    max_calls_per_calendar_day: int = Field(default=30, ge=1)
    run_budget_seconds: int = Field(default=900, ge=1)
    lock_wait_seconds: float = Field(default=5.0, ge=0, le=60)
    max_tokens_per_run: int | None = Field(default=None, ge=1, strict=True)
    max_tokens_per_calendar_day: int | None = Field(default=None, ge=1, strict=True)
    max_cost_per_run_microunits: int | None = Field(default=None, ge=0, strict=True)
    max_cost_per_calendar_day_microunits: int | None = Field(default=None, ge=0, strict=True)
    model_pricing: ModelPricing | None = None

    @model_validator(mode="after")
    def validate_budgets(self) -> RecoverySettings:
        if self.max_calls_per_calendar_day < self.max_calls_per_run:
            raise ValueError("daily call budget must be at least the per-run budget")
        if (self.max_tokens_per_run is not None and self.max_tokens_per_calendar_day is not None
                and self.max_tokens_per_calendar_day < self.max_tokens_per_run):
            raise ValueError("daily token budget must be at least the per-run budget")
        if (self.max_cost_per_run_microunits is not None and self.max_cost_per_calendar_day_microunits is not None
                and self.max_cost_per_calendar_day_microunits < self.max_cost_per_run_microunits):
            raise ValueError("daily cost budget must be at least the per-run budget")
        if self.model_pricing is None and (self.max_cost_per_run_microunits is not None
                                          or self.max_cost_per_calendar_day_microunits is not None):
            raise ValueError("cost budgets require verified pricing")
        return self


class RetentionSettings(SettingsModel):
    input_snapshot_days: int = Field(default=14, ge=1)
    terminal_run_days: int = Field(default=30, ge=1)
    snapshot_max_mib: int = Field(default=512, ge=1)

    @model_validator(mode="after")
    def validate_retention(self) -> RetentionSettings:
        if self.terminal_run_days < self.input_snapshot_days:
            raise ValueError("terminal runs must outlive their input snapshots")
        return self


class ScheduleSettings(SettingsModel):
    task_name: str = Field(default="AI Daily Report", min_length=1, max_length=128)
    local_time: str = Field(default="09:00", pattern=r"^(?:[01][0-9]|2[0-3]):[0-5][0-9]$")
    logon_catchup: bool = True
    wake_to_run: bool = False
    allow_on_battery: bool = True
    stop_on_battery: bool = False
    restart_count: int = Field(default=2, ge=0, le=10)
    restart_interval_minutes: int = Field(default=15, ge=1)
    execution_limit_minutes: int = Field(default=20, ge=1)
