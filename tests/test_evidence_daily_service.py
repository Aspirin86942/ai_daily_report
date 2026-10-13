from datetime import UTC, datetime, timedelta
from hashlib import sha256
import json
import os
from pathlib import Path
from types import SimpleNamespace

import pytest

from src.models.report_settings import ModelRequestSettings, RecoverySettings, ReportSettings, RetentionSettings
from src.models.scan_sources import ScanSource
from src.services.native_scanner import ScanResult
from src.services.report_runner.context import RunContext
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.evidence_daily import EvidenceDailyService
from src.services.report_runner.snapshots import SnapshotStore


NOW = datetime(2026, 10, 12, 1, tzinfo=UTC)


@pytest.fixture
def config(tmp_path):
    work = tmp_path / "work"
    work.mkdir()
    return SimpleNamespace(project_root=tmp_path, work_dir=work, data_dir=tmp_path / "data",
        reports_dir=tmp_path / "data/reports", db_dir=tmp_path / "data/db", log_dir=tmp_path / "logs",
        index_db_path=tmp_path / "data/db/scan_index_v3.sqlite3",
        office_worker_path=Path(__file__).parents[1] / "rust/target/release/ai-daily-office-parser.exe",
        scanner_settings=lambda: {"allowed_extensions": [".md"], "total_deadline_ms": 30000, "total_max_chars": 50000},
        llm_provider="deepseek", llm_config={"base_url": "https://gateway.example.invalid", "model_id": "deepseek-flash",
            "temperature": 0.2, "max_tokens": 2048, "max_retries": 3},
        report_settings=ReportSettings(), recovery_settings=RecoverySettings(), retention_settings=RetentionSettings(),
        model_request_settings=ModelRequestSettings(context_window_tokens=64000, verified_max_output_tokens=4096))


def source(text="方案记载计划于下周验收。", **changes):
    digest = sha256(text.encode()).hexdigest()
    data = dict(source_id="src_" + sha256(b"work.md").hexdigest(), relative_path="work.md", extension=".md",
        size_bytes=len(text.encode()), modified_at_utc="2026-10-11T01:00:00Z", observed_at_utc="2026-10-12T01:00:00Z",
        content_sha256=digest, excerpt_sha256=digest, excerpt=text, parser_backend="light_text_v2", worker_lane="rust_core",
        parse_status="success", parse_truncated=False, excerpt_truncated=False, coverage_limits={}, reason=None)
    data.update(changes)
    return ScanSource.model_validate(data)


def scan_result(*sources, status="ok", warnings=()):
    return ScanResult(envelope=SimpleNamespace(status=status, warnings=warnings,
        error=SimpleNamespace(error_code="DISCOVERY_FAILED") if status == "error" else None), evidence=None, sources=sources)


class Scanner:
    def __init__(self, *results):
        self.results = list(results)
        self.requests = []

    def build_context(self, request):
        self.requests.append(request)
        return self.results.pop(0) if len(self.results) > 1 else self.results[0]


class Transport:
    def __init__(self, interrupt_at=0):
        self.requests = []
        self.interrupt_at = interrupt_at

    def send(self, request, *, timeout):
        self.requests.append(request)
        if len(self.requests) == self.interrupt_at:
            raise KeyboardInterrupt
        payload = json.loads(request["messages"][1]["content"])
        ids = sorted({source["source_id"] for source in payload["sources"]})
        content = json.dumps({"summary_items": [{"text": "材料记载了工作安排。", "source_ids": ids}],
                              "verification_items": [{"text": "执行结果尚未确认。", "source_ids": ids}]}, ensure_ascii=False)
        return {"choices": [{"finish_reason": "stop", "message": {"content": content}}], "usage": None}


def context(now=NOW):
    return RunContext.create(now=now)


def service(config, scanner, transport=None, clock=lambda: NOW):
    return EvidenceDailyService(config, scanner=scanner, transport_factory=lambda: transport or Transport(), clock=clock)


def test_parallel_model_budget_is_atomic_and_resume_reuses_completed_calls(config):
    config.recovery_settings = RecoverySettings(max_calls_per_run=2, max_calls_per_calendar_day=2)
    scanner = Scanner(scan_result(source(text="有来源的材料。" * 3000)))
    transport = Transport()
    app = EvidenceDailyService(config, scanner=scanner, transport_factory=lambda: transport,
                               clock=lambda: NOW, model_workers=3)
    with pytest.raises(ReportRunError, match="MODEL_(RUN|DAY)_BUDGET_EXHAUSTED"):
        app.run(context())
    run = app.runs.list_runs()[0]
    assert len(transport.requests) == 2
    assert [call['status'] for call in app.runs.calls(run['run_id'])] == ['succeeded', 'succeeded']
    config.recovery_settings = RecoverySettings()
    resumed = app.run(context())
    assert resumed.quality == 'complete'
    assert 2 < resumed.model_calls <= 8
    assert len(transport.requests) == resumed.model_calls
    assert app.runs.pending() == []


@pytest.mark.parametrize('native_hash_available', [True, False])
def test_local_ocr_retains_source_hash_and_exposes_recognition_uncertainty(tmp_path, native_hash_available):
    from scripts.backfill_daily_reports import PdfOcrScanner
    from src.services.report_runner.scan_evidence import coverage_gaps
    text_path = tmp_path / 'ocr.txt'
    text_path.write_text('扫描件说明', encoding='utf-8')
    pdf_path = tmp_path / 'work.pdf'
    from reportlab.pdfgen import canvas
    pdf = canvas.Canvas(str(pdf_path), invariant=1)
    for _ in range(8):
        pdf.showPage()
    pdf.save()
    digest = sha256(pdf_path.read_bytes()).hexdigest()
    original = source(text='', relative_path='work.pdf', extension='.pdf',
                      source_id='src_' + sha256(b'work.pdf').hexdigest(), size_bytes=pdf_path.stat().st_size,
                      content_sha256=digest if native_hash_available else None)
    manifest = tmp_path / 'documents.json'
    manifest.write_text(json.dumps([{'source_id': original.source_id, 'content_sha256': digest,
        'pages_total': 8, 'pages': [{'page': 1, 'text': str(text_path), 'text_sha256': sha256(text_path.read_bytes()).hexdigest()}]}]), encoding='utf-8')
    scanner = PdfOcrScanner(Scanner(scan_result(original)), manifest, work_root=tmp_path)
    result = scanner.build_context(None)
    parsed = ScanSource.model_validate(result.sources[0].model_dump())
    assert parsed.content_sha256 == digest
    assert parsed.parser_backend == 'local_ocr_import_v2' and parsed.worker_lane == 'local_ocr_preflight'
    assert parsed.parse_truncated and '扫描件说明' in parsed.excerpt
    assert 'OCR_TEXT_UNVERIFIED' in {gap.code for gap in coverage_gaps(result)}
    if native_hash_available:
        scanner.inner = Scanner(scan_result(original.model_copy(update={'content_sha256': 'b' * 64})))
    else:
        pdf_path.write_bytes(b'scanned PDF modified bytes')
    with pytest.raises(ReportRunError, match='BACKFILL_OCR_SOURCE_CHANGED'):
        scanner.build_context(None)


def test_native_empty_directory_publishes_no_changes_without_credentials(config):
    def no_model():
        raise AssertionError("empty scan must not construct the model")
    app = EvidenceDailyService(config, transport_factory=no_model, clock=lambda: NOW)
    result = app.run(context(), scheduled=True)
    assert result.quality == "no_changes" and result.model_calls == 0
    assert result.output_path.exists() and "未发现符合扫描规则的修改文件" in result.markdown


def test_unverified_ocr_claims_are_forced_into_verification(config):
    scanned = source(coverage_limits={"ocr": {"text_unverified": True}})
    app = service(config, Scanner(scan_result(scanned)), Transport())
    result = app.run(context())
    report = app.runs.store.get_report(result.target_date)
    assert not report.summary_items
    assert any(item.text == "材料记载了工作安排。" for item in report.verification_items)


def test_native_full_pipeline_keeps_candidate_on_cache_reuse(config):
    path = config.work_dir / "资料.md"
    path.write_text("文件记载：新方案计划下周实施。", encoding="utf-8")
    stamp = datetime(2026, 10, 11, 1, tzinfo=UTC).timestamp()
    os.utime(path, (stamp, stamp))
    transport = Transport()
    app = service(config, None, transport)
    first = app.run(context())
    old_bytes = first.output_path.read_bytes()
    second = app.run(context(NOW + timedelta(minutes=1)))
    assert second.action == "reused" and len(transport.requests) == 1
    assert second.output_path.read_bytes() == old_bytes
    report = app.runs.store.get_report(first.target_date)
    assert report.sources[0].content_sha256 == sha256(path.read_bytes()).hexdigest()


def test_local_notes_republish_without_sending_them_to_model(config):
    transport = Transport()
    app = service(config, Scanner(scan_result(source())), transport)
    first = app.run(context())
    original = app.runs.store.get_report(first.target_date)
    notes = config.project_root / config.report_settings.notes_dir / f"{first.target_date}.md"
    notes.parent.mkdir(parents=True)
    notes.write_text("私有附注：等待老板确认。", encoding="utf-8")
    second = app.run(context(NOW + timedelta(minutes=1)))
    assert second.action == "published" and len(transport.requests) == 1
    assert "私有附注" in second.markdown and "私有附注" not in json.dumps(transport.requests, ensure_ascii=False)
    assert app.runs.store.get_report(first.target_date).generated_at == original.generated_at


@pytest.mark.parametrize("retained_sources", [False, True])
def test_unavailable_scan_keeps_diagnostic_and_can_rescan_with_new_attempt(config, retained_sources):
    scanner = Scanner(scan_result(*([source()] if retained_sources else []), status="error"), scan_result(source()))
    transport = Transport()
    app = service(config, scanner, transport)
    with pytest.raises(ReportRunError, match="SCAN_UNAVAILABLE"):
        app.run(context())
    assert transport.requests == [] and app.runs.pending() == []
    assert app.runs.list_runs()[0]["quality"] == "unavailable"
    assert app.run(context()).quality == "complete"


def test_one_unstable_retry_keeps_disappeared_source_as_gap(config):
    empty = sha256(b"").hexdigest()
    unstable = source("", parse_status="error", content_sha256=None, excerpt_sha256=empty,
                      reason="source_unstable_or_fingerprint_unavailable")
    scanner = Scanner(scan_result(unstable), scan_result())
    app = service(config, scanner)
    outcome = app.run(context(), user_input="本人补充：正在核实该文件。")
    assert len(scanner.requests) == 2 and outcome.quality == "partial"
    report = app.runs.store.get_report(outcome.target_date)
    assert len(report.sources) == 2 and any(gap.code == "SOURCE_UNAVAILABLE" for gap in report.gaps)


def test_resume_uses_frozen_input_and_reuses_prior_batches(config):
    config.model_request_settings = config.model_request_settings.model_copy(update={"max_request_utf8_bytes": 7000})
    scanner = Scanner(scan_result(source("读取到的表格行\n" * 2000)))
    transport = Transport(interrupt_at=2)
    app = service(config, scanner, transport)
    with pytest.raises(KeyboardInterrupt):
        app.run(context())
    pending = app.runs.pending()
    assert len(pending) == 1 and pending[0]["phase"] == "generating"
    run_id = pending[0]["run_id"]
    assert app.runs.calls(run_id)[0]["status"] == "succeeded"
    # The retry starts on the next wall-clock day, while the requested business date stays frozen.
    app.clock = lambda: NOW + timedelta(days=1)
    result = app.run(context())
    assert len(scanner.requests) == 1 and result.target_date == "2026-10-11"
    assert result.run_id == run_id and result.output_path.exists()
    first_message = transport.requests[0]
    assert sum(request == first_message for request in transport.requests) == 1


def test_missing_snapshot_blocks_resume_but_staged_output_repairs_without_snapshot(config):
    app = service(config, Scanner(scan_result(source())))
    result = app.run(context())
    for path in (config.data_dir / "run_snapshots").rglob("*.json"):
        path.unlink()
    result.output_path.unlink()
    app.scanner = None
    repaired = app.repair(result.target_date)
    assert repaired.output_path.exists() and repaired.model_calls == 0


def test_no_save_audits_model_but_does_not_publish(config):
    app = service(config, Scanner(scan_result(source())))
    result = app.run(context(), save=False)
    assert result.output_path is None and result.action == "preview" and result.model_calls == 1
    assert app.runs.store.get_report(result.target_date) is None
    assert not list(config.reports_dir.rglob("*.md"))


def test_regenerate_keeps_published_version_until_new_result_is_ready(config):
    transport = Transport()
    app = service(config, Scanner(scan_result(source())), transport)
    first = app.run(context())
    second = app.run(context(), regenerate_reason="复核表达")
    assert len(transport.requests) == 2 and first.run_id != second.run_id
    assert app.runs.get(second.run_id)["regeneration_reason"] == "复核表达"


def test_user_input_can_fill_empty_scan_but_file_failure_stays_visible(config):
    app = service(config, Scanner(scan_result(status="error")))
    result = app.run(context(), user_input="已核对一条采购记录。")
    assert result.quality == "partial"
    assert app.runs.store.get_report(result.target_date).sources[0].source_kind == "user_statement"


def test_verified_regeneration_retires_old_pending_run_and_preserves_call_budget(config):
    transport = Transport(interrupt_at=1)
    app = service(config, Scanner(scan_result(source())), transport)
    with pytest.raises(KeyboardInterrupt):
        app.run(context())
    old = app.runs.pending()[0]
    config.llm_config["max_tokens"] = 3072
    newer = app.run(context(), regenerate_reason="Increase verified output budget")
    retired = app.runs.get(old["run_id"])
    assert retired["status"] == "cancelled" and retired["error_code"] == "RUN_SUPERSEDED"
    assert len(app.runs.calls(old["run_id"])) == 1
    assert app.runs.pending() == []
    with pytest.raises(ReportRunError, match="RUN_NOT_RECOVERABLE"):
        app.runs.resume(old["run_id"])
    reused = app.run(context(), scheduled=True)
    assert reused.action == "reused" and reused.output_path == newer.output_path
    assert len(transport.requests) == 2

    from datetime import timedelta
    from src.services.report_runner.retention import cleanup_snapshots

    cleanup_snapshots(app.runs, app.snapshots, config.retention_settings, now=NOW + timedelta(days=31))
    assert old["run_id"] not in {run["run_id"] for run in app.runs.list_runs()}
    assert app.runs.current(newer.target_date)["run_id"] == newer.run_id
    assert newer.output_path.exists()


def test_failed_regeneration_keeps_old_run_recoverable_until_publication(config):
    transport = Transport(interrupt_at=1)
    app = service(config, Scanner(scan_result(source())), transport)
    with pytest.raises(KeyboardInterrupt):
        app.run(context())
    old = app.runs.pending()[0]
    path = config.reports_dir / "2026-10/2026-10-11.md"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("User-owned report", encoding="utf-8")
    with pytest.raises(ReportRunError, match="OUTPUT_CONFLICT"):
        app.run(context(), regenerate_reason="Retry interrupted generation")
    assert app.runs.get(old["run_id"])["error_code"] != "RUN_SUPERSEDED"
    assert len(app.runs.pending()) == 2
    assert path.read_text(encoding="utf-8") == "User-owned report"


@pytest.mark.parametrize("workers", [1, 3])
def test_all_successful_batches_restore_without_constructing_gateway_client(config, monkeypatch, workers):
    config.model_request_settings = config.model_request_settings.model_copy(update={"max_sources_per_batch": 1})
    scanner = Scanner(scan_result(source(), source("第二份材料记载金额。",
        source_id="src_" + sha256(b"second.md").hexdigest(), relative_path="second.md")))
    transport = Transport()
    app = EvidenceDailyService(config, scanner=scanner, transport_factory=lambda: transport,
                               clock=lambda: NOW, model_workers=workers)
    publish = app._stage_publish
    monkeypatch.setattr(app, "_stage_publish", lambda *_args: (_ for _ in ()).throw(KeyboardInterrupt()))
    with pytest.raises(KeyboardInterrupt):
        app.run(context())
    run = app.runs.pending()[0]
    assert len(app.runs.calls(run["run_id"])) == len(transport.requests) == 2
    monkeypatch.setattr(app, "_stage_publish", publish)
    monkeypatch.setattr(app, "_transport", lambda: (_ for _ in ()).throw(AssertionError("credentials must not be needed")))
    monkeypatch.setattr(type(app.runs), "reserve_call", lambda *_args, **_kwargs: (_ for _ in ()).throw(
        AssertionError("offline recovery must not reserve calls")))
    config.recovery_settings = RecoverySettings(max_calls_per_run=1, max_calls_per_calendar_day=1)
    config.model_request_settings = config.model_request_settings.model_copy(update={
        "context_window_tokens": None, "verified_max_output_tokens": None})
    result = app.run(context())
    assert result.run_id == run["run_id"] and result.output_path.exists()
    assert len(scanner.requests) == 1 and len(transport.requests) == 2


def test_cached_recovery_rejects_changed_model_semantics(config, monkeypatch):
    app = service(config, Scanner(scan_result(source())))
    monkeypatch.setattr(app, "_stage_publish", lambda *_args: (_ for _ in ()).throw(KeyboardInterrupt()))
    with pytest.raises(KeyboardInterrupt):
        app.run(context())
    config.llm_config["model_id"] = "another-model"
    with pytest.raises(ReportRunError, match="RECOVERY_CONFIG_CHANGED"):
        app.run(context())


def test_reduced_batch_publishes_actual_character_counts_and_preserves_planned_counts(config):
    class TruncatedTransport(Transport):
        def send(self, request, *, timeout):
            result = super().send(request, timeout=timeout)
            if len(self.requests) == 1:
                result["choices"][0]["finish_reason"] = "length"
            return result

    transport = TruncatedTransport()
    app = service(config, Scanner(scan_result(source("材料记载金额100元。\n" * 400))), transport)
    outcome = app.run(context())
    report = app.runs.store.get_report(outcome.target_date)
    actual = report.sources[0]
    assert actual.model_chars < actual.coverage_limits["evidence_audit"]["planned_model_chars"]
    assert actual.coverage_limits["evidence_audit"]["effective_plan_sha256"]
    assert actual.model_truncated and report.quality == "partial"
    assert report.coverage_summary.model_covered_count == 1
    audit = actual.coverage_limits["evidence_audit"]
    from src.services.report_runner.model_slicing import union_chars
    assert audit["model_ranges"] and union_chars(audit["model_ranges"]) == actual.model_chars
    assert audit["attempted_model_chars"] >= actual.model_chars


def test_synthetic_model_check_is_audited_and_never_contains_working_file_text(config):
    from src.services.report_runner.model_check import check_model

    class CheckTransport:
        requests = []

        def send(self, request, *, timeout):
            self.requests.append(request)
            content = json.dumps({"summary_items": [{"text": "合成检查通过：SYNTH-731，2 条记录。",
                "source_ids": ["user_input"]}], "verification_items": []}, ensure_ascii=False)
            return {"choices": [{"finish_reason": "stop", "message": {"content": content}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}}

    config.model_request_settings = ModelRequestSettings()
    (config.work_dir / "private.md").write_text("PRIVATE_FILE_BODY", encoding="utf-8")
    transport = CheckTransport()
    result = check_model(config, transport_factory=lambda: transport, clock=lambda: NOW)
    assert result["json_verified"] and result["calls"] == 1
    assert result["context_limits_verified"] is False and result["usage_available"] is True
    assert "PRIVATE_FILE_BODY" not in json.dumps(transport.requests)


def test_weekly_model_call_uses_shared_budget_and_retains_legacy_schema(config):
    from src.models.schemas import DailyReportData
    from src.services.report_runner.period_model import generate_period_model
    from src.services.report_run_store import ReportRunStore
    from src.services.sqlite_store import SQLiteStore

    class PeriodTransport:
        def send(self, request, *, timeout):
            return {"choices": [{"finish_reason": "stop", "message": {"content": json.dumps({
                "date": "2026-10-11", "completed_work": "用户确认的事实", "work_summary": "小结", "next_plan": "计划"})}}]}

    result = generate_period_model(config, "synthetic period prompt", DailyReportData, transport_factory=PeriodTransport)
    assert result.date == "2026-10-11"
    runs = ReportRunStore(SQLiteStore(config.db_dir / "reports.sqlite3"))
    audit = runs.list_runs()[0]
    assert audit["status"] == "succeeded" and len(runs.calls(audit["run_id"])) == 1


def test_company_gateway_requires_its_explicit_environment_credential(config, monkeypatch):
    monkeypatch.delenv("DEEPSEEK_API_KEY", raising=False)
    config.llm_config["base_url"] = "https://ai-gateway.fsdev.cn"
    config.deepseek_api_key = "unrelated-existing-provider-test-value"
    app = EvidenceDailyService(config, scanner=Scanner(scan_result(source())), clock=lambda: NOW)
    with pytest.raises(ReportRunError, match="CREDENTIAL_MISSING"):
        app.run(context())
    assert app.runs.calls(app.runs.list_runs()[0]["run_id"]) == []
