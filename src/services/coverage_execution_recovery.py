"""An explicit partial-success revision, preserving the frozen plan and shared call ledger."""
from datetime import UTC, date, datetime
import json
from pathlib import Path
import re
import sqlite3

from pydantic import Field

from ..models.model_budget import JobCallsRevision
from .coverage_repair import (RepairModel, RepairPlan, _file_pin, _job_calls, _read_current, _scanner,
    algorithm_fingerprint, execution_amendment)
from .coverage_repair_events import RepairEvents, write_atomic
from .report_runner.context import RunContext
from .report_runner.errors import ReportRunError
from .report_runner.evidence_daily import EvidenceDailyService
from .report_runner.locking import ReportLock
from .report_runner.model_calls import AuditedModel, Deadline, freeze_model_call
from .report_runner.model_plan import PlannedBatch, check_capabilities
from .report_runner.publication import _hash
from .report_runner.scan_evidence import scan_with_retry
from .report_runner.snapshots import SnapshotRef, fingerprint, is_redirect


class ExecutionRecovery(RepairModel):
    version: int = 1
    job_id: str
    plan_sha256: str
    previous_amendment_sha256: str | None
    created_at: str
    reason: str = Field(min_length=1, max_length=1000)
    authorization_note: str = Field(min_length=1, max_length=1000)
    previous_config_fingerprint: str
    previous_algorithm_fingerprint: str
    config_fingerprint: str
    algorithm_fingerprint: str
    calls_limit: int = Field(ge=1, strict=True)
    retained_call_ids: list[int] = Field(min_length=1, max_length=10000)
    retained_calls_sha256: str
    budget_revision_sha256: str | None = None
    prepared_runs: dict[str, dict]


class ExecutionRuntimeRevision(RepairModel):
    version: int = 1
    job_id: str
    plan_sha256: str
    base_recovery_sha256: str
    previous_algorithm_fingerprint: str
    algorithm_fingerprint: str
    config_fingerprint: str
    prepared_runs_sha256: str
    retained_call_ids: list[int] = Field(min_length=1, max_length=10000)
    retained_calls_sha256: str
    created_at: str
    reason: str = Field(min_length=1, max_length=1000)
    authorization_note: str = Field(min_length=1, max_length=1000)


def _runtime_revision(path, recovery, base_hash, plan, identity):
    if is_redirect(path) or path.stat().st_size > 128 * 1024:
        raise ValueError()
    envelope = json.loads(path.read_text(encoding="utf-8"))
    if set(envelope) != {"revision", "sha256"} or fingerprint(envelope["revision"]) != envelope["sha256"]:
        raise ValueError()
    revision = ExecutionRuntimeRevision.model_validate(envelope["revision"])
    if (revision.version != 1 or revision.job_id != plan.job_id or revision.plan_sha256 != identity
            or revision.base_recovery_sha256 != base_hash
            or revision.previous_algorithm_fingerprint != recovery.algorithm_fingerprint
            or revision.algorithm_fingerprint == recovery.algorithm_fingerprint
            or revision.config_fingerprint != recovery.config_fingerprint
            or revision.prepared_runs_sha256 != fingerprint(recovery.prepared_runs)
            or revision.retained_call_ids != recovery.retained_call_ids
            or revision.retained_calls_sha256 != recovery.retained_calls_sha256):
        raise ValueError()
    return revision, envelope["sha256"]


def execution_recovery(plan, directory, identity):
    path = directory / "execution-recovery.json"
    if not path.exists():
        return None, None
    if is_redirect(path) or path.stat().st_size > 512 * 1024:
        raise ReportRunError("COVERAGE_EXECUTION_RECOVERY_INVALID")
    try:
        envelope = json.loads(path.read_text(encoding="utf-8"))
        if set(envelope) != {"recovery", "sha256"} or envelope["sha256"] != fingerprint(envelope["recovery"]):
            raise ValueError()
        recovery = ExecutionRecovery.model_validate(envelope["recovery"])
        amendment, amendment_hash = execution_amendment(plan, directory, identity)
        previous = amendment or plan
        retained = _job_calls(plan, set(recovery.retained_call_ids))
        if (recovery.version != 1 or recovery.job_id != plan.job_id or recovery.plan_sha256 != identity
                or recovery.previous_amendment_sha256 != amendment_hash
                or recovery.previous_config_fingerprint != previous.config_fingerprint
                or recovery.previous_algorithm_fingerprint != previous.algorithm_fingerprint
                or recovery.calls_limit < plan.calls_limit
                or len(set(recovery.retained_call_ids)) != len(recovery.retained_call_ids)
                or len(retained) != len(recovery.retained_call_ids)
                or any(row["status"] not in {"failed", "succeeded"} for row in retained)
                or fingerprint(retained) != recovery.retained_calls_sha256):
            raise ValueError()
        with sqlite3.connect(Path(plan.db_path).resolve().as_uri() + "?mode=ro", uri=True) as connection:
            connection.row_factory = sqlite3.Row
            if recovery.calls_limit > plan.calls_limit:
                row = connection.execute("SELECT value FROM report_metadata WHERE key=?",
                    ("job_calls_revision:" + plan.job_id,)).fetchone()
                budget_envelope = json.loads(row["value"]) if row is not None else {}
                budget = JobCallsRevision.model_validate(budget_envelope["revision"])
                if (fingerprint(budget_envelope["revision"]) != budget_envelope["sha256"]
                        or budget_envelope["sha256"] != recovery.budget_revision_sha256
                        or budget.plan_sha256 != identity or budget.job_id != plan.job_id
                        or budget.previous_calls_limit != plan.calls_limit or budget.calls_limit != recovery.calls_limit
                        or budget.tokens_limit != plan.tokens_limit or budget.cost_limit_microunits != plan.cost_limit_microunits
                        or (budget.pricing.model_dump(mode="json") if budget.pricing else None) != plan.pricing
                        or budget.retained_call_ids != recovery.retained_call_ids
                        or budget.retained_calls_sha256 != recovery.retained_calls_sha256):
                    raise ValueError()
            elif recovery.budget_revision_sha256 is not None:
                raise ValueError()
            for target, prepared in recovery.prepared_runs.items():
                row = connection.execute("SELECT * FROM report_runs WHERE run_id=?", (prepared["run_id"],)).fetchone()
                if (row is None or row["target_date"] != target or row["run_kind"] != "preview"
                        or row["config_fingerprint"] != recovery.config_fingerprint
                        or any(row[key] != prepared[key] for key in ("generation_id", "snapshot_path", "snapshot_sha256", "input_fingerprint"))):
                    raise ValueError()
        revision_path = directory / "execution-runtime-revision.json"
        if revision_path.exists():
            revision, revision_hash = _runtime_revision(revision_path, recovery, envelope["sha256"], plan, identity)
            return recovery.model_copy(update={"algorithm_fingerprint": revision.algorithm_fingerprint}), revision_hash
        return recovery, envelope["sha256"]
    except (ValueError, KeyError, TypeError):
        raise ReportRunError("COVERAGE_EXECUTION_RECOVERY_INVALID") from None


def recovery_committed(journal, recovery, identity):
    actions = {(event["body"]["action"], event["body"]["date"]) for event in journal.events
        if event["body"]["details"].get("recovery_sha256") == identity}
    return ("partial_execution_recovered", None) in actions and all(
        ("recovery_run_prepared", target) in actions for target in recovery.prepared_runs)


def revise_execution_runtime(config, path: Path, *, reason: str, authorization_note: str) -> Path:
    """Append one implementation revision without changing inputs, runs, limits or prior calls."""
    envelope = json.loads(path.read_text(encoding="utf-8"))
    plan = RepairPlan.model_validate(envelope["plan"])
    identity = fingerprint(envelope["plan"])
    directory = Path(config.data_dir) / "coverage_repair" / plan.job_id
    if envelope["sha256"] != identity or path.absolute() != (directory / "plan.json").absolute():
        raise ReportRunError("COVERAGE_PLAN_CHANGED")
    with ReportLock(directory / "job", scope_id=plan.job_id), ReportLock(Path(plan.db_path)):
        recovery, recovery_hash = execution_recovery(plan, directory, identity)
        if recovery is None or EvidenceDailyService(config)._config_fingerprint() != recovery.config_fingerprint:
            raise ReportRunError("COVERAGE_EXECUTION_RECOVERY_INVALID")
        revision_path = directory / "execution-runtime-revision.json"
        current_algorithm = algorithm_fingerprint(config)
        journal = RepairEvents(directory, plan.job_id, identity)
        if revision_path.exists():
            if recovery.algorithm_fingerprint != current_algorithm:
                raise ReportRunError("COVERAGE_EXECUTION_ALREADY_REVISED")
            if recovery_committed(journal, recovery, recovery_hash):
                return revision_path
        else:
            calls = _job_calls(plan)
            if calls != _job_calls(plan, set(recovery.retained_call_ids)):
                raise ReportRunError("COVERAGE_RUNTIME_NEW_CALLS_PRESENT")
            if (current_algorithm == recovery.algorithm_fingerprint or not reason.strip() or not authorization_note.strip()
                    or any(value.get("status") == "candidate" for value in journal.state["dates"].values())
                    or plan.db_path != str(Path(config.db_dir) / "reports.sqlite3")
                    or plan.pricing != (config.recovery_settings.model_pricing.model_dump(mode="json") if config.recovery_settings.model_pricing else None)):
                raise ReportRunError("COVERAGE_EXECUTION_RECOVERY_INVALID")
            app = EvidenceDailyService(config)
            app._open()
            for prepared in recovery.prepared_runs.values():
                app.snapshots.read(SnapshotRef(prepared["snapshot_path"], prepared["snapshot_sha256"]))
            revision = ExecutionRuntimeRevision(job_id=plan.job_id, plan_sha256=identity,
                base_recovery_sha256=recovery_hash, previous_algorithm_fingerprint=recovery.algorithm_fingerprint,
                algorithm_fingerprint=current_algorithm, config_fingerprint=recovery.config_fingerprint,
                prepared_runs_sha256=fingerprint(recovery.prepared_runs), retained_call_ids=recovery.retained_call_ids,
                retained_calls_sha256=recovery.retained_calls_sha256, created_at=datetime.now(UTC).isoformat(),
                reason=reason, authorization_note=authorization_note)
            values = revision.model_dump(mode="json")
            recovery_hash = fingerprint(values)
            write_atomic(revision_path, {"revision": values, "sha256": recovery_hash})
        journal.commit("execution_runtime_revised", details={"recovery_sha256": recovery_hash, "calls_delta": 0})
        journal.commit("partial_execution_recovered", details={"recovery_sha256": recovery_hash, "calls_limit": recovery.calls_limit,
            "tokens_limit": plan.tokens_limit, "cost_limit_microunits": plan.cost_limit_microunits, "calls_delta": 0})
        for target, prepared in recovery.prepared_runs.items():
            journal.commit("recovery_run_prepared", target_date=target, run_id=prepared["run_id"],
                details={"recovery_sha256": recovery_hash, "runtime_only": True, "calls_delta": 0})
    return revision_path


def _gateway_batch(batch, config):
    if (batch.model != config.llm_config["model_id"] or batch.temperature != config.llm_config["temperature"]
            or batch.max_tokens != config.llm_config["max_tokens"]):
        raise ReportRunError("COVERAGE_RECOVERY_MODEL_CHANGED")
    values = batch.model_dump(mode="json")
    values.update(thinking_mode=config.model_request_settings.thinking_mode,
        thinking_parameter_location=config.model_request_settings.thinking_parameter_location)
    result = PlannedBatch.model_validate(values)
    if result.request_bytes > config.model_request_settings.max_request_utf8_bytes:
        # Remove layout whitespace between complete instructions; source text/ranges stay exact.
        system = "\n".join(line.strip() for line in batch.messages[0].content.splitlines() if line.strip())
        system = re.sub(r"(?<=[。；！？：}\]])\n+", "", system)
        values["messages"][0]["content"] = system
        result = PlannedBatch.model_validate(values)
    check_capabilities(config.model_request_settings, message_bytes=result.request_bytes, max_tokens=result.max_tokens)
    return result


def _prepare_preview(app, parent, *, calls_limit, plan, all_calls, budget_revision_sha256):
    payload = app.snapshots.read(SnapshotRef(parent["snapshot_path"], parent["snapshot_sha256"]))
    if (payload.get("version") not in {1, 2} or payload.get("input_fingerprint") != parent["input_fingerprint"]
            or payload["context"]["run_id"] != parent["run_id"]):
        raise ReportRunError("COVERAGE_RECOVERY_INPUT_MISMATCH")
    roots = [PlannedBatch.model_validate(value) for value in payload["batches"]]
    if not roots or any(batch.response_schema_version != 2 for batch in roots):
        raise ReportRunError("COVERAGE_RECOVERY_SCHEMA_UNSUPPORTED")
    client = AuditedModel(app.runs, app.snapshots, transport=None, settings=app.config.model_request_settings,
        limits=app.config.recovery_settings, deadline=Deadline(300), job_budget={"job_id": plan.job_id})
    batches, reused, supplemental = [], {}, []
    for root in roots:
        effective, _ = client.effective_batch(parent["run_id"], root)
        result = client.cached_result(parent["run_id"], root)
        if result is None:
            batches.append(_gateway_batch(root, app.config))
        else:
            call = app.runs.successful_call(parent["generation_id"], client._request_fingerprint(parent, effective))
            reused[root.logical_step] = freeze_model_call(parent, call).model_dump(mode="json")
            batches.append(effective)
            if effective.revision:
                # The successful prefix remains reusable. Its omitted original ranges need a separate request.
                full = _gateway_batch(root, app.config)
                step = f"batch_{len(roots) + len(supplemental):03d}"
                full = PlannedBatch.model_validate({**full.model_dump(mode="json"), "logical_step": step})
                supplemental.append((full, {"logical_step": step, "parent_logical_step": root.logical_step,
                    "parent_root_sha256": fingerprint(root.model_dump(mode="json")),
                    "reused_call_id": call["call_id"]}))
    if not reused:
        raise ReportRunError("COVERAGE_RECOVERY_SUCCESS_REQUIRED")
    batches.extend(batch for batch, _ in supplemental)
    prior = []
    for row in all_calls:
        run = app.runs.get(row["run_id"])
        if run["target_date"] != parent["target_date"]:
            continue
        for call in app.runs.calls(run["run_id"]):
            if call["call_id"] == row["call_id"] and call["logical_step"].startswith("batch_"):
                prior.append(freeze_model_call(run, call).model_dump(mode="json"))
    context = RunContext.create(now=datetime.now(UTC), target_date=date.fromisoformat(parent["target_date"]),
        config_fingerprint=app._config_fingerprint())
    identity = fingerprint({"version": 1, "parent_input_fingerprint": payload["input_fingerprint"],
        "parent_snapshot_sha256": parent["snapshot_sha256"], "batches": [batch.model_dump(mode="json") for batch in batches],
        "reused_model_calls": reused})
    revised = {**payload, "context": context.to_dict(), "input_fingerprint": identity,
        "recovery_fingerprint": app._recovery_fingerprint(), "batches": [batch.model_dump(mode="json") for batch in batches],
        "reused_model_calls": reused, "prior_model_calls": prior,
        "supplemental_model_batches": [metadata for _, metadata in supplemental],
        "job_budget": {"job_id": plan.job_id, "job_calls_limit": calls_limit,
            "job_tokens_limit": plan.tokens_limit, "job_cost_limit_microunits": plan.cost_limit_microunits},
        "execution_recovery_parent": {"run_id": parent["run_id"], "generation_id": parent["generation_id"],
            "input_fingerprint": parent["input_fingerprint"], "snapshot_sha256": parent["snapshot_sha256"]}}
    if budget_revision_sha256 is not None:
        revised["job_budget"]["job_calls_revision_sha256"] = budget_revision_sha256
    app.runs.create(context, kind="preview", regeneration_reason="coverage_candidate:" + plan.job_id)
    reference = app.snapshots.put(context.run_id, "input", revised)
    app.runs.prepare(context.run_id, reference, identity, [])
    prepared_run = app.runs.get(context.run_id)
    return {"run_id": context.run_id, "generation_id": prepared_run["generation_id"], "parent_run_id": parent["run_id"], "snapshot_path": reference.path,
        "snapshot_sha256": reference.sha256, "input_fingerprint": identity,
        "calls_before_run": len(prior), "reused_call_ids": [value["call_id"] for value in reused.values()],
        "supplemental_model_batches": [metadata for _, metadata in supplemental]}


def _prepare_pending(app, plan, item, directory, *, calls_limit, budget_revision_sha256, scanner_factory):
    context = RunContext.create(now=datetime.now(UTC), target_date=date.fromisoformat(item.date),
        config_fingerprint=app._config_fingerprint())
    app.job_budget = {"job_id": plan.job_id, "job_calls_limit": calls_limit,
        "job_tokens_limit": plan.tokens_limit, "job_cost_limit_microunits": plan.cost_limit_microunits}
    if budget_revision_sha256 is not None:
        app.job_budget["job_calls_revision_sha256"] = budget_revision_sha256
    scan = scan_with_retry(_scanner(app.config, plan, item, directory, factory=scanner_factory),
        context, Deadline(app.config.recovery_settings.run_budget_seconds))
    original_settings = app.config.model_request_settings.model_copy(update={"thinking_parameter_location": None})
    source_plan, payload = app._prepare_input(context, scan, model_settings=original_settings)
    if payload["quality"] == "unavailable" or not source_plan.batches:
        raise ReportRunError("COVERAGE_RECOVERY_INPUT_UNAVAILABLE")
    batches = [_gateway_batch(batch, app.config) for batch in source_plan.batches]
    identity = fingerprint({"version": 1, "original_input_fingerprint": source_plan.input_fingerprint,
        "batches": [batch.model_dump(mode="json") for batch in batches], "job_budget": app.job_budget})
    payload.update(batches=[batch.model_dump(mode="json") for batch in batches], input_fingerprint=identity,
        execution_recovery_pending=True)
    app.runs.create(context, kind="preview", regeneration_reason="coverage_candidate:" + plan.job_id)
    reference = app.snapshots.put(context.run_id, "input", payload)
    app.runs.prepare(context.run_id, reference, identity, list(source_plan.sources))
    return {"run_id": context.run_id, "generation_id": app.runs.get(context.run_id)["generation_id"],
        "parent_run_id": None, "snapshot_path": reference.path, "snapshot_sha256": reference.sha256,
        "input_fingerprint": identity, "calls_before_run": 0, "reused_call_ids": [], "supplemental_model_batches": []}


def prepare_execution_recovery(config, path: Path, *, reason: str, authorization_note: str,
                               calls_limit: int | None = None, scanner_factory=None) -> Path:
    """Prepare only; the caller supplies the authorized limit. Never send or erase old calls."""
    if path.stat().st_size > 2 * 1024 * 1024 or is_redirect(path):
        raise ReportRunError("COVERAGE_PLAN_INVALID")
    envelope = json.loads(path.read_text(encoding="utf-8"))
    plan = RepairPlan.model_validate(envelope["plan"])
    identity = fingerprint(envelope["plan"])
    directory = Path(config.data_dir) / "coverage_repair" / plan.job_id
    if envelope["sha256"] != identity or path.absolute() != (directory / "plan.json").absolute():
        raise ReportRunError("COVERAGE_PLAN_CHANGED")
    approved_limit = plan.calls_limit if calls_limit is None else calls_limit
    if (type(approved_limit) is not int or approved_limit < plan.calls_limit or not reason.strip() or not authorization_note.strip()
            or plan.pricing != (config.recovery_settings.model_pricing.model_dump(mode="json") if config.recovery_settings.model_pricing else None)
            or plan.db_path != str(Path(config.db_dir) / "reports.sqlite3")
            or plan.reports_root != str(Path(config.reports_dir).absolute())
            or plan.work_root != str(Path(config.work_dir).absolute())
            or config.model_request_settings.max_request_utf8_bytes + 1024 + config.llm_config["max_tokens"] > plan.worst_tokens_per_call):
        raise ReportRunError("COVERAGE_EXECUTION_RECOVERY_INVALID")
    config_identity = EvidenceDailyService(config)._config_fingerprint()
    algorithm_identity = algorithm_fingerprint(config)
    with ReportLock(directory / "job", scope_id=plan.job_id), ReportLock(Path(plan.db_path)):
        journal = RepairEvents(directory, plan.job_id, identity)
        recovery, recovery_hash = execution_recovery(plan, directory, identity)
        if recovery is not None:
            if (recovery.config_fingerprint != config_identity or recovery.algorithm_fingerprint != algorithm_identity
                    or recovery.calls_limit != approved_limit):
                raise ReportRunError("COVERAGE_EXECUTION_ALREADY_RECOVERED")
            if recovery_committed(journal, recovery, recovery_hash):
                return directory / "execution-recovery.json"
        else:
            calls = _job_calls(plan)
            if not calls or any(row["status"] not in {"failed", "succeeded"} for row in calls):
                raise ReportRunError("COVERAGE_RECOVERY_UNKNOWN_CALLS")
            amendment, amendment_hash = execution_amendment(plan, directory, identity)
            previous = amendment or plan
            app = EvidenceDailyService(config)
            app._open()
            budget_revision_sha256 = None
            if approved_limit > plan.calls_limit:
                budget_revision_sha256 = app.runs.register_job_calls_revision(JobCallsRevision(
                    job_id=plan.job_id, plan_sha256=identity, previous_calls_limit=plan.calls_limit,
                    calls_limit=approved_limit, tokens_limit=plan.tokens_limit,
                    cost_limit_microunits=plan.cost_limit_microunits, pricing=plan.pricing,
                    retained_call_ids=[row["call_id"] for row in calls], retained_calls_sha256=fingerprint(calls),
                    authorization_note=authorization_note))
            prepared = {}
            for item in plan.dates:
                current = _read_current(config, item.date)
                if (current["current_run_id"] != item.old_run_id or current["expected_sha256"] != item.old_sha256
                        or _hash(Path(plan.reports_root) / item.date[:7] / (item.date + ".md")) != item.old_sha256):
                    raise ReportRunError("COVERAGE_CURRENT_CHANGED")
                for pin in item.files:
                    if _file_pin(Path(plan.work_root), pin, max(pin.size_bytes, 1)) != pin:
                        raise ReportRunError("COVERAGE_SOURCE_CHANGED")
                progress = journal.state["dates"].get(item.date, {})
                if progress.get("status") == "candidate":
                    raise ReportRunError("COVERAGE_RECOVERY_CANDIDATE_EXISTS")
                if not progress.get("run_id"):
                    prepared[item.date] = _prepare_pending(app, plan, item, directory, calls_limit=approved_limit,
                        budget_revision_sha256=budget_revision_sha256, scanner_factory=scanner_factory)
                    continue
                parent = app.runs.get(progress["run_id"])
                if parent["status"] != "failed" or parent["run_kind"] != "preview" or parent["phase"] not in {"prepared", "generating"}:
                    raise ReportRunError("COVERAGE_RECOVERY_FAILED_PREVIEW_REQUIRED")
                prepared[item.date] = _prepare_preview(app, parent, calls_limit=approved_limit, plan=plan, all_calls=calls,
                    budget_revision_sha256=budget_revision_sha256)
            if not any(value["reused_call_ids"] for value in prepared.values()):
                raise ReportRunError("COVERAGE_RECOVERY_SUCCESS_REQUIRED")
            recovery = ExecutionRecovery(job_id=plan.job_id, plan_sha256=identity, previous_amendment_sha256=amendment_hash,
                created_at=datetime.now(UTC).isoformat(), reason=reason, authorization_note=authorization_note,
                previous_config_fingerprint=previous.config_fingerprint, previous_algorithm_fingerprint=previous.algorithm_fingerprint,
                config_fingerprint=config_identity, algorithm_fingerprint=algorithm_identity, calls_limit=approved_limit,
                retained_call_ids=[row["call_id"] for row in calls], retained_calls_sha256=fingerprint(calls),
                budget_revision_sha256=budget_revision_sha256, prepared_runs=prepared)
            data = recovery.model_dump(mode="json")
            recovery_hash = fingerprint(data)
            write_atomic(directory / "execution-recovery.json", {"recovery": data, "sha256": recovery_hash})
        journal.commit("partial_execution_recovered", details={"recovery_sha256": recovery_hash,
            "prior_calls_limit": plan.calls_limit, "calls_limit": approved_limit, "retained_call_ids": recovery.retained_call_ids,
            "tokens_limit": plan.tokens_limit, "cost_limit_microunits": plan.cost_limit_microunits, "calls_delta": 0})
        for target, prepared in recovery.prepared_runs.items():
            journal.commit("recovery_run_prepared", target_date=target, run_id=prepared["run_id"],
                updates={"run_id": prepared["run_id"], "status": "generating", "calls_used": prepared["calls_before_run"],
                    "calls_before_run": prepared["calls_before_run"]}, details={"recovery_sha256": recovery_hash,
                    "parent_run_id": prepared["parent_run_id"], "reused_call_ids": prepared["reused_call_ids"], "calls_delta": 0})
    return directory / "execution-recovery.json"
