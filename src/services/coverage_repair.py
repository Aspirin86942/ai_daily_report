"""Explicit, frozen historical coverage jobs and zero-call candidate publication."""
from __future__ import annotations

from dataclasses import replace
from datetime import UTC, date, datetime
from difflib import unified_diff
from hashlib import sha256
import json
from pathlib import Path
import shutil
import sqlite3
from uuid import uuid4
from zoneinfo import ZoneInfo

from pydantic import BaseModel, ConfigDict, Field, model_validator

from src.models.coverage import summarize_coverage
from src.models.evidence_report import EvidenceDailyReportData, RelativePath, ReportSource
from src.models.model_budget import cost_microunits
from src.services.coverage_repair_events import RepairEvents, write_atomic
from src.services.native_scanner import NativeScanner, ScanRequest
from src.services.report_runner.context import RunContext, parse_report_date
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.evidence_daily import EvidenceDailyService
from src.services.report_runner.locking import ReportLock
from src.services.report_runner.model_calls import Deadline
from src.services.report_runner.publication import _hash
from src.services.report_runner.snapshots import SnapshotRef, canonical_json, fingerprint, is_redirect
from src.services.scan_preview import ReportScannerConfig


class RepairModel(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True, frozen=True)


class FilePin(RepairModel):
    relative_path: RelativePath
    size_bytes: int = Field(ge=0)
    prior_content_sha256: str | None = Field(pattern=r"^[a-f0-9]{64}$")
    content_sha256: str | None = Field(pattern=r"^[a-f0-9]{64}$")
    mtime_ns: int | None
    current_version_changed: bool | None = False


class RepairDate(RepairModel):
    date: str
    gap_codes: list[str] = Field(min_length=1, max_length=64)
    old_run_id: str = Field(pattern=r"^[a-f0-9]{32}$")
    old_sha256: str = Field(pattern=r"^[a-f0-9]{64}$")
    old_quality: str
    files: list[FilePin] = Field(min_length=1, max_length=4096)


class RepairPlan(RepairModel):
    version: int = 1
    job_id: str = Field(pattern=r"^coverage_[a-f0-9]{32}$")
    created_at: str
    work_root: str
    approved_root: str
    db_path: str
    reports_root: str
    config_fingerprint: str = Field(pattern=r"^[a-f0-9]{64}$")
    algorithm_fingerprint: str = Field(pattern=r"^[a-f0-9]{64}$")
    pricing: dict | None = None
    calls_limit: int = Field(ge=0, le=10000)
    tokens_limit: int = Field(ge=1)
    cost_limit_microunits: int | None = Field(default=None, ge=0)
    worst_tokens_per_call: int = Field(ge=1)
    worst_cost_per_call_microunits: int | None = Field(default=None, ge=0)
    refreshed: bool
    dates: list[RepairDate] = Field(min_length=1, max_length=31)

    @model_validator(mode="after")
    def validate_dates(self):
        if self.version != 1 or len({item.date for item in self.dates}) != len(self.dates):
            raise ValueError("invalid coverage plan version or duplicate date")
        for item in self.dates:
            parse_report_date(item.date)
            if len({pin.relative_path for pin in item.files}) != len(item.files):
                raise ValueError("duplicate planned source")
        if sum(pin.size_bytes for item in self.dates for pin in item.files) > 512 * 1024 * 1024:
            raise ValueError("coverage job input exceeds 512 MiB")
        return self


class RuntimeOverlay:
    def __init__(self, original, **changes):
        self.original = original
        self.__dict__.update(changes)

    def __getattr__(self, name):
        return getattr(self.original, name)


class ExecutionAmendment(RepairModel):
    version: int = 1
    job_id: str
    plan_sha256: str
    created_at: str
    reason: str = Field(min_length=1, max_length=400)
    initial_config_fingerprint: str
    initial_algorithm_fingerprint: str
    config_fingerprint: str
    algorithm_fingerprint: str
    retained_call_ids: list[int] = Field(min_length=1, max_length=10000)
    retained_calls_sha256: str
    failed_runs: dict[str, dict]


def _job_calls(plan, call_ids=None):
    with sqlite3.connect(Path(plan.db_path).resolve().as_uri() + "?mode=ro", uri=True) as connection:
        connection.row_factory = sqlite3.Row
        rows = connection.execute("""SELECT call_id,run_id,status,error_code,usage_json,request_sha256,response_sha256
            FROM report_model_calls WHERE json_extract(usage_json,'$.reserved_budget.job_id')=? ORDER BY call_id""",
            (plan.job_id,)).fetchall()
    return [dict(row) for row in rows if call_ids is None or row["call_id"] in call_ids]


def execution_amendment(plan, directory, identity):
    path = directory / "execution-amendment.json"
    if not path.exists():
        return None, None
    if is_redirect(path) or path.stat().st_size > 128 * 1024:
        raise ReportRunError("COVERAGE_EXECUTION_AMENDMENT_INVALID")
    try:
        envelope = json.loads(path.read_text(encoding="utf-8"))
        if set(envelope) != {"amendment", "sha256"} or envelope["sha256"] != fingerprint(envelope["amendment"]):
            raise ValueError()
        amendment = ExecutionAmendment.model_validate(envelope["amendment"])
        retained = _job_calls(plan, set(amendment.retained_call_ids))
        if (amendment.version != 1 or amendment.job_id != plan.job_id or amendment.plan_sha256 != identity
            or amendment.initial_config_fingerprint != plan.config_fingerprint
            or amendment.initial_algorithm_fingerprint != plan.algorithm_fingerprint
            or len(set(amendment.retained_call_ids)) != len(amendment.retained_call_ids)
            or len(retained) != len(amendment.retained_call_ids)
            or any(row["status"] != "failed" for row in retained)
            or fingerprint(retained) != amendment.retained_calls_sha256):
            raise ValueError()
        return amendment, envelope["sha256"]
    except (ValueError, KeyError, TypeError):
        raise ReportRunError("COVERAGE_EXECUTION_AMENDMENT_INVALID") from None


def _amendment_committed(journal, amendment, identity):
    actions = {(event["body"]["action"], event["body"]["date"])
        for event in journal.events if event["body"]["details"].get("amendment_sha256") == identity}
    return ("execution_amended", None) in actions and all(
        ("failed_execution_restarted", target) in actions for target in amendment.failed_runs)


def amend_failed_execution(config, path: Path, *, reason: str) -> Path:
    """Rebind a failed-only execution; preserve the frozen plan and shared ledger budget."""
    envelope = json.loads(path.read_text(encoding="utf-8"))
    plan = RepairPlan.model_validate(envelope["plan"])
    identity = fingerprint(envelope["plan"])
    directory = Path(config.data_dir) / "coverage_repair" / plan.job_id
    if envelope["sha256"] != identity or path.absolute() != (directory / "plan.json").absolute() or is_redirect(path):
        raise ReportRunError("COVERAGE_PLAN_CHANGED")
    config_identity = EvidenceDailyService(config)._config_fingerprint()
    algorithm_identity = algorithm_fingerprint(config)
    if (config_identity == plan.config_fingerprint and algorithm_identity == plan.algorithm_fingerprint
        or plan.pricing != (config.recovery_settings.model_pricing.model_dump(mode="json")
                            if config.recovery_settings.model_pricing else None)
        or plan.db_path != str(Path(config.db_dir) / "reports.sqlite3")
        or plan.reports_root != str(Path(config.reports_dir).absolute())
        or config.model_request_settings.max_request_utf8_bytes + 1024 + config.llm_config["max_tokens"] > plan.worst_tokens_per_call):
        raise ReportRunError("COVERAGE_EXECUTION_AMENDMENT_INVALID")
    with ReportLock(directory / "job", scope_id=plan.job_id), ReportLock(Path(plan.db_path)):
        journal = RepairEvents(directory, plan.job_id, identity)
        amendment, amendment_hash = execution_amendment(plan, directory, identity)
        if amendment is not None:
            if (amendment.config_fingerprint != config_identity or amendment.algorithm_fingerprint != algorithm_identity):
                raise ReportRunError("COVERAGE_EXECUTION_ALREADY_AMENDED")
            if _amendment_committed(journal, amendment, amendment_hash):
                return directory / "execution-amendment.json"
        else:
            calls = _job_calls(plan)
            if not calls or any(row["status"] != "failed" for row in calls):
                raise ReportRunError("COVERAGE_EXECUTION_NOT_FAILED_ONLY")
            failed_runs = {}
            with sqlite3.connect(Path(plan.db_path).resolve().as_uri() + "?mode=ro", uri=True) as connection:
                for item in plan.dates:
                    run_id = journal.state["dates"].get(item.date, {}).get("run_id")
                    if not run_id:
                        continue
                    run = connection.execute("SELECT status,run_kind FROM report_runs WHERE run_id=?", (run_id,)).fetchone()
                    if not run or run != ("failed", "preview"):
                        raise ReportRunError("COVERAGE_EXECUTION_NOT_FAILED_ONLY")
                    failed_runs[item.date] = {"run_id": run_id, "calls_used": sum(row["run_id"] == run_id for row in calls)}
            if {row["run_id"] for row in calls} != {row["run_id"] for row in failed_runs.values()}:
                raise ReportRunError("COVERAGE_EXECUTION_NOT_FAILED_ONLY")
            for item in plan.dates:
                for pin in item.files:
                    if _file_pin(Path(plan.work_root), pin, max(pin.size_bytes, 1)) != pin:
                        raise ReportRunError("COVERAGE_SOURCE_CHANGED")
            amendment = ExecutionAmendment(job_id=plan.job_id, plan_sha256=identity, created_at=datetime.now(UTC).isoformat(),
                reason=reason, initial_config_fingerprint=plan.config_fingerprint,
                initial_algorithm_fingerprint=plan.algorithm_fingerprint, config_fingerprint=config_identity,
                algorithm_fingerprint=algorithm_identity, retained_call_ids=[row["call_id"] for row in calls],
                retained_calls_sha256=fingerprint(calls), failed_runs=failed_runs)
            value = amendment.model_dump(mode="json")
            amendment_hash = fingerprint(value)
            write_atomic(directory / "execution-amendment.json", {"amendment": value, "sha256": amendment_hash})
        journal.commit("execution_amended", details={"amendment_sha256": amendment_hash,
            "retained_call_ids": amendment.retained_call_ids, "calls_limit": plan.calls_limit,
            "tokens_limit": plan.tokens_limit, "cost_limit_microunits": plan.cost_limit_microunits})
        for target, prior in amendment.failed_runs.items():
            journal.commit("failed_execution_restarted", target_date=target, run_id=prior["run_id"],
                updates={"run_id": None, "status": "planned", "calls_used": prior["calls_used"],
                         "calls_before_run": prior["calls_used"]},
                details={"amendment_sha256": amendment_hash, "previous_run_id": prior["run_id"], "calls_delta": 0})
    return directory / "execution-amendment.json"


def algorithm_fingerprint(config) -> str:
    root = Path(__file__).resolve().parents[2]
    paths = sorted((root / "src").rglob("*.py")) + sorted((root / "templates").glob("*.md"))
    hashes = {path.relative_to(root).as_posix(): sha256(path.read_bytes()).hexdigest() for path in paths}
    worker = Path(config.office_worker_path)
    if worker.is_file():
        hashes["office_worker"] = sha256(worker.read_bytes()).hexdigest()
    import ai_daily_scanner_native
    native_path = Path(ai_daily_scanner_native.__file__)
    hashes["native"] = sha256(native_path.read_bytes()).hexdigest()
    if native_path.suffix == ".py":
        for binary in native_path.parent.glob("*.pyd"):
            hashes["native/" + binary.name] = sha256(binary.read_bytes()).hexdigest()
    return fingerprint(hashes)


def _read_current(config, target: str) -> dict:
    db = Path(config.db_dir) / "reports.sqlite3"
    with sqlite3.connect(db.resolve().as_uri() + "?mode=ro", uri=True) as connection:
        connection.row_factory = sqlite3.Row
        row = connection.execute("""SELECT d.raw_json,d.current_run_id,r.expected_sha256,r.quality,r.status,r.phase
            FROM daily_reports d LEFT JOIN report_runs r ON r.run_id=d.current_run_id WHERE d.date=?""", (target,)).fetchone()
    if row is None or row["current_run_id"] is None or row["status"] != "succeeded" or row["phase"] != "published":
        raise ReportRunError("COVERAGE_CURRENT_UNBOUND")
    value = dict(row)
    value["report"] = EvidenceDailyReportData.model_validate_json(value.pop("raw_json"))
    return value


def _file_pin(root: Path, pin: FilePin, max_bytes: int) -> FilePin:
    path = root / pin.relative_path
    try:
        if any(component.exists() and is_redirect(component) for component in (path, *path.parents)):
            raise ReportRunError("COVERAGE_SOURCE_REDIRECTED")
        before = path.stat()
        if not 0 <= before.st_size <= max_bytes:
            raise ReportRunError("COVERAGE_SOURCE_SIZE")
        digest = sha256()
        read = 0
        with path.open("rb") as stream:
            for data in iter(lambda: stream.read(1 << 20), b""):
                read += len(data)
                if read > max_bytes:
                    raise ReportRunError("COVERAGE_SOURCE_SIZE")
                digest.update(data)
        after = path.stat()
        identity = lambda stat: (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns)
        if identity(before) != identity(after):
            raise ReportRunError("COVERAGE_SOURCE_CHANGED")
    except OSError:
        raise ReportRunError("COVERAGE_SOURCE_UNAVAILABLE") from None
    content_hash = digest.hexdigest()
    return pin.model_copy(update={"size_bytes": before.st_size, "mtime_ns": before.st_mtime_ns,
        "content_sha256": content_hash,
        "current_version_changed": (pin.prior_content_sha256 != content_hash
            if pin.prior_content_sha256 is not None else None)})


def make_plan(config, *, dates: list[str], gap_codes: list[str], approved_root: Path,
              calls_limit: int, tokens_limit: int, cost_limit_microunits: int | None = None,
              refresh: bool = False, scanner_factory=None) -> Path:
    if not dates or not gap_codes or len(set(dates)) != len(dates) or len(set(gap_codes)) != len(gap_codes):
        raise ValueError("explicit distinct dates and gap codes are required")
    work_root, approved_root = Path(config.work_dir).absolute(), approved_root.absolute()
    if not approved_root.is_relative_to(work_root):
        raise ReportRunError("COVERAGE_SCOPE_INVALID")
    planned = []
    for target in sorted(dates):
        parse_report_date(target)
        current = _read_current(config, target)
        report = current["report"]
        matched = sorted({gap.code for gap in report.gaps} & set(gap_codes))
        if not matched:
            raise ReportRunError("COVERAGE_DATE_HAS_NO_SELECTED_GAP")
        files = [FilePin(relative_path=source.relative_path, size_bytes=source.size_bytes,
            prior_content_sha256=source.content_sha256, content_sha256=None, mtime_ns=None)
            for source in report.sources if source.source_kind == "file"
            and (work_root / source.relative_path).is_relative_to(approved_root)]
        if not files:
            raise ReportRunError("COVERAGE_SCOPE_EMPTY")
        if len(files) != sum(source.source_kind == "file" for source in report.sources):
            raise ReportRunError("COVERAGE_SCOPE_INCOMPLETE")
        if refresh:
            files = [_file_pin(work_root, pin, int(config.scanner_settings().get("max_file_size_mb", 50)) * 1024 * 1024) for pin in files]
        planned.append(RepairDate(date=target, gap_codes=matched, old_run_id=current["current_run_id"],
            old_sha256=current["expected_sha256"], old_quality=report.quality, files=files))
    input_upper = int(config.model_request_settings.max_request_utf8_bytes) + 1024
    output_upper = int(config.llm_config["max_tokens"])
    pricing = config.recovery_settings.model_pricing
    if cost_limit_microunits is not None and pricing is None:
        raise ReportRunError("MODEL_PRICING_REQUIRED")
    plan = RepairPlan(job_id="coverage_" + uuid4().hex, created_at=datetime.now(UTC).isoformat(),
        work_root=str(work_root), approved_root=str(approved_root), db_path=str(Path(config.db_dir) / "reports.sqlite3"),
        reports_root=str(Path(config.reports_dir).absolute()), config_fingerprint=EvidenceDailyService(config)._config_fingerprint(),
        algorithm_fingerprint=algorithm_fingerprint(config), calls_limit=calls_limit, tokens_limit=tokens_limit,
        pricing=pricing.model_dump(mode="json") if pricing else None,
        cost_limit_microunits=cost_limit_microunits, worst_tokens_per_call=input_upper + output_upper,
        worst_cost_per_call_microunits=cost_microunits(input_upper, output_upper, pricing) if pricing else None,
        refreshed=refresh, dates=planned)
    directory = Path(config.data_dir) / "coverage_repair" / plan.job_id
    directory.mkdir(parents=True, exist_ok=False)
    envelope = {"plan": plan.model_dump(mode="json"), "sha256": fingerprint(plan.model_dump(mode="json"))}
    write_atomic(directory / "plan.json", envelope)
    with ReportLock(directory / "job", scope_id=plan.job_id):
        journal = RepairEvents(directory, plan.job_id, envelope["sha256"])
        journal.commit("planned", details={"calls_limit": calls_limit, "tokens_limit": tokens_limit,
            "cost_limit_microunits": cost_limit_microunits, "estimate": "upper_bound", "refreshed": refresh})
        if refresh:
            for item in plan.dates:
                scanner = _scanner(config, plan, item, directory, scanner_factory)
                result = scanner.build_context(ScanRequest("daily", date.fromisoformat(item.date), date.fromisoformat(item.date)))
                journal.commit("preflight_refreshed", target_date=item.date, updates={"status": "planned"},
                    details={"registered": len(result.sources), "scan_status": result.envelope.status, "model_calls": 0})
    return directory / "plan.json"


def load_plan(config, path: Path) -> tuple[RepairPlan, Path, str]:
    if path.stat().st_size > 2 * 1024 * 1024 or is_redirect(path):
        raise ReportRunError("COVERAGE_PLAN_INVALID")
    envelope = json.loads(path.read_text(encoding="utf-8"))
    if set(envelope) != {"plan", "sha256"} or envelope["sha256"] != fingerprint(envelope["plan"]):
        raise ReportRunError("COVERAGE_PLAN_CHANGED")
    plan = RepairPlan.model_validate(envelope["plan"])
    directory = Path(config.data_dir) / "coverage_repair" / plan.job_id
    if path.absolute() != (directory / "plan.json").absolute():
        raise ReportRunError("COVERAGE_JOB_LOCATION_CHANGED")
    amendment, _ = execution_amendment(plan, directory, envelope["sha256"])
    from .coverage_execution_recovery import execution_recovery
    recovery, _ = execution_recovery(plan, directory, envelope["sha256"])
    effective = recovery or amendment or plan
    if (effective.config_fingerprint != EvidenceDailyService(config)._config_fingerprint()
        or effective.algorithm_fingerprint != algorithm_fingerprint(config)
        or plan.pricing != (config.recovery_settings.model_pricing.model_dump(mode="json") if config.recovery_settings.model_pricing else None)
        or plan.db_path != str(Path(config.db_dir) / "reports.sqlite3")
        or plan.reports_root != str(Path(config.reports_dir).absolute())):
        raise ReportRunError("COVERAGE_CONFIG_CHANGED")
    return plan, directory, envelope["sha256"]


def history_priority_paths(item):
    """Keep depth for previously used, byte-identical sources; never reuse old claims."""
    return sorted(pin.relative_path for pin in item.files
        if pin.prior_content_sha256 is not None and pin.prior_content_sha256 == pin.content_sha256)


def _scanner(config, plan, item, directory, factory=None):
    root = directory / "sources" / item.date
    for pin in item.files:
        actual = _file_pin(Path(plan.work_root), pin, max(pin.size_bytes, 1))
        if actual.content_sha256 != pin.content_sha256 or actual.mtime_ns != pin.mtime_ns or actual.size_bytes != pin.size_bytes:
            raise ReportRunError("COVERAGE_SOURCE_CHANGED")
        target = root / pin.relative_path
        target.parent.mkdir(parents=True, exist_ok=True)
        if not target.exists():
            temporary = target.with_name(target.name + "." + uuid4().hex + ".tmp")
            try:
                shutil.copy2(Path(plan.work_root) / pin.relative_path, temporary)
                if sha256(temporary.read_bytes()).hexdigest() != pin.content_sha256:
                    raise ReportRunError("COVERAGE_SOURCE_CHANGED")
                temporary.rename(target)
            finally:
                temporary.unlink(missing_ok=True)
        elif is_redirect(target) or sha256(target.read_bytes()).hexdigest() != pin.content_sha256:
            raise ReportRunError("COVERAGE_STAGED_SOURCE_CHANGED")
    index = directory / "indexes" / item.date / "scan_index_v3.sqlite3"
    index.parent.mkdir(parents=True, exist_ok=True)
    priority_paths = history_priority_paths(item)
    priority_set = set(priority_paths)
    # Staged inputs live under the application's data_dir. Give this scan a
    # sibling runtime directory so generated-data exclusions keep nested inputs.
    overlay = RuntimeOverlay(config, work_dir=root, index_db_path=index,
        data_dir=directory / "runtime", scanner_settings=lambda: {
            **config.scanner_settings(), "excerpt_priority_paths": priority_paths})
    inner = factory(item) if factory else NativeScanner(ReportScannerConfig(overlay))
    zone = ZoneInfo("Asia/Shanghai")
    days = [datetime.fromtimestamp(pin.mtime_ns / 1e9, UTC).astimezone(zone).date() for pin in item.files]

    class FrozenScanner:
        def build_context(self, request):
            result = inner.build_context(ScanRequest("daily", min(days), max(days)))
            expected = {pin.relative_path: pin for pin in item.files}
            if {source.relative_path for source in result.sources} != set(expected):
                raise ReportRunError("COVERAGE_SCANNER_DROPPED_SOURCE")
            if any(source.relative_path not in expected or source.content_sha256 is not None
                   and source.content_sha256 != expected[source.relative_path].content_sha256
                   for source in result.sources):
                raise ReportRunError("COVERAGE_SCAN_SCOPE_CHANGED")
            sources = tuple(source.model_copy(update={"coverage_limits": {**source.coverage_limits,
                "history": {"current_version_changed": (expected[source.relative_path].current_version_changed
                    if expected[source.relative_path].prior_content_sha256 is not None else None),
                    "excerpt_priority": source.relative_path in priority_set,
                    "prior_content_sha256": expected[source.relative_path].prior_content_sha256}}}) for source in result.sources)
            return replace(result, sources=sources)
    return FrozenScanner()



def _current_report(config, plan: RepairPlan, item: RepairDate) -> tuple[dict, Path]:
    """核对数据库版本和磁盘内容，返回同一份正式报告。"""
    current = _read_current(config, item.date)
    if current["current_run_id"] != item.old_run_id or current["expected_sha256"] != item.old_sha256:
        raise ReportRunError("COVERAGE_CURRENT_CHANGED")
    path = Path(plan.reports_root) / item.date[:7] / (item.date + ".md")
    if _hash(path) != item.old_sha256:
        raise ReportRunError("COVERAGE_CURRENT_MODIFIED")
    return current, path


def _recover_candidate(app, prior_id: str | None, config) -> dict | None:
    """复用已完成候选，或恢复准备和生成阶段的候选。"""
    if prior_id:
        with ReportLock(app.db_path):
            app._open()
            run = app.runs.get(prior_id)
            if run["run_kind"] != "preview":
                raise ReportRunError("COVERAGE_CANDIDATE_KIND_CHANGED")
            if run["status"] == "succeeded" and run["report_json"] and run["markdown"]:
                return run
            if run["phase"] in {"prepared", "generating"}:
                app.runs.resume(prior_id)
                app._finish_guarded(prior_id, Deadline(config.recovery_settings.run_budget_seconds))
                run = app.runs.get(prior_id)
                return run
            if app.runs.calls(prior_id):
                raise ReportRunError("COVERAGE_CANDIDATE_NOT_RECOVERABLE")
            app.runs.fail(prior_id, "CANDIDATE_PREPARATION_INTERRUPTED")
    return None


def _write_candidate_files(directory: Path, target_date: str, candidate: dict, markdown: str, diff: str) -> None:
    prefix = directory / target_date
    _write_artifact(prefix.with_suffix(".candidate.json"), canonical_json({"candidate": candidate, "sha256": fingerprint(candidate)}) + b"\n")
    _write_artifact(prefix.with_suffix(".candidate.md"), markdown.encode("utf-8"))
    _write_artifact(prefix.with_suffix(".diff.md"), diff.encode("utf-8"))


def generate_candidates(config, path: Path, *, scanner_factory=None, transport_factory=None, workers=1) -> Path:
    plan, directory, plan_hash = load_plan(config, path)
    if not plan.refreshed or any(pin.content_sha256 is None for item in plan.dates for pin in item.files):
        raise ReportRunError("COVERAGE_PREFLIGHT_REQUIRED")
    if transport_factory is None and (plan.cost_limit_microunits is None or config.recovery_settings.model_pricing is None):
        raise ReportRunError("MODEL_PRICING_AND_COST_LIMIT_REQUIRED")
    with ReportLock(directory / "job", scope_id=plan.job_id):
        journal = RepairEvents(directory, plan.job_id, plan_hash)
        amendment, amendment_hash = execution_amendment(plan, directory, plan_hash)
        if amendment is not None and not _amendment_committed(journal, amendment, amendment_hash):
            raise ReportRunError("COVERAGE_EXECUTION_AMENDMENT_PENDING")
        from .coverage_execution_recovery import execution_recovery, recovery_committed
        recovery, recovery_hash = execution_recovery(plan, directory, plan_hash)
        if recovery is not None and not recovery_committed(journal, recovery, recovery_hash):
            raise ReportRunError("COVERAGE_EXECUTION_RECOVERY_PENDING")
        entries = []
        for item in plan.dates:
            progress = journal.state["dates"].get(item.date, {})
            inherited_calls = progress.get("calls_before_run", 0)
            _current_report(config, plan, item)
            app = EvidenceDailyService(config, transport_factory=transport_factory, model_workers=workers)
            app.job_budget = {"job_id": plan.job_id, "job_calls_limit": recovery.calls_limit if recovery is not None else plan.calls_limit,
                "job_tokens_limit": plan.tokens_limit, "job_cost_limit_microunits": plan.cost_limit_microunits}
            if recovery is not None and recovery.budget_revision_sha256 is not None:
                app.job_budget["job_calls_revision_sha256"] = recovery.budget_revision_sha256
            app.on_run_created = lambda run_id: journal.commit("run_registered", target_date=item.date, run_id=run_id,
                updates={"run_id": run_id, "status": "generating"}, details={"calls_delta": 0})
            prior_id = progress.get("run_id")
            try:
                run = _recover_candidate(app, prior_id, config)
                if run is None:
                    app.scanner = _scanner(config, plan, item, directory, scanner_factory)
                    outcome = app.run(RunContext.create(now=datetime.now(UTC), target_date=date.fromisoformat(item.date)),
                        save=False, regenerate_reason="coverage_candidate:" + plan.job_id)
                    run = app.runs.get(outcome.run_id)
                report = EvidenceDailyReportData.model_validate_json(run["report_json"])
                old, old_path = _current_report(config, plan, item)
                diff = "".join(unified_diff(old_path.read_text(encoding="utf-8").splitlines(keepends=True),
                    run["markdown"].splitlines(keepends=True), fromfile="current", tofile="candidate"))
                candidate = {"version": 1, "job_id": plan.job_id, "plan_sha256": plan_hash,
                    "date": item.date, "run_id": run["run_id"], "generation_id": run["generation_id"],
                    "input_fingerprint": run["input_fingerprint"], "snapshot_sha256": run["snapshot_sha256"],
                    "old_run_id": item.old_run_id, "old_sha256": item.old_sha256,
                    "report": report.model_dump(mode="json"), "markdown_sha256": run["expected_sha256"],
                    "old_metrics": summarize_coverage(old["report"].sources).model_dump(),
                    "candidate_metrics": summarize_coverage(report.sources, report.summary_items + report.verification_items).model_dump(),
                    "model_calls": len(app.runs.calls(run["run_id"])), "diff_sha256": sha256(diff.encode()).hexdigest()}
                if amendment is not None:
                    candidate["execution_amendment_sha256"] = amendment_hash
                    candidate["execution_algorithm_fingerprint"] = amendment.algorithm_fingerprint
                    candidate["execution_config_fingerprint"] = amendment.config_fingerprint
                if recovery is not None:
                    candidate["execution_recovery_sha256"] = recovery_hash
                    candidate["execution_algorithm_fingerprint"] = recovery.algorithm_fingerprint
                    candidate["execution_config_fingerprint"] = recovery.config_fingerprint
                    candidate["execution_calls_limit"] = recovery.calls_limit
                reference = app.snapshots.put(run["run_id"], "coverage-candidate", candidate)
                _write_candidate_files(directory, item.date, candidate, run["markdown"], diff)
                entry = {"date": item.date, "run_id": run["run_id"], "sha256": fingerprint(candidate),
                    "snapshot_path": reference.path, "snapshot_sha256": reference.sha256}
                entries.append(entry)
                previous_calls = progress.get("calls_used", 0)
                if progress.get("status") == "candidate" and progress.get("candidate_sha256") == entry["sha256"]:
                    continue
                journal.commit("candidate_saved", target_date=item.date, run_id=run["run_id"],
                    updates={"status": "candidate", "quality": report.quality, "calls_used": inherited_calls + candidate["model_calls"],
                        "candidate_sha256": entry["sha256"]}, details={"calls_delta": inherited_calls + candidate["model_calls"] - previous_calls,
                            "old_run_id": item.old_run_id, "candidate_run_id": run["run_id"], "metrics": candidate["candidate_metrics"]})
            except BaseException as error:
                failed_id = journal.state["dates"].get(item.date, {}).get("run_id")
                used = inherited_calls + (len(app.runs.calls(failed_id)) if failed_id and hasattr(app, "runs") else 0)
                journal.commit("candidate_failed", target_date=item.date, run_id=failed_id,
                    updates={"status": "failed", "calls_used": used}, details={
                        "calls_delta": used - progress.get("calls_used", 0),
                        "error_code": error.code if isinstance(error, ReportRunError) else "CANDIDATE_EXECUTION_FAILED"})
                raise
        manifest = {"version": 1, "job_id": plan.job_id, "plan_sha256": plan_hash, "candidates": entries}
        write_atomic(directory / "candidates.json", {"manifest": manifest, "sha256": fingerprint(manifest)})
    return directory / "candidates.json"


def _write_artifact(path: Path, data: bytes):
    if path.exists():
        if is_redirect(path) or path.read_bytes() != data:
            raise ReportRunError("COVERAGE_CANDIDATE_ARTIFACT_CHANGED")
        return
    import os
    temporary = path.parent / (".candidate-" + uuid4().hex)
    try:
        with temporary.open("xb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        try:
            os.link(temporary, path)
        except FileExistsError:
            if is_redirect(path) or path.read_bytes() != data:
                raise ReportRunError("COVERAGE_CANDIDATE_ARTIFACT_CHANGED") from None
    finally:
        temporary.unlink(missing_ok=True)


def publish_candidates(config, path: Path) -> list[str]:
    if path.stat().st_size > 2 * 1024 * 1024 or is_redirect(path):
        raise ReportRunError("COVERAGE_CANDIDATE_MANIFEST_INVALID")
    envelope = json.loads(path.read_text(encoding="utf-8"))
    manifest = envelope["manifest"]
    if envelope["sha256"] != fingerprint(manifest):
        raise ReportRunError("COVERAGE_CANDIDATE_CHANGED")
    plan, directory, plan_hash = load_plan(config, path.parent / "plan.json")
    if manifest["job_id"] != plan.job_id or manifest["plan_sha256"] != plan_hash or path.absolute() != (directory / "candidates.json").absolute():
        raise ReportRunError("COVERAGE_CANDIDATE_CHANGED")
    planned = {item.date: item for item in plan.dates}
    if len(manifest["candidates"]) != len({item["date"] for item in manifest["candidates"]}):
        raise ReportRunError("COVERAGE_CANDIDATE_CHANGED")
    published = []
    with ReportLock(directory / "job", scope_id=plan.job_id):
        journal = RepairEvents(directory, plan.job_id, plan_hash)
        app = EvidenceDailyService(config)
        with ReportLock(app.db_path):
            app._open()
            for entry in manifest["candidates"]:
                item = planned.get(entry["date"])
                if item is None:
                    raise ReportRunError("COVERAGE_CANDIDATE_CHANGED")
                candidate = app.snapshots.read(SnapshotRef(entry["snapshot_path"], entry["snapshot_sha256"]))
                local = json.loads((directory / (item.date + ".candidate.json")).read_text(encoding="utf-8"))
                preview = app.runs.get(entry["run_id"])
                markdown = (directory / (item.date + ".candidate.md")).read_text(encoding="utf-8")
                if (fingerprint(candidate) != entry["sha256"] or local != {"candidate": candidate, "sha256": entry["sha256"]}
                    or preview["run_kind"] != "preview" or preview["status"] != "succeeded"
                    or candidate["plan_sha256"] != plan_hash or candidate["run_id"] != preview["run_id"]
                    or candidate["generation_id"] != preview["generation_id"] or candidate["input_fingerprint"] != preview["input_fingerprint"]
                    or candidate["snapshot_sha256"] != preview["snapshot_sha256"]
                    or candidate["old_run_id"] != item.old_run_id or candidate["old_sha256"] != item.old_sha256
                    or sha256(markdown.encode()).hexdigest() != candidate["markdown_sha256"]
                    or _hash(directory / (item.date + ".diff.md")) != candidate["diff_sha256"]
                    or markdown != preview["markdown"] or candidate["report"] != json.loads(preview["report_json"])):
                    raise ReportRunError("COVERAGE_CANDIDATE_CHANGED")
                report = EvidenceDailyReportData.model_validate(candidate["report"])
                progress = journal.state["dates"].get(item.date, {})
                run_id = progress.get("publish_run_id")
                if run_id is None:
                    current = app.runs.current(item.date)
                    if current is None or current["run_id"] != item.old_run_id or _hash(app.publisher.output_path(item.date)) != item.old_sha256:
                        raise ReportRunError("COVERAGE_CURRENT_CHANGED")
                    context = replace(RunContext.from_dict(json.loads(preview["context_json"])), run_id=uuid4().hex,
                        started_at=datetime.now(UTC))
                    app.runs.create(context, regeneration_reason="coverage_publish:" + preview["run_id"])
                    run_id = context.run_id
                    journal.commit("publish_registered", target_date=item.date, run_id=run_id,
                        updates={"publish_run_id": run_id}, details={"candidate_run_id": preview["run_id"], "calls_delta": 0})
                run = app.runs.get(run_id)
                if run["regeneration_reason"] != "coverage_publish:" + preview["run_id"]:
                    raise ReportRunError("COVERAGE_PUBLICATION_BINDING_CHANGED")
                if app.runs.calls(run_id):
                    raise ReportRunError("COVERAGE_PUBLICATION_HAS_MODEL_CALLS")
                if run["phase"] == "created":
                    reference = app.snapshots.put(run_id, "coverage-publication", candidate)
                    app.runs.prepare(run_id, reference, fingerprint({"candidate": entry["sha256"], "plan": plan_hash}), report.sources)
                    run = app.runs.get(run_id)
                if run["phase"] in {"prepared", "generating"}:
                    current = app.runs.current(item.date)
                    if current is None or current["run_id"] != item.old_run_id or _hash(app.publisher.output_path(item.date)) != item.old_sha256:
                        raise ReportRunError("COVERAGE_CURRENT_CHANGED")
                    if run["status"] != "running":
                        app.runs.resume(run_id)
                    app.runs.stage_report(run_id, report, markdown, output_path=str(app.publisher.output_path(item.date)),
                        publication_fingerprint=fingerprint({"candidate": entry["sha256"], "preview": preview["run_id"]}))
                try:
                    app.publisher.publish(run_id)
                except BaseException as error:
                    journal.commit("publication_failed", target_date=item.date, run_id=run_id,
                        details={"old_run_id": item.old_run_id, "candidate_run_id": preview["run_id"],
                            "publish_run_id": run_id, "calls_delta": 0,
                            "error_code": error.code if isinstance(error, ReportRunError)
                            else "COVERAGE_PUBLICATION_FAILED"})
                    raise
                journal.commit("published", target_date=item.date, run_id=run_id,
                    updates={"status": "published", "quality": report.quality},
                    details={"old_run_id": item.old_run_id, "candidate_run_id": preview["run_id"], "publish_run_id": run_id,
                        "calls_delta": 0, "markdown_sha256": candidate["markdown_sha256"]})
                published.append(item.date)
    return published
