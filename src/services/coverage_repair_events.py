"""Append-first, fsynced repair events and a replayable atomic progress view."""
from datetime import UTC, datetime
from hashlib import sha256
import json
import os
from pathlib import Path
from uuid import uuid4

from .report_runner.errors import ReportRunError
from .report_runner.snapshots import canonical_json, fingerprint, is_redirect


def write_atomic(path: Path, value: dict) -> None:
    if path.exists() and is_redirect(path):
        raise ReportRunError("COVERAGE_JOB_PATH_REDIRECTED")
    data = canonical_json(value) + b"\n"
    temporary = path.parent / (".pending-" + uuid4().hex)
    with temporary.open("xb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
    try:
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


class RepairEvents:
    """Call while holding the job lock; the event log is the progress authority."""

    def __init__(self, directory: Path, job_id: str, plan_sha256: str):
        self.directory, self.job_id, self.plan_sha256 = directory, job_id, plan_sha256
        for component in (directory, *directory.parents):
            if component.exists() and is_redirect(component):
                raise ReportRunError("COVERAGE_JOB_PATH_REDIRECTED")
        directory.mkdir(parents=True, exist_ok=True)
        self.path, self.state_path = directory / "events.jsonl", directory / "state.json"
        if self.state_path.exists() and is_redirect(self.state_path):
            raise ReportRunError("COVERAGE_JOB_PATH_REDIRECTED")
        self.events, self.ids = [], set()
        self.state = {"version": 1, "job_id": job_id, "plan_sha256": plan_sha256, "dates": {}, "last_event_seq": 0}
        tail = self._read()
        if self.state_path.exists():
            if self.state_path.stat().st_size > 1024 * 1024:
                raise ReportRunError("COVERAGE_STATE_LIMIT")
            try:
                prior = json.loads(self.state_path.read_text(encoding="utf-8"))
            except ValueError:
                prior = None
            if prior is not None and (prior.get("job_id") != job_id or prior.get("plan_sha256") != plan_sha256):
                raise ReportRunError("COVERAGE_PLAN_CHANGED")
        if tail:
            self.commit("tail_recovered", details={"tail_sha256": tail, "error_code": "INCOMPLETE_EVENT_TAIL"})
        else:
            write_atomic(self.state_path, self.state)

    def _read(self):
        if not self.path.exists():
            return None
        if is_redirect(self.path) or self.path.stat().st_size > 32 * 1024 * 1024:
            raise ReportRunError("COVERAGE_EVENTS_LIMIT")
        data = self.path.read_bytes()
        offset, tail = 0, None
        lines = data.splitlines(keepends=True)
        for index, line in enumerate(lines):
            if len(line) > 64 * 1024:
                raise ReportRunError("COVERAGE_EVENT_LIMIT")
            if not line.endswith(b"\n"):
                if index != len(lines) - 1:
                    raise ReportRunError("COVERAGE_EVENTS_CORRUPT")
                tail = line
                break
            try:
                event = json.loads(line)
                body = event["body"]
                if (event["seq"] != len(self.events) + 1 or event["event_id"] != fingerprint(body)
                    or body["job_id"] != self.job_id or body["plan_sha256"] != self.plan_sha256
                    or event["event_id"] in self.ids):
                    raise ValueError()
            except (ValueError, KeyError, TypeError):
                raise ReportRunError("COVERAGE_EVENTS_CORRUPT") from None
            self._apply(event)
            offset += len(line)
        if tail is not None:
            digest = sha256(tail).hexdigest()
            backup = self.directory / ("events.tail." + digest + ".bin")
            if not backup.exists():
                with backup.open("xb") as stream:
                    stream.write(tail)
                    stream.flush()
                    os.fsync(stream.fileno())
            elif backup.read_bytes() != tail:
                raise ReportRunError("COVERAGE_EVENTS_CORRUPT")
            with self.path.open("r+b") as stream:
                stream.truncate(offset)
                stream.flush()
                os.fsync(stream.fileno())
            return digest
        return None

    def _apply(self, event):
        self.events.append(event)
        self.ids.add(event["event_id"])
        body = event["body"]
        if body["date"] is not None:
            self.state["dates"].setdefault(body["date"], {}).update(body["updates"])
        self.state["last_event_seq"] = event["seq"]
        self.state["calls_used"] = sum(item.get("calls_used", 0) for item in self.state["dates"].values())

    def commit(self, action: str, *, target_date: str | None = None, run_id: str | None = None,
               updates: dict | None = None, details: dict | None = None):
        before = self.state["dates"].get(target_date, {})
        body = {"job_id": self.job_id, "plan_sha256": self.plan_sha256, "date": target_date,
            "phase": (updates or {}).get("status", action),
            "run_id": run_id, "action": action, "updates": updates or {}, "details": details or {},
            "before_quality": before.get("quality"), "after_quality": (updates or {}).get("quality", before.get("quality"))}
        # Prior quality is descriptive and may already have advanced on replay.
        identity = {key: value for key, value in body.items() if key != "before_quality"}
        for event in self.events:
            prior = {key: value for key, value in event["body"].items() if key != "before_quality"}
            if prior == identity:
                return event
        event = {"seq": len(self.events) + 1, "event_id": fingerprint(body), "body": body,
                 "at": datetime.now(UTC).isoformat()}
        data = canonical_json(event) + b"\n"
        if len(data) > 64 * 1024 or (self.path.stat().st_size if self.path.exists() else 0) + len(data) > 32 * 1024 * 1024:
            raise ReportRunError("COVERAGE_EVENTS_LIMIT")
        with self.path.open("ab") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        self._apply(event)
        write_atomic(self.state_path, self.state)
        return event
