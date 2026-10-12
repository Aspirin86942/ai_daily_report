"""The absolute scan window wins over the host's local calendar."""

from datetime import UTC, date, datetime
import json
from pathlib import Path

import pytest

from src.models.scanner_contract import BuildContextRequest
from src.services.native_scanner import ScanRequest


def test_native_request_carries_exclusive_utc_end():
    request = ScanRequest(
        "daily", date(2026, 10, 11), date(2026, 10, 11),
        window_start_utc=datetime(2026, 10, 10, 16, tzinfo=UTC),
        window_end_utc=datetime(2026, 10, 11, 16, tzinfo=UTC),
    )
    assert request.to_native()["window_end_utc"] == "2026-10-11T16:00:00+00:00"


@pytest.mark.parametrize("start,end", [
    (datetime(2026, 10, 10, 16, tzinfo=UTC), None),
    (None, datetime(2026, 10, 11, 16, tzinfo=UTC)),
    (datetime(2026, 10, 10), datetime(2026, 10, 11)),
    (datetime(2026, 10, 11, tzinfo=UTC), datetime(2026, 10, 11, tzinfo=UTC)),
])
def test_native_rejects_ambiguous_or_empty_window(start, end):
    request = ScanRequest("daily", date(2026, 10, 11), date(2026, 10, 11),
                          window_start_utc=start, window_end_utc=end)
    with pytest.raises(ValueError):
        request.to_native()


def test_legacy_native_request_keeps_the_original_shape():
    assert set(ScanRequest("daily", date(2026, 1, 1), date(2026, 1, 1)).to_native()) == {
        "report_mode", "start_date", "end_date", "compression_profile",
    }


def test_absolute_window_contract_rejects_partial_naive_and_reverse():
    fixture = Path(__file__).parent / "fixtures/scanner_contract/v1/request-windows-unc.json"
    base = json.loads(fixture.read_text(encoding="utf-8"))
    good = {**base, "window_start_utc": "2026-10-10T16:00:00Z",
            "window_end_utc": "2026-10-11T16:00:00+00:00"}
    BuildContextRequest.model_validate(good)
    for overrides in [
        {"window_end_utc": None},
        {"window_start_utc": "2026-10-10T16:00:00"},
        {"window_start_utc": "2026-10-12T16:00:00Z"},
        {"window_start_utc": "2026-10-10T16:00:00+08:00"},
    ]:
        with pytest.raises(ValueError):
            BuildContextRequest.model_validate({**good, **overrides})
