"""Daily evidence workflow: freeze, scan, persist, generate, stage, then publish."""

from __future__ import annotations

from dataclasses import dataclass, replace
from collections import defaultdict
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
from datetime import UTC, datetime
from hashlib import sha256
import json
import os
from pathlib import Path
from threading import Event
from typing import Any, Callable
from urllib.parse import urlsplit

from ...models.evidence_report import CoverageGap, EvidenceDailyReportData, EvidenceSummaryData, ReportSource
from ...models.coverage import source_audit
from ..native_scanner import NativeScanner
from ..report_gen import ReportGenerator
from ..report_run_store import ReportRunStore
from ..scan_preview import ReportScannerConfig
from ..sqlite_store import SQLiteStore
from .context import RunContext
from .errors import ReportRunError
from .locking import ReportLock
from .model_calls import AuditedModel, Deadline, LazyModelTransport, OpenAITransport
from .model_plan import PlannedBatch, merge_summaries, plan_evidence
from .model_slicing import batch_ranges, union_chars
from .publication import ReportPublisher
from .retention import cleanup_snapshots
from .scan_evidence import coverage_gaps, scan_with_retry
from .snapshots import SnapshotRef, SnapshotStore, fingerprint, is_redirect


@dataclass(frozen=True, slots=True)
class EvidenceDailyOutcome:
    run_id: str
    target_date: str
    quality: str
    markdown: str
    output_path: Path | None
    action: str
    model_calls: int


class EvidenceDailyService:
    def __init__(self, runtime_config: Any, *, scanner=None, transport_factory: Callable | None = None,
                 clock: Callable[[], datetime] | None = None, model_workers: int = 1):
        if not 1 <= model_workers <= 4:
            raise ValueError("model workers must be between one and four")
        self.config = runtime_config
        self.model_workers = model_workers
        self.scanner = scanner
        self.transport_factory = transport_factory
        self.clock = clock or (lambda: datetime.now(UTC))
        self.db_path = Path(runtime_config.db_dir) / "reports.sqlite3"
        self.templates = Path(__file__).resolve().parents[3] / "templates"
        self.job_budget: dict = {}
        self.on_run_created: Callable | None = None

    def _config_fingerprint(self) -> str:
        return fingerprint({"report": self.config.report_settings.model_dump(), "llm": self.config.llm_config,
            "model_requests": self.config.model_request_settings.fingerprint_data(),
            "provider": self.config.llm_provider, "scanner": ReportScannerConfig(self.config).scanner_settings(),
            "work_dir": str(Path(self.config.work_dir).resolve())})

    def _legacy_config_fingerprint(self) -> str:
        return fingerprint({"report": self.config.report_settings.model_dump(), "llm": self.config.llm_config,
            "model_requests": self.config.model_request_settings.fingerprint_data(exclude={
                "truncation_policy", "max_strategy_changes"}),
            "provider": self.config.llm_provider, "scanner": ReportScannerConfig(self.config).scanner_settings(),
            "work_dir": str(Path(self.config.work_dir).resolve())})

    def _recovery_fingerprint(self) -> str:
        """Sending availability may change; frozen input and output meaning may not."""
        return fingerprint({"version": 1, "report": self.config.report_settings.model_dump(),
            "llm": {key: value for key, value in self.config.llm_config.items() if key != "max_retries"},
            "provider": self.config.llm_provider,
            "scanner": ReportScannerConfig(self.config).scanner_settings(),
            "work_dir": str(Path(self.config.work_dir).resolve()),
            "model_requests": self.config.model_request_settings.fingerprint_data(exclude={
                "context_window_tokens", "verified_max_output_tokens", "request_timeout_seconds", "max_retries"})})

    def _model_client(self, deadline: Deadline, payload: dict | None = None) -> AuditedModel:
        from ...models.report_settings import ModelPricing
        frozen = (payload or {}).get("model_pricing")
        return AuditedModel(self.runs, self.snapshots, transport=LazyModelTransport(self._transport),
            settings=self.config.model_request_settings, limits=self.config.recovery_settings, deadline=deadline,
            output_tokens_upper=self.config.llm_config["max_tokens"],
            frozen_pricing=ModelPricing.model_validate(frozen) if frozen is not None else None,
            job_budget=(payload or {}).get("job_budget", self.job_budget),
            reused_calls=(payload or {}).get("reused_model_calls"), prior_calls=(payload or {}).get("prior_model_calls"))

    def _local_recovery_compatible(self, run: dict, deadline: Deadline) -> bool:
        payload = self.snapshots.read(SnapshotRef(run["snapshot_path"], run["snapshot_sha256"]))
        if payload.get("recovery_fingerprint") != self._recovery_fingerprint():
            return False
        client = self._model_client(deadline, payload)
        return all(client.cached_result(run["run_id"], PlannedBatch.model_validate(batch)) is not None
                   for batch in payload["batches"])

    def _open(self) -> None:
        self.runs = ReportRunStore(SQLiteStore(self.db_path, report_lock_held=True), clock=self.clock)
        self.snapshots = SnapshotStore(Path(self.config.data_dir) / "run_snapshots",
            max_bytes=self.config.retention_settings.snapshot_max_mib * 1024 * 1024)
        self.publisher = ReportPublisher(Path(self.config.reports_dir), self.runs)
        self.renderer = ReportGenerator(Path(self.config.reports_dir))
        self.runs.recover_interrupted_bookkeeping()
        cleanup_snapshots(self.runs, self.snapshots, self.config.retention_settings, now=self.clock())

    def _notes(self, context: RunContext) -> str:
        root = Path(self.config.report_settings.notes_dir)
        if not root.is_absolute():
            root = Path(self.config.project_root) / root
        path = root / f"{context.target_date}.md"
        for component in (path, *path.parents):
            if component.exists() and is_redirect(component):
                raise ReportRunError("NOTES_PATH_REDIRECTED")
        try:
            with path.open("rb") as stream:
                data = stream.read(65537)
        except FileNotFoundError:
            return ""
        if len(data) > 65536:
            raise ReportRunError("NOTES_SIZE_EXCEEDED")
        try:
            return data.decode("utf-8-sig").replace("\r\n", "\n").replace("\r", "\n")
        except UnicodeError:
            raise ReportRunError("NOTES_ENCODING_INVALID") from None

    def _transport(self):
        if self.transport_factory:
            return self.transport_factory()
        llm = self.config.llm_config
        if not llm["base_url"]:
            raise ReportRunError("MODEL_BASE_URL_REQUIRED")
        if self.config.llm_provider == "deepseek" and urlsplit(llm["base_url"]).hostname == "ai-gateway.fsdev.cn":
            key = os.environ.get("DEEPSEEK_API_KEY", "")
        elif self.config.llm_provider == "deepseek":
            key = self.config.deepseek_api_key
        elif self.config.llm_provider == "openai":
            key = self.config.openai_api_key
        else:
            raise ReportRunError("MODEL_PROVIDER_INVALID")
        if not key:
            raise ReportRunError("MODEL_CREDENTIAL_MISSING")
        return OpenAITransport(base_url=llm["base_url"], api_key=key,
                               timeout=self.config.model_request_settings.request_timeout_seconds)

    def _scanner(self):
        if self.scanner is not None:
            return self.scanner
        index = Path(self.config.index_db_path)
        if not index.is_absolute():
            index = Path(self.config.project_root) / index
        index.parent.mkdir(parents=True, exist_ok=True)
        return NativeScanner(ReportScannerConfig(self.config))

    def _prepare_input(self, context: RunContext, scan, *, user_input: str = "", model_settings=None):
        """Freeze evidence and output meaning without preparing or sending a model request."""
        gaps = coverage_gaps(scan, root=Path(self.config.work_dir))
        prompt = (self.templates / "evidence_summary_prompt.md").read_text(encoding="utf-8")
        plan = plan_evidence(context, scan.sources, user_input=user_input or "", system_prompt=prompt,
            llm_config={**self.config.llm_config, "provider": self.config.llm_provider},
            settings=model_settings or self.config.model_request_settings,
            scanner_profile={"work_root": str(Path(self.config.work_dir).resolve()),
                             "settings": ReportScannerConfig(self.config).scanner_settings()},
            gaps=[gap.model_dump(mode="json") for gap in gaps])
        for source in plan.sources:
            if source.model_truncated:
                gaps.append(CoverageGap(code="MODEL_INPUT_BOUNDED", stage="model", source_id=source.source_id,
                    message="受模型请求数量或文本体积上限影响，此来源部分内容未发送。"))
        has_evidence = any(source.model_chars for source in plan.sources)
        quality = "partial" if gaps else "complete"
        if not has_evidence:
            quality = "no_changes" if not plan.sources and not gaps else "unavailable"
        if scan.envelope.status == "error" and not user_input.strip():
            quality = "unavailable"
        payload = {"version": 2, "context": context.to_dict(), "quality": quality,
            "job_budget": self.job_budget, "recovery_fingerprint": self._recovery_fingerprint(),
            "model_pricing": self.config.recovery_settings.model_pricing.model_dump(mode="json")
                if self.config.recovery_settings.model_pricing is not None else None,
            "sources": [source.model_dump(mode="json") for source in plan.sources],
            "gaps": [gap.model_dump(mode="json") for gap in gaps],
            "batches": [batch.model_dump(mode="json") for batch in plan.batches],
            "input_fingerprint": plan.input_fingerprint,
            "missed_dates": self.runs.missed_dates(context.target_date),
            "template": (self.templates / "evidence_report_template.md").read_text(encoding="utf-8"),
            "notes": self._notes(context), "root_alias": self.config.report_settings.root_alias}
        return plan, payload

    def run(self, context: RunContext, *, user_input: str = "", save: bool = True,
            scheduled: bool = False, regenerate_reason: str | None = None) -> EvidenceDailyOutcome:
        limits = self.config.recovery_settings
        with ReportLock(self.db_path, wait_seconds=limits.lock_wait_seconds):
            self._open()
            deadline = Deadline(limits.run_budget_seconds)
            context = replace(context, config_fingerprint=self._config_fingerprint())
            if scheduled:
                self.runs.activate(context.target_date)
            if save and not regenerate_reason:
                pending = self.runs.pending(target_date=None if scheduled else str(context.target_date))
                for run in pending:
                    # Recovery uses the old frozen context even after midnight. A changed
                    # model/configuration cannot silently reinterpret prepared requests.
                    if (run["phase"] != "db_committed" and run["config_fingerprint"] != context.config_fingerprint
                            and run["config_fingerprint"] != self._legacy_config_fingerprint()
                            and not self._local_recovery_compatible(run, deadline)):
                        raise ReportRunError("RECOVERY_CONFIG_CHANGED")
                    self.runs.resume(run["run_id"])
                    recovered = self._finish_guarded(run["run_id"], deadline)
                    if run["target_date"] == str(context.target_date) and not user_input:
                        return recovered
            self.runs.create(context, kind="report" if save else "preview", regeneration_reason=regenerate_reason)
            try:
                if self.on_run_created is not None:
                    self.on_run_created(context.run_id)
                self.runs.set_scanning(context.run_id)
                scan = scan_with_retry(self._scanner(), context, deadline)
                plan, payload = self._prepare_input(context, scan, user_input=user_input)
                quality = payload["quality"]
                reference = self.snapshots.put(context.run_id, "input", payload)
                if quality == "unavailable":
                    self.runs.scan_failure(context.run_id, reference, list(plan.sources))
                    self.runs.fail(context.run_id, "SCAN_UNAVAILABLE", quality="unavailable")
                    raise ReportRunError("SCAN_UNAVAILABLE")
                self.runs.prepare(context.run_id, reference, plan.input_fingerprint, list(plan.sources))
                publication_hash = self._publication_fingerprint(payload)
                current = self.runs.current(str(context.target_date)) if save and not regenerate_reason else None
                if current and current["input_fingerprint"] == plan.input_fingerprint:
                    if current["publication_fingerprint"] == publication_hash:
                        path = self.publisher.publish(current["run_id"])
                        self.runs.finish_observation(context.run_id, current["run_id"], current["quality"])
                        return EvidenceDailyOutcome(context.run_id, str(context.target_date), current["quality"],
                                                    current["markdown"], path, "reused", 0)
                    old = EvidenceDailyReportData.model_validate_json(current["report_json"])
                    revised = old.model_copy(update={"user_notes": payload["notes"], "root_alias": payload["root_alias"],
                                                     "missed_dates": payload["missed_dates"]})
                    return self._stage_publish(context.run_id, revised, payload)
                return self._finish(context.run_id, deadline)
            except KeyboardInterrupt:
                self.runs.fail(context.run_id, "RUN_CANCELLED", cancelled=True)
                raise
            except Exception as error:
                self.runs.fail(context.run_id, error.code if isinstance(error, ReportRunError) else "REPORT_EXECUTION_FAILED")
                raise

    @staticmethod
    def _publication_fingerprint(payload: dict) -> str:
        return fingerprint({"input": payload["input_fingerprint"], "template": payload["template"],
                            "notes": payload["notes"], "missed_dates": payload["missed_dates"],
                            "root_alias": payload["root_alias"]})

    def _finish_guarded(self, run_id: str, deadline: Deadline) -> EvidenceDailyOutcome:
        try:
            return self._finish(run_id, deadline)
        except KeyboardInterrupt:
            self.runs.fail(run_id, "RUN_CANCELLED", cancelled=True)
            raise
        except Exception as error:
            self.runs.fail(run_id, error.code if isinstance(error, ReportRunError) else "REPORT_EXECUTION_FAILED")
            raise

    def _finish(self, run_id: str, deadline: Deadline) -> EvidenceDailyOutcome:
        deadline.remaining()
        run = self.runs.get(run_id)
        if run["phase"] == "db_committed":
            path = self.publisher.publish(run_id)
            return EvidenceDailyOutcome(run_id, run["target_date"], run["quality"], run["markdown"], path,
                                        "repaired", len(self.runs.calls(run_id)))
        payload = self.snapshots.read(SnapshotRef(run["snapshot_path"], run["snapshot_sha256"]))
        if payload.get("version") not in {1, 2}:
            raise ReportRunError("SNAPSHOT_VERSION_UNSUPPORTED")
        context = RunContext.from_dict(payload["context"])
        if context.run_id != run_id or payload["input_fingerprint"] != run["input_fingerprint"]:
            raise ReportRunError("SNAPSHOT_CONTEXT_MISMATCH")
        batches = [PlannedBatch.model_validate(batch) for batch in payload["batches"]]
        results = []
        sources = [ReportSource.model_validate(source) for source in payload["sources"]]
        gaps = [CoverageGap.model_validate(gap) for gap in payload["gaps"]]
        quality = payload["quality"]
        if batches:
            client = self._model_client(deadline, payload)
            results = generate_model_batches(client, run_id, batches, workers=self.model_workers)
            if all(batch.response_schema_version == 2 for batch in batches):
                effective = [client.effective_batch(run_id, batch)[0] for batch in batches]
                actual_chars = defaultdict(int)
                successful_ranges = defaultdict(list)
                known_ranges = all(batch.input_range_version in {1, 2, 3} for batch in effective)
                for batch in effective:
                    for source_id, chars in batch.source_chars.items():
                        actual_chars[source_id] += chars
                    for source_id, ranges in (batch_ranges(batch) or {}).items():
                        successful_ranges[source_id].extend({**range_, "logical_step": batch.logical_step,
                            "revision": batch.revision} for range_ in ranges)
                attempted_ranges = defaultdict(list)
                sent_unknown = set()
                for call in client.audited_calls(run_id):
                    request_audit = json.loads(call["usage_json"] or "{}").get("reserved_budget", {}).get("request_audit", {})
                    for source_id, ranges in (request_audit.get("source_ranges") or {}).items():
                        attempted_ranges[source_id].extend(ranges)
                        if call["status"] in {"reserved", "unknown"}:
                            sent_unknown.add(source_id)
                effective_hash = fingerprint([batch.model_dump(mode="json") for batch in effective])
                updated = []
                for source in sources:
                    audit = source_audit(source).model_dump(mode="json")
                    audit.update(planned_model_chars=source.model_chars, effective_plan_sha256=effective_hash)
                    if known_ranges:
                        ranges = successful_ranges[source.source_id]
                        if any(range_["end"] > source.excerpt_chars for range_ in ranges + attempted_ranges[source.source_id]):
                            raise ReportRunError("MODEL_SOURCE_RANGE_MISMATCH")
                        actual_chars[source.source_id] = union_chars(ranges)
                        audit.update(model_ranges=ranges,
                            attempted_model_chars=union_chars(attempted_ranges[source.source_id]),
                            sent_unknown=source.source_id in sent_unknown)
                    truncated = actual_chars[source.source_id] < source.excerpt_chars
                    if truncated:
                        audit["secondary_tags"] = sorted(set(audit["secondary_tags"]) | {"MODEL_INPUT_BOUNDED"})
                    updated.append(source.model_copy(update={"model_chars": actual_chars[source.source_id],
                        "model_truncated": truncated, "coverage_limits": {**source.coverage_limits, "evidence_audit": audit}}))
                    if truncated and not any(gap.code == "MODEL_INPUT_BOUNDED" and gap.source_id == source.source_id for gap in gaps):
                        gaps.append(CoverageGap(code="MODEL_INPUT_BOUNDED", stage="model", source_id=source.source_id,
                            message="实际成功批次仅包含此来源的部分摘录；其余范围未被模型成功处理。"))
                sources = updated
                if gaps:
                    quality = "partial"
        summary = merge_summaries(results, bounded=all(batch.response_schema_version == 2 for batch in batches)) if results else EvidenceSummaryData(summary_items=[], verification_items=[])
        unverified = {source.source_id for source in sources if
            source.coverage_limits.get("evidence_audit", {}).get("text_unverified")
            or source.coverage_limits.get("ocr", {}).get("text_unverified")}
        if unverified:
            pending = [item for item in summary.summary_items if set(item.source_ids) & unverified]
            retained = [item for item in summary.summary_items if not set(item.source_ids) & unverified]
            from ...models.evidence_report import EvidenceMergedSummaryData
            unique = {(item.text, tuple(sorted(item.source_ids))): item for item in summary.verification_items + pending}
            summary = EvidenceMergedSummaryData(summary_items=retained, verification_items=list(unique.values()))
        report = EvidenceDailyReportData(**summary.model_dump(), date=str(context.target_date),
            window_start_utc=context.window_start_utc.isoformat(), window_end_utc=context.window_end_utc.isoformat(),
            quality=quality, sources=sources, gaps=gaps, missed_dates=payload["missed_dates"],
            generated_at=self.clock().astimezone(UTC).isoformat(), user_notes=payload["notes"], root_alias=payload["root_alias"])
        deadline.remaining()
        if run["run_kind"] == "preview":
            markdown = self._render(payload, report)
            self.runs.finish_preview(run_id, report.quality, report=report, markdown=markdown)
            return EvidenceDailyOutcome(run_id, report.date, report.quality, markdown, None, "preview", len(self.runs.calls(run_id)))
        return self._stage_publish(run_id, report, payload)

    def _render(self, payload: dict, report: EvidenceDailyReportData) -> str:
        return self.renderer.env.from_string(payload["template"]).render(report=report).replace("\r\n", "\n").replace("\r", "\n").rstrip() + "\n"

    def _stage_publish(self, run_id: str, report: EvidenceDailyReportData, payload: dict) -> EvidenceDailyOutcome:
        markdown = self._render(payload, report)
        self.runs.stage_report(run_id, report, markdown, output_path=str(self.publisher.output_path(report.date)),
                              publication_fingerprint=self._publication_fingerprint(payload))
        path = self.publisher.publish(run_id)
        return EvidenceDailyOutcome(run_id, report.date, report.quality, markdown, path, "published", len(self.runs.calls(run_id)))

    def repair(self, target_date: str) -> EvidenceDailyOutcome:
        with ReportLock(self.db_path, wait_seconds=self.config.recovery_settings.lock_wait_seconds):
            self._open()
            pending = [run for run in self.runs.pending(target_date=target_date) if run["phase"] == "db_committed"]
            run = pending[0] if pending else self.runs.current(target_date)
            if run is None:
                raise ReportRunError("REPORT_NOT_STAGED")
            path = self.publisher.publish(run["run_id"])
            return EvidenceDailyOutcome(run["run_id"], target_date, run["quality"], run["markdown"], path, "repaired", 0)


def generate_model_batches(client, run_id: str, batches: list[PlannedBatch], *, workers: int = 1):
    """Keep ordered results and finish in-flight audit writes before propagating errors."""
    results = [None] * len(batches)
    cache = getattr(client, "cached_result", None)
    missing = []
    for index, batch in enumerate(batches):
        result = cache(run_id, batch) if cache is not None else None
        if result is None:
            missing.append(index)
        else:
            results[index] = result
    if not missing:
        return results
    if workers == 1 or len(missing) <= 1:
        for index in missing:
            results[index] = client.generate(run_id, batches[index])
        return results
    check = getattr(client, "check_batch", None)
    if check is not None:
        for index in missing:
            check(batches[index])
    client.transport.prepare()
    pending = {}
    next_index = 0
    failed = Event()

    def generate(batch):
        try:
            return client.generate(run_id, batch)
        except BaseException:
            failed.set()
            raise

    with ThreadPoolExecutor(max_workers=workers) as executor:
        try:
            while next_index < len(missing) or pending:
                while next_index < len(missing) and len(pending) < workers and not failed.is_set():
                    index = missing[next_index]
                    future = executor.submit(generate, batches[index])
                    pending[future] = index
                    next_index += 1
                completed, _ = wait(pending, return_when=FIRST_COMPLETED)
                for future in completed:
                    results[pending.pop(future)] = future.result()
        except BaseException:
            for future in pending:
                future.cancel()
            raise
    return results
