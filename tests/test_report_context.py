"""Business dates stay stable across timezones, retries, and midnight."""

from dataclasses import FrozenInstanceError, replace
from datetime import UTC, date, datetime, timedelta
import json

import pytest

from src.services.report_runner.context import RunContext, parse_report_date, resolve_target_date


@pytest.mark.parametrize(
    ("clock", "scheduled", "expected"),
    [
        ("2026-10-12T00:00:00Z", False, date(2026, 10, 11)),
        ("2026-10-12T00:59:59Z", True, date(2026, 10, 10)),
        ("2026-10-12T01:00:00Z", True, date(2026, 10, 11)),
        ("2026-01-01T01:00:00Z", True, date(2025, 12, 31)),
        ("2028-03-01T01:00:00Z", True, date(2028, 2, 29)),
    ],
)
def test_target_date_at_delivery_boundary(clock, scheduled, expected):
    assert resolve_target_date(datetime.fromisoformat(clock), scheduled=scheduled) == expected


def test_utc_window_matches_shanghai_midnights():
    context = RunContext.create(now=datetime(2026, 10, 12, 1, tzinfo=UTC))
    assert context.window_start_utc == datetime(2026, 10, 10, 16, tzinfo=UTC)
    assert context.window_end_utc == datetime(2026, 10, 11, 16, tzinfo=UTC)
    assert context.target_date == date(2026, 10, 11)


def test_restored_context_does_not_recompute_date_after_midnight():
    original = RunContext.create(now=datetime(2026, 10, 12, 15, 59, tzinfo=UTC))
    restored = RunContext.from_dict(json.loads(json.dumps(original.to_dict())))
    assert restored == original
    with pytest.raises(FrozenInstanceError):
        restored.target_date = date(2026, 10, 12)


def test_inconsistent_persisted_window_is_rejected():
    context = RunContext.create(now=datetime(2026, 10, 12, 1, tzinfo=UTC))
    with pytest.raises(ValueError, match="window must match"):
        replace(context, window_end_utc=context.window_end_utc + timedelta(hours=1))


def test_naive_clock_and_unfinished_day_are_rejected():
    with pytest.raises(ValueError, match="timezone-aware"):
        RunContext.create(now=datetime(2026, 10, 12, 9))
    with pytest.raises(ValueError, match="completed calendar"):
        RunContext.create(
            now=datetime(2026, 10, 12, 1, tzinfo=UTC), target_date=date(2026, 10, 12)
        )


def test_scheduled_and_explicit_date_are_mutually_exclusive():
    with pytest.raises(ValueError, match="cannot override"):
        RunContext.create(
            now=datetime(2026, 10, 12, 1, tzinfo=UTC),
            target_date=date(2026, 10, 10),
            scheduled=True,
        )


@pytest.mark.parametrize("value", ["2026-1-02", "2026-02-30", "today", "9999-12-31"])
def test_report_date_requires_a_real_iso_day(value):
    with pytest.raises(ValueError):
        parse_report_date(value)
