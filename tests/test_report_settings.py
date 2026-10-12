from types import SimpleNamespace
from pathlib import Path

import yaml

import pytest
from pydantic import ValidationError

from src.core.config import Config
from src.models.report_settings import ModelRequestSettings, RecoverySettings, RetentionSettings


def test_old_settings_get_explicit_automation_defaults():
    config = object.__new__(Config)
    config._settings = SimpleNamespace(llm=SimpleNamespace())
    assert config.report_settings.timezone == "Asia/Shanghai"
    assert config.schedule_settings.local_time == "09:00"
    assert config.recovery_settings.max_calls_per_run == 12
    assert config.retention_settings.snapshot_max_mib == 512
    assert config.model_request_settings.context_window_tokens is None


def test_section_overrides_and_unknown_fields_are_validated():
    config = object.__new__(Config)
    config._settings = SimpleNamespace(report={"notes_dir": "custom/notes"})
    assert config.report_settings.notes_dir == "custom/notes"
    config._settings = SimpleNamespace(report={"timezne": "Asia/Shanghai"})
    with pytest.raises(ValidationError):
        config.report_settings


def test_invalid_budget_and_retention_are_rejected():
    with pytest.raises(ValidationError):
        RecoverySettings(max_calls_per_run=31, max_calls_per_calendar_day=30)
    with pytest.raises(ValidationError):
        RetentionSettings(input_snapshot_days=31, terminal_run_days=30)
    with pytest.raises(ValidationError):
        ModelRequestSettings(request_timeout_seconds=0)


def test_example_uses_valid_complete_report_sections():
    settings = yaml.safe_load((Path(__file__).parents[1] / "config/settings.example.yaml").read_text(encoding="utf-8"))
    config = object.__new__(Config)
    config._settings = SimpleNamespace(**settings)
    assert config.report_settings.timezone == "Asia/Shanghai"
    assert config.schedule_settings.logon_catchup
    assert config.retention_settings.input_snapshot_days == 14
    assert config.recovery_settings.max_calls_per_run == 12
    assert config.model_request_settings.request_timeout_seconds == 120
