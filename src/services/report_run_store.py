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
from ..models.model_budget import BudgetReservation, JobBudgetRevision, JobCallsRevision, charged_usage, usage_tokens
from .job_budget_revisions import append_budget_revision, read_revision_chain, reservation_is_approved
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
                AND COALESCE(regeneration_reason,'') NOT LIKE 'coverage_publish:%'
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
                     request: SnapshotRef, limits: RecoverySettings, max_attempts: int = 3,
                     budget: BudgetReservation | None = None) -> dict:
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
            if limits.enforce_model_budgets and count >= limits.max_calls_per_run:
                raise ReportRunError("MODEL_RUN_BUDGET_EXHAUSTED")
            daily = connection.execute("SELECT COUNT(*) FROM report_model_calls WHERE call_date=?", (day,)).fetchone()[0]
            if limits.enforce_model_budgets and daily >= limits.max_calls_per_calendar_day:
                raise ReportRunError("MODEL_DAY_BUDGET_EXHAUSTED")
            if budget is not None and limits.enforce_model_budgets:
                self._check_reserved_budget(connection, run_id, day, budget)
            timestamp = now.isoformat()
            cursor = connection.execute("""INSERT INTO report_model_calls(run_id,generation_id,request_fingerprint,
                logical_step,attempt,call_date,status,request_path,request_sha256,started_at,usage_json)
                VALUES (?,?,?,?,?,?,'reserved',?,?,?,?)""",
                (run_id, run["generation_id"], request_fingerprint, logical_step, existing + 1, day,
                 request.path, request.sha256, timestamp,
                 _json({"reserved_budget": budget.model_dump(mode="json"),
                     **({"budget_limits_enforced": False} if not limits.enforce_model_budgets else {})}) if budget else
                 (_json({"budget_limits_enforced": False}) if not limits.enforce_model_budgets else None)))
            connection.execute("UPDATE report_runs SET phase='generating',updated_at=? WHERE run_id=?", (timestamp, run_id))
            return dict(connection.execute("SELECT * FROM report_model_calls WHERE call_id=?", (cursor.lastrowid,)).fetchone())

    @staticmethod
    def _check_reserved_budget(connection, run_id: str, day: str, budget: BudgetReservation) -> None:
        revision_hash = budget.request_audit.get("job_calls_revision_sha256")
        chain = read_revision_chain(connection, budget.job_id)
        if chain:
            revision, identity = chain[-1]
            if (revision_hash != identity or revision.calls_limit != budget.job_calls_limit
                    or revision.tokens_limit != budget.job_tokens_limit
                    or revision.cost_limit_microunits != budget.job_cost_limit_microunits
                    or revision.pricing != budget.pricing):
                raise ReportRunError("MODEL_JOB_BUDGET_CHANGED")
        elif revision_hash is not None:
            raise ReportRunError("MODEL_JOB_BUDGET_CHANGED")
        rows = connection.execute("""SELECT call_id,run_id,call_date,status,usage_json FROM report_model_calls
            WHERE run_id=? OR call_date=? OR (? IS NOT NULL AND
                json_extract(usage_json,'$.reserved_budget.job_id')=?)""",
            (run_id, day, budget.job_id, budget.job_id)).fetchall()
        used = {scope: [0, 0] for scope in ("run", "day", "job")}
        money_unknown = set()
        job_calls = 0
        for row in rows:
            usage = json.loads(row["usage_json"] or "{}")
            stored = usage.get("reserved_budget")
            scopes = []
            if row["run_id"] == run_id:
                scopes.append("run")
            if row["call_date"] == day:
                scopes.append("day")
            if budget.job_id is not None and stored and stored.get("job_id") == budget.job_id:
                scopes.append("job")
                job_calls += 1
            if stored:
                prior = BudgetReservation.model_validate(stored)
                tokens, money = charged_usage(row, prior, usage)
                if budget.pricing is not None and (prior.pricing is None
                        or prior.pricing.currency != budget.pricing.currency):
                    money = None
                if "job" in scopes and not reservation_is_approved(prior, row["call_id"], chain, budget):
                    raise ReportRunError("MODEL_JOB_BUDGET_CHANGED")
            else:
                # Old unknown calls occupy a full conservative request; fees cannot be invented.
                tokens = usage_tokens(usage)[2] if row["status"] in {"succeeded", "failed"} else None
                tokens = tokens if tokens is not None else budget.legacy_tokens_upper
                money = None
            for scope in scopes:
                used[scope][0] += tokens
                if money is None:
                    money_unknown.add(scope)
                else:
                    used[scope][1] += money
        if budget.job_calls_limit is not None and job_calls >= budget.job_calls_limit:
            raise ReportRunError("MODEL_JOB_CALL_BUDGET_EXHAUSTED")
        for scope, token_limit, cost_limit in (
                ("run", budget.run_tokens_limit, budget.run_cost_limit_microunits),
                ("day", budget.day_tokens_limit, budget.day_cost_limit_microunits),
                ("job", budget.job_tokens_limit, budget.job_cost_limit_microunits)):
            if token_limit is not None and used[scope][0] + budget.token_upper > token_limit:
                raise ReportRunError(f"MODEL_{scope.upper()}_TOKEN_BUDGET_EXHAUSTED")
            if cost_limit is not None:
                if scope in money_unknown:
                    raise ReportRunError("MODEL_LEGACY_COST_UNKNOWN")
                if used[scope][1] + budget.cost_upper_microunits > cost_limit:
                    raise ReportRunError(f"MODEL_{scope.upper()}_COST_BUDGET_EXHAUSTED")

    def register_job_calls_revision(self, revision: JobCallsRevision | JobBudgetRevision) -> str:
        """Register an explicitly authorized increase; never rewrite existing reservations."""
        if isinstance(revision, JobBudgetRevision):
            with self.store._get_conn() as connection:
                connection.execute("BEGIN IMMEDIATE")
                return append_budget_revision(connection, revision)
        value = revision.model_dump(mode="json")
        identity = sha256(canonical_json(value)).hexdigest()
        key = "job_calls_revision:" + revision.job_id
        envelope = _json({"revision": value, "sha256": identity})
        with self.store._get_conn() as connection:
            connection.execute("BEGIN IMMEDIATE")
            prior = connection.execute("SELECT value FROM report_metadata WHERE key=?", (key,)).fetchone()
            if prior is not None:
                if prior["value"] != envelope:
                    raise ReportRunError("MODEL_JOB_CALL_REVISION_ALREADY_REGISTERED")
                return identity
            rows = [dict(row) for row in connection.execute("""SELECT call_id,run_id,status,error_code,
                usage_json,request_sha256,response_sha256 FROM report_model_calls
                WHERE json_extract(usage_json,'$.reserved_budget.job_id')=? ORDER BY call_id""", (revision.job_id,))]
            if ([row["call_id"] for row in rows] != revision.retained_call_ids
                    or sha256(canonical_json(rows)).hexdigest() != revision.retained_calls_sha256):
                raise ReportRunError("MODEL_JOB_CALL_REVISION_CHANGED")
            for row in rows:
                old = BudgetReservation.model_validate(json.loads(row["usage_json"])["reserved_budget"])
                if (row["status"] not in {"failed", "succeeded"} or old.request_audit.get("job_calls_revision_sha256") is not None
                        or old.job_calls_limit != revision.previous_calls_limit or old.job_tokens_limit != revision.tokens_limit
                        or old.job_cost_limit_microunits != revision.cost_limit_microunits or old.pricing != revision.pricing):
                    raise ReportRunError("MODEL_JOB_CALL_REVISION_CHANGED")
            connection.execute("INSERT INTO report_metadata(key,value) VALUES (?,?)", (key, envelope))
        return identity

    def finish_call(self, call_id: int, *, status: str, response: SnapshotRef | None = None,
                    usage: dict | None = None, error_code: str | None = None, duration_ms: int = 0) -> None:
        if status not in {"succeeded", "failed", "unknown"} or (status == "succeeded" and response is None):
            raise ValueError("invalid call completion")
        with self.store._get_conn() as connection:
            connection.execute("BEGIN IMMEDIATE")
            row = connection.execute("SELECT usage_json FROM report_model_calls WHERE call_id=? AND status='reserved'",
                                     (call_id,)).fetchone()
            if row is None:
                raise ReportRunError("CALL_ALREADY_FINISHED")
            reserved_usage = json.loads(row["usage_json"] or "{}")
            reserved = reserved_usage.get("reserved_budget")
            merged_usage = dict(usage or {})
            merged_usage.pop("reserved_budget", None)
            merged_usage.pop("budget_limits_enforced", None)
            if reserved_usage.get("budget_limits_enforced") is False:
                merged_usage["budget_limits_enforced"] = False
            if reserved is not None:
                merged_usage["reserved_budget"] = reserved
            cursor = connection.execute("""UPDATE report_model_calls SET status=?,response_path=?,response_sha256=?,
                usage_json=?,error_code=?,finished_at=?,duration_ms=? WHERE call_id=? AND status='reserved'""",
                (status, response.path if response else None, response.sha256 if response else None,
                 _json(merged_usage) if merged_usage or usage is not None else None,
                 error_code, self._now().isoformat(), duration_ms, call_id))
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

    def finish_preview(self, run_id: str, quality: str, *, report: EvidenceDailyReportData | None = None,
                       markdown: str | None = None) -> None:
        now = self._now().isoformat()
        if (report is None) != (markdown is None):
            raise ValueError("preview report and Markdown must be saved together")
        with self.store._get_conn() as connection:
            if report is not None:
                run = connection.execute("SELECT run_kind,context_json FROM report_runs WHERE run_id=?", (run_id,)).fetchone()
                if run is None or run["run_kind"] != "preview":
                    raise ReportRunError("RUN_PHASE_INVALID")
                context = RunContext.from_dict(json.loads(run["context_json"]))
                if report.date != str(context.target_date) or report.window_start_utc != context.window_start_utc.isoformat():
                    raise ReportRunError("REPORT_CONTEXT_MISMATCH")
                if markdown.startswith("\ufeff") or "\r" in markdown:
                    raise ReportRunError("REPORT_MARKDOWN_INVALID")
                connection.execute("UPDATE report_runs SET report_json=?,markdown=?,expected_sha256=? WHERE run_id=?",
                    (report.model_dump_json(), markdown, sha256(markdown.encode("utf-8")).hexdigest(), run_id))
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
