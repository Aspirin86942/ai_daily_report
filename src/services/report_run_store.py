"""Durable report runs and atomic model-call reservations in the existing report DB."""

from __future__ import annotations

from datetime import UTC, date, datetime, timedelta
from hashlib import sha256
import json
from typing import Any, Callable
from uuid import uuid4
from zoneinfo import ZoneInfo

from ..models.evidence_report import EvidenceDailyReportData, ReportSource
from ..models.report_settings import RecoverySettings
from .report_runner.context import RunContext
from .report_runner.errors import ReportRunError
from .report_runner.snapshots import SnapshotRef, canonical_json
from .sqlite_store import SQLiteStore


def _json(value: Any) -> str:
    return canonical_json(value).decode("utf-8")


class ReportRunStore:
    def __init__(self, store: SQLiteStore, *, clock: Callable[[], datetime] | None = None):
        self.store = store
        self.clock = clock or (lambda: datetime.now(UTC))

    def _now(self) -> datetime:
        value = self.clock()
        if value.utcoffset() is None:
            raise ValueError("audit clock must be timezone-aware")
        return value.astimezone(UTC)

    def create(self, context: RunContext, *, kind: str = "report", generation_id: str | None = None,
               regeneration_reason: str | None = None) -> dict:
        if kind not in {"report", "preview", "model_check", "weekly", "monthly"}:
            raise ValueError("invalid run kind")
        now = self._now().isoformat()
        with self.store._get_conn() as connection:
            connection.execute("""INSERT INTO report_runs(
                run_id,scope_id,target_date,timezone,run_kind,context_json,config_fingerprint,
                generation_id,regeneration_reason,created_at,updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)""",
                (context.run_id, context.scope_id, context.target_date.isoformat(), context.timezone,
                 kind, _json(context.to_dict()), context.config_fingerprint, generation_id or uuid4().hex,
                 regeneration_reason, now, now))
        return self.get(context.run_id)

    def get(self, run_id: str) -> dict:
        with self.store._get_conn() as connection:
            row = connection.execute("SELECT * FROM report_runs WHERE run_id=?", (run_id,)).fetchone()
        if row is None:
            raise ReportRunError("RUN_NOT_FOUND")
        return dict(row)

    def list_runs(self, *, limit: int = 50, target_date: str | None = None) -> list[dict]:
        with self.store._get_conn() as connection:
            rows = connection.execute("""SELECT run_id,target_date,run_kind,phase,status,quality,
                error_code,created_at,updated_at FROM report_runs
                WHERE (? IS NULL OR target_date=?) ORDER BY created_at DESC,run_id LIMIT ?""",
                (target_date, target_date, min(max(limit, 1), 500))).fetchall()
        return [dict(row) for row in rows]

    def pending(self, *, target_date: str | None = None) -> list[dict]:
        with self.store._get_conn() as connection:
            rows = connection.execute("""SELECT * FROM report_runs WHERE run_kind='report'
                AND phase IN ('prepared','generating','db_committed') AND status!='succeeded'
                AND COALESCE(error_code,'')!='RUN_SUPERSEDED'
                AND (? IS NULL OR target_date=?)
                ORDER BY CASE WHEN phase='db_committed' THEN 0 ELSE 1 END,created_at,run_id""",
                (target_date, target_date)).fetchall()
        return [dict(row) for row in rows]

    def set_scanning(self, run_id: str) -> None:
        with self.store._get_conn() as connection:
            cursor = connection.execute("""UPDATE report_runs SET phase='scanning',updated_at=?
                WHERE run_id=? AND phase='created' AND status='running'""", (self._now().isoformat(), run_id))
            if cursor.rowcount != 1:
                raise ReportRunError("RUN_PHASE_INVALID")

    def prepare(self, run_id: str, reference: SnapshotRef, input_fingerprint: str,
                sources: list[ReportSource]) -> None:
        with self.store._get_conn() as connection:
            connection.execute("BEGIN IMMEDIATE")
            cursor = connection.execute("""UPDATE report_runs SET phase='prepared',snapshot_path=?,
                snapshot_sha256=?,input_fingerprint=?,updated_at=?
                WHERE run_id=? AND phase IN ('created','scanning') AND status='running'""",
                (reference.path, reference.sha256, input_fingerprint, self._now().isoformat(), run_id))
            if cursor.rowcount != 1:
                raise ReportRunError("RUN_INPUT_ALREADY_FROZEN")
            connection.executemany("""INSERT INTO report_run_sources(run_id,source_id,metadata_json)
                VALUES (?,?,?)""", [(run_id, item.source_id, item.model_dump_json()) for item in sources])

    def resume(self, run_id: str) -> dict:
        now = self._now().isoformat()
        with self.store._get_conn() as connection:
            connection.execute("BEGIN IMMEDIATE")
            row = connection.execute("SELECT phase,status,error_code FROM report_runs WHERE run_id=?", (run_id,)).fetchone()
            if row is None or row["phase"] not in {"prepared", "generating", "db_committed"}:
                raise ReportRunError("RUN_NOT_RECOVERABLE")
            if row["status"] == "succeeded":
                raise ReportRunError("RUN_ALREADY_FINISHED")
            if row["error_code"] == "RUN_SUPERSEDED":
                raise ReportRunError("RUN_NOT_RECOVERABLE")
            connection.execute("""UPDATE report_model_calls SET status='unknown',error_code='CALL_RESULT_UNKNOWN',
                finished_at=? WHERE run_id=? AND status='reserved'""", (now, run_id))
            connection.execute("""UPDATE report_runs SET status='running',
                error_code=CASE WHEN error_code='OUTPUT_CONFLICT_CAPTURED' THEN error_code ELSE NULL END,finished_at=NULL,
                updated_at=? WHERE run_id=?""", (now, run_id))
        return self.get(run_id)

    def reserve_call(self, run_id: str, *, request_fingerprint: str, logical_step: str,
                     request: SnapshotRef, limits: RecoverySettings, max_attempts: int = 3) -> dict:
        if not 1 <= max_attempts <= 3:
            raise ValueError("model attempts must be between one and three")
        now = self._now()
        day = now.astimezone(ZoneInfo("Asia/Shanghai")).date().isoformat()
        with self.store._get_conn() as connection:
            connection.execute("BEGIN IMMEDIATE")
            run = connection.execute("SELECT * FROM report_runs WHERE run_id=?", (run_id,)).fetchone()
            if run is None or run["status"] != "running" or run["phase"] not in {"prepared", "generating"}:
                raise ReportRunError("RUN_PHASE_INVALID")
            existing = connection.execute("""SELECT COUNT(*) FROM report_model_calls
                WHERE generation_id=? AND logical_step=?""", (run["generation_id"], logical_step)).fetchone()[0]
            if existing >= max_attempts:
                raise ReportRunError("MODEL_STEP_BUDGET_EXHAUSTED")
            count = connection.execute("SELECT COUNT(*) FROM report_model_calls WHERE run_id=?", (run_id,)).fetchone()[0]
            if count >= limits.max_calls_per_run:
                raise ReportRunError("MODEL_RUN_BUDGET_EXHAUSTED")
            daily = connection.execute("SELECT COUNT(*) FROM report_model_calls WHERE call_date=?", (day,)).fetchone()[0]
            if daily >= limits.max_calls_per_calendar_day:
                raise ReportRunError("MODEL_DAY_BUDGET_EXHAUSTED")
            timestamp = now.isoformat()
            cursor = connection.execute("""INSERT INTO report_model_calls(run_id,generation_id,request_fingerprint,
                logical_step,attempt,call_date,status,request_path,request_sha256,started_at)
                VALUES (?,?,?,?,?,?,'reserved',?,?,?)""",
                (run_id, run["generation_id"], request_fingerprint, logical_step, existing + 1, day,
                 request.path, request.sha256, timestamp))
            connection.execute("UPDATE report_runs SET phase='generating',updated_at=? WHERE run_id=?", (timestamp, run_id))
            return dict(connection.execute("SELECT * FROM report_model_calls WHERE call_id=?", (cursor.lastrowid,)).fetchone())

    def finish_call(self, call_id: int, *, status: str, response: SnapshotRef | None = None,
                    usage: dict | None = None, error_code: str | None = None, duration_ms: int = 0) -> None:
        if status not in {"succeeded", "failed", "unknown"} or (status == "succeeded" and response is None):
            raise ValueError("invalid call completion")
        with self.store._get_conn() as connection:
            cursor = connection.execute("""UPDATE report_model_calls SET status=?,response_path=?,response_sha256=?,
                usage_json=?,error_code=?,finished_at=?,duration_ms=? WHERE call_id=? AND status='reserved'""",
                (status, response.path if response else None, response.sha256 if response else None,
                 _json(usage) if usage is not None else None, error_code, self._now().isoformat(), duration_ms, call_id))
            if cursor.rowcount != 1:
                raise ReportRunError("CALL_ALREADY_FINISHED")

    def calls(self, run_id: str, logical_step: str | None = None) -> list[dict]:
        with self.store._get_conn() as connection:
            rows = connection.execute("""SELECT * FROM report_model_calls WHERE run_id=?
                AND (? IS NULL OR logical_step=?) ORDER BY call_id""", (run_id, logical_step, logical_step)).fetchall()
        return [dict(row) for row in rows]

    def successful_call(self, generation_id: str, request_fingerprint: str) -> dict | None:
        with self.store._get_conn() as connection:
            row = connection.execute("""SELECT * FROM report_model_calls WHERE generation_id=?
                AND request_fingerprint=? AND status='succeeded' ORDER BY call_id DESC LIMIT 1""",
                (generation_id, request_fingerprint)).fetchone()
        return dict(row) if row else None

    def fail(self, run_id: str, code: str, *, cancelled: bool = False, quality: str | None = None) -> None:
        now = self._now().isoformat()
        with self.store._get_conn() as connection:
            connection.execute("""UPDATE report_runs SET status=?,error_code=?,quality=COALESCE(?,quality),
                updated_at=?,finished_at=? WHERE run_id=? AND status!='succeeded'""",
                ("cancelled" if cancelled else "failed", code, quality, now, now, run_id))

    def current(self, target_date: str) -> dict | None:
        with self.store._get_conn() as connection:
            row = connection.execute("""SELECT r.* FROM report_runs r JOIN daily_reports d
                ON d.current_run_id=r.run_id WHERE d.date=?""", (target_date,)).fetchone()
        return dict(row) if row else None

    def stage_report(self, run_id: str, report: EvidenceDailyReportData, markdown: str, *,
                     output_path: str, publication_fingerprint: str) -> dict:
        if markdown != markdown.replace("\r\n", "\n").replace("\r", "\n") or markdown.startswith("\ufeff"):
            raise ValueError("published markdown must be LF without a BOM")
        digest = sha256(markdown.encode("utf-8")).hexdigest()
        with self.store._get_conn() as connection:
            connection.execute("BEGIN IMMEDIATE")
            run = connection.execute("SELECT * FROM report_runs WHERE run_id=?", (run_id,)).fetchone()
            if (run is None or run["run_kind"] != "report" or run["status"] != "running"
                    or run["phase"] not in {"prepared", "generating"}):
                raise ReportRunError("RUN_PHASE_INVALID")
            context = RunContext.from_dict(json.loads(run["context_json"]))
            if report.date != str(context.target_date) or datetime.fromisoformat(report.window_start_utc) != context.window_start_utc:
                raise ReportRunError("REPORT_CONTEXT_MISMATCH")
            previous = connection.execute("""SELECT r.run_id,r.expected_sha256 FROM daily_reports d
                JOIN report_runs r ON r.run_id=d.current_run_id WHERE d.date=?""", (report.date,)).fetchone()
            connection.execute("""UPDATE report_runs SET phase='db_committed',quality=?,report_json=?,markdown=?,
                expected_sha256=?,previous_sha256=?,previous_run_id=?,output_path=?,publication_fingerprint=?,updated_at=?
                WHERE run_id=?""", (report.quality, report.model_dump_json(), markdown, digest,
                previous["expected_sha256"] if previous else None, previous["run_id"] if previous else None,
                output_path, publication_fingerprint, self._now().isoformat(), run_id))
            connection.execute("DELETE FROM report_run_sources WHERE run_id=?", (run_id,))
            connection.executemany("INSERT INTO report_run_sources(run_id,source_id,metadata_json) VALUES (?,?,?)",
                [(run_id, source.source_id, source.model_dump_json()) for source in report.sources])
        return self.get(run_id)

    def mark_published(self, run_id: str, *, verified_sha256: str) -> None:
        now = self._now().isoformat()
        with self.store._get_conn() as connection:
            connection.execute("BEGIN IMMEDIATE")
            run = connection.execute("SELECT * FROM report_runs WHERE run_id=?", (run_id,)).fetchone()
            if run is None or run["phase"] != "db_committed" or run["expected_sha256"] != verified_sha256:
                raise ReportRunError("PUBLICATION_NOT_VERIFIED")
            current = connection.execute("SELECT current_run_id FROM daily_reports WHERE date=?", (run["target_date"],)).fetchone()
            if (current[0] if current else None) != run["previous_run_id"]:
                raise ReportRunError("PUBLICATION_VERSION_CHANGED")
            # The current history row moves only after the exact file was verified.
            connection.execute("""INSERT INTO daily_reports(date,completed_work,work_summary,next_plan,raw_json,
                schema_version,report_kind,current_run_id) VALUES (?,'','','',?,2,'file_evidence',?)
                ON CONFLICT(date) DO UPDATE SET completed_work='',work_summary='',next_plan='',raw_json=excluded.raw_json,
                schema_version=2,report_kind='file_evidence',current_run_id=excluded.current_run_id,updated_at=?""",
                (run["target_date"], run["report_json"], run_id, now))
            connection.execute("""UPDATE report_runs SET phase='published',status='succeeded',error_code=NULL,
                updated_at=?,finished_at=? WHERE run_id=?""", (now, now, run_id))
            if run["regeneration_reason"]:
                # Only a verified replacement retires earlier recovery work.
                # Calls and frozen evidence remain auditable and still consume budget.
                connection.execute("""UPDATE report_runs SET status='cancelled',error_code='RUN_SUPERSEDED',
                    updated_at=?,finished_at=? WHERE run_kind='report' AND scope_id=? AND target_date=?
                    AND phase IN ('prepared','generating','db_committed') AND status!='succeeded'
                    AND run_id!=? AND created_at<=?""",
                    (now, now, run["scope_id"], run["target_date"], run_id, run["created_at"]))

    def finish_preview(self, run_id: str, quality: str) -> None:
        now = self._now().isoformat()
        with self.store._get_conn() as connection:
            cursor = connection.execute("""UPDATE report_runs SET status='succeeded',quality=?,updated_at=?,finished_at=?
                WHERE run_id=? AND run_kind!='report' AND status='running'""", (quality, now, now, run_id))
            if cursor.rowcount != 1:
                raise ReportRunError("RUN_PHASE_INVALID")

    def finish_observation(self, run_id: str, current_run_id: str, quality: str) -> None:
        now = self._now().isoformat()
        with self.store._get_conn() as connection:
            cursor = connection.execute("""UPDATE report_runs SET status='succeeded',quality=?,previous_run_id=?,
                updated_at=?,finished_at=? WHERE run_id=? AND phase='prepared' AND status='running'""",
                (quality, current_run_id, now, now, run_id))
            if cursor.rowcount != 1:
                raise ReportRunError("RUN_PHASE_INVALID")

    def scan_failure(self, run_id: str, reference: SnapshotRef, sources: list[ReportSource]) -> None:
        with self.store._get_conn() as connection:
            connection.execute("BEGIN IMMEDIATE")
            cursor = connection.execute("""UPDATE report_runs SET snapshot_path=?,snapshot_sha256=?
                WHERE run_id=? AND phase='scanning'""", (reference.path, reference.sha256, run_id))
            if cursor.rowcount != 1:
                raise ReportRunError("RUN_PHASE_INVALID")
            connection.executemany("INSERT INTO report_run_sources(run_id,source_id,metadata_json) VALUES (?,?,?)",
                                   [(run_id, source.source_id, source.model_dump_json()) for source in sources])

    def activate(self, first_target_date: date) -> None:
        with self.store._get_conn() as connection:
            connection.execute("INSERT OR IGNORE INTO report_metadata(key,value) VALUES ('first_target_date',?)",
                               (first_target_date.isoformat(),))

    def recover_interrupted_bookkeeping(self) -> None:
        """Caller holds the process lock, so no previous running owner can remain."""
        now = self._now().isoformat()
        with self.store._get_conn() as connection:
            connection.execute("BEGIN IMMEDIATE")
            connection.execute("""UPDATE report_model_calls SET status='unknown',error_code='CALL_RESULT_UNKNOWN',
                finished_at=? WHERE status='reserved'""", (now,))
            connection.execute("""UPDATE report_runs SET status='failed',error_code='RUN_INTERRUPTED',updated_at=?,finished_at=?
                WHERE status='running' AND (phase IN ('created','scanning') OR run_kind!='report')""", (now, now))

    def missed_dates(self, latest_target: date) -> list[str]:
        with self.store._get_conn() as connection:
            row = connection.execute("SELECT value FROM report_metadata WHERE key='first_target_date'").fetchone()
            if row is None:
                return []
            first = date.fromisoformat(row[0])
            existing = {item[0] for item in connection.execute("SELECT date FROM daily_reports WHERE date>=? AND date<?",
                                                               (first.isoformat(), latest_target.isoformat()))}
        return [(first + timedelta(days=offset)).isoformat() for offset in range(max(0, (latest_target - first).days))
                if (first + timedelta(days=offset)).isoformat() not in existing]
