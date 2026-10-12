"""A frozen business date and absolute window, shared by every run phase."""

from __future__ import annotations

from dataclasses import asdict, dataclass
from datetime import UTC, date, datetime, time, timedelta
import re
from uuid import uuid4
from zoneinfo import ZoneInfo


def parse_report_date(value: str) -> date:
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", value):
        raise ValueError("target date must use YYYY-MM-DD")
    result = date.fromisoformat(value)
    if result == date.max:
        raise ValueError("target date must allow a following midnight")
    return result


def utc_window(target: date, timezone: str) -> tuple[datetime, datetime]:
    zone = ZoneInfo(timezone)
    start = datetime.combine(target, time.min, zone)
    end = datetime.combine(target + timedelta(days=1), time.min, zone)
    return start.astimezone(UTC), end.astimezone(UTC)


def resolve_target_date(
    now: datetime,
    *,
    timezone: str = "Asia/Shanghai",
    target_date: date | None = None,
    scheduled: bool = False,
    scheduled_time: str = "09:00",
) -> date:
    if now.tzinfo is None or now.utcoffset() is None:
        raise ValueError("run clock must be timezone-aware")
    if scheduled and target_date is not None:
        raise ValueError("scheduled runs cannot override the target date")
    local = now.astimezone(ZoneInfo(timezone))
    if target_date is not None:
        if target_date >= local.date():
            raise ValueError("target date must be a completed calendar day")
        return target_date
    days = 1
    if scheduled and local.time() < time.fromisoformat(scheduled_time):
        days = 2
    return local.date() - timedelta(days=days)


@dataclass(frozen=True, slots=True)
class RunContext:
    run_id: str
    scope_id: str
    target_date: date
    timezone: str
    window_start_utc: datetime
    window_end_utc: datetime
    started_at: datetime
    config_fingerprint: str

    def __post_init__(self) -> None:
        expected = utc_window(self.target_date, self.timezone)
        actual = (self.window_start_utc, self.window_end_utc)
        if actual != expected or any(value.utcoffset() != timedelta(0) for value in actual):
            raise ValueError("run window must match the business date in UTC")
        if self.started_at.tzinfo is None or self.started_at.utcoffset() is None:
            raise ValueError("run start must be timezone-aware")
        if not re.fullmatch(r"[0-9a-f]{32}", self.run_id):
            raise ValueError("run identity must be a UUID hex value")
        if self.scope_id != "personal_work" or self.timezone != "Asia/Shanghai":
            raise ValueError("only the configured personal_work Shanghai scope is supported")

    @classmethod
    def create(
        cls,
        *,
        now: datetime,
        scope_id: str = "personal_work",
        timezone: str = "Asia/Shanghai",
        target_date: date | None = None,
        scheduled: bool = False,
        scheduled_time: str = "09:00",
        config_fingerprint: str = "",
    ) -> RunContext:
        target = resolve_target_date(
            now,
            timezone=timezone,
            target_date=target_date,
            scheduled=scheduled,
            scheduled_time=scheduled_time,
        )
        start, end = utc_window(target, timezone)
        return cls(
            run_id=uuid4().hex,
            scope_id=scope_id,
            target_date=target,
            timezone=timezone,
            window_start_utc=start,
            window_end_utc=end,
            started_at=now.astimezone(UTC),
            config_fingerprint=config_fingerprint,
        )

    def to_dict(self) -> dict[str, str]:
        return {
            key: value.isoformat() if isinstance(value, (date, datetime)) else value
            for key, value in asdict(self).items()
        }

    @classmethod
    def from_dict(cls, value: dict[str, str]) -> RunContext:
        fields = dict(value)
        fields["target_date"] = parse_report_date(fields["target_date"])
        for name in ("window_start_utc", "window_end_utc", "started_at"):
            fields[name] = datetime.fromisoformat(fields[name])
        return cls(**fields)
