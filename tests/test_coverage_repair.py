from hashlib import sha256
import json
from pathlib import Path

import pytest

from test_evidence_daily_service import (
    config as config_fixture, source, Scanner, scan_result, service, context, Transport,
)
from src.services.coverage_repair import make_plan, generate_candidates, publish_candidates
from src.services.coverage_repair_events import RepairEvents
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.locking import ReportLock


@pytest.mark.parametrize("prior_kind,changed", [("unknown", None), ("identical", False), ("changed", True)])
def test_file_pin_distinguishes_unknown_identical_and_changed_versions(tmp_path, prior_kind, changed):
    from src.services.coverage_repair import FilePin, _file_pin

    content = b"frozen current source"
    (tmp_path / "source.md").write_bytes(content)
    current = sha256(content).hexdigest()
    prior = None if prior_kind == "unknown" else current if prior_kind == "identical" else "1" * 64
    pin = FilePin(relative_path="source.md", size_bytes=len(content), mtime_ns=None,
        prior_content_sha256=prior, content_sha256=None)
    refreshed = _file_pin(tmp_path, pin, 1000)
    assert refreshed.content_sha256 == current
    assert refreshed.current_version_changed is changed


@pytest.mark.parametrize("prior,changed,expected", [
    (None, True, "HISTORICAL_CONTENT_UNKNOWN"),
    ("1" * 64, True, "HISTORICAL_CONTENT_CHANGED"),
    ("1" * 64, False, None),
])
def test_history_gaps_require_evidence_of_a_changed_version(prior, changed, expected):
    from src.services.report_runner.scan_evidence import coverage_gaps

    result = scan_result(source(coverage_limits={"history": {
        "prior_content_sha256": prior, "current_version_changed": changed}}))
    codes = [gap.code for gap in coverage_gaps(result) if gap.code.startswith("HISTORICAL_CONTENT_")]
    assert codes == ([expected] if expected else [])


@pytest.fixture
def scenario(config_fixture):
    config = config_fixture
    path = config.work_dir / "nested/work.md"
    path.parent.mkdir()
    text = "方案计划于2026-09-22验收。局部小计1234.56元，执行结果待确认。"
    path.write_text(text, encoding="utf-8")
    original = source(text=text[:8], relative_path="nested/work.md",
        source_id="src_" + sha256(b"nested/work.md").hexdigest(), size_bytes=path.stat().st_size,
        content_sha256=sha256(path.read_bytes()).hexdigest(), parse_truncated=True)
    app = service(config, Scanner(scan_result(original)), Transport())
    outcome = app.run(context())
    return config, path, app, outcome


def plan_for(scenario, *, refresh=True, calls=4):
    config, _, _, outcome = scenario
    return make_plan(config, dates=[outcome.target_date], gap_codes=["PARSE_BOUNDED"],
        approved_root=config.work_dir, calls_limit=calls, tokens_limit=200000, refresh=refresh)


def candidate_for(scenario):
    plan = plan_for(scenario)
    transport = Transport()
    manifest = generate_candidates(scenario[0], plan, transport_factory=lambda: transport)
    return plan, manifest, transport


def test_native_preflight_keeps_nested_staged_inputs_under_application_data(scenario):
    from datetime import date
    from src.services.coverage_repair import load_plan, _scanner
    from src.services.native_scanner import ScanRequest

    config, _, app, outcome = scenario
    before = app.runs.calls(outcome.run_id)
    path = plan_for(scenario)
    plan, directory, _ = load_plan(config, path)
    item = plan.dates[0]
    target = date.fromisoformat(item.date)
    result = _scanner(config, plan, item, directory).build_context(ScanRequest("daily", target, target))
    assert [source.relative_path for source in result.sources] == ["nested/work.md"]
    assert "1234.56" in result.sources[0].excerpt
    assert (directory / "sources" / item.date / "nested/work.md").is_relative_to(config.data_dir)
    assert app.runs.calls(outcome.run_id) == before


def test_metadata_dry_run_never_reads_business_files_or_calls_models(scenario, monkeypatch):
    import src.services.coverage_repair as repair
    monkeypatch.setattr(repair, "_file_pin", lambda *args: pytest.fail("metadata preflight must not read files"))
    _, _, app, outcome = scenario
    before = outcome.output_path.read_bytes()
    count = len(app.runs.calls(outcome.run_id))
    path = plan_for(scenario, refresh=False)
    value = json.loads(path.read_text(encoding="utf-8"))["plan"]
    assert not value["refreshed"] and value["dates"][0]["files"][0]["content_sha256"] is None
    with pytest.raises(ReportRunError, match="COVERAGE_PREFLIGHT_REQUIRED"):
        generate_candidates(scenario[0], path, transport_factory=lambda: pytest.fail("no model"))
    assert outcome.output_path.read_bytes() == before and len(app.runs.calls(outcome.run_id)) == count


def test_candidate_preview_preserves_current_and_repeated_generation_reuses_the_saved_run(scenario):
    config, _, app, outcome = scenario
    before = outcome.output_path.read_bytes()
    plan, manifest, transport = candidate_for(scenario)
    envelope = json.loads(manifest.read_text(encoding="utf-8"))
    entry = envelope["manifest"]["candidates"][0]
    preview = app.runs.get(entry["run_id"])
    assert preview["run_kind"] == "preview" and preview["status"] == "succeeded"
    assert preview["report_json"] and preview["markdown"]
    assert app.runs.current(outcome.target_date)["run_id"] == outcome.run_id
    assert outcome.output_path.read_bytes() == before and len(transport.requests) == 1
    calls = app.runs.calls(entry["run_id"])
    assert json.loads(calls[0]["usage_json"])["reserved_budget"]["job_id"] == envelope["manifest"]["job_id"]
    events = [json.loads(line) for line in (plan.parent / "events.jsonl").read_text(encoding="utf-8").splitlines()]
    registered = next(event for event in events if event["body"]["action"] == "run_registered")
    assert registered["at"] <= calls[0]["started_at"]
    event_count = len(events)
    replay = generate_candidates(config, plan, scanner_factory=lambda _: pytest.fail("saved preview must not scan"),
        transport_factory=lambda: pytest.fail("saved preview must not send"))
    assert replay == manifest and app.runs.calls(entry["run_id"]) == calls
    assert len((plan.parent / "events.jsonl").read_text(encoding="utf-8").splitlines()) == event_count


def test_candidate_publication_is_zero_call_bound_and_idempotent(scenario, monkeypatch):
    config, _, app, outcome = scenario
    plan, manifest, transport = candidate_for(scenario)
    from src.services.native_scanner import NativeScanner
    from src.services.report_runner.evidence_daily import EvidenceDailyService
    monkeypatch.setattr(NativeScanner, "build_context", lambda *args: pytest.fail("publication must not scan"))
    monkeypatch.setattr(EvidenceDailyService, "_transport", lambda *args: pytest.fail("publication must not generate"))
    assert publish_candidates(config, manifest) == [outcome.target_date]
    current = app.runs.current(outcome.target_date)
    assert current["run_id"] != outcome.run_id and current["regeneration_reason"].startswith("coverage_publish:")
    assert app.runs.calls(current["run_id"]) == [] and outcome.output_path.read_text(encoding="utf-8") == current["markdown"]
    count = len((plan.parent / "events.jsonl").read_text(encoding="utf-8").splitlines())
    assert publish_candidates(config, manifest) == [outcome.target_date]
    assert len((plan.parent / "events.jsonl").read_text(encoding="utf-8").splitlines()) == count
    assert len(transport.requests) == 1


@pytest.mark.parametrize("edit_candidate", [False, True])
def test_publication_preserves_conflicting_current_or_candidate_edits(scenario, edit_candidate):
    config, _, app, outcome = scenario
    plan, manifest, _ = candidate_for(scenario)
    target = plan.parent / (outcome.target_date + ".candidate.md") if edit_candidate else outcome.output_path
    target.write_text("hand edited content", encoding="utf-8")
    with pytest.raises(ReportRunError, match="COVERAGE_(CANDIDATE|CURRENT)_CHANGED"):
        publish_candidates(config, manifest)
    assert app.runs.current(outcome.target_date)["run_id"] == outcome.run_id
    assert target.read_text(encoding="utf-8") == "hand edited content"


def test_changed_source_blocks_generation_before_a_new_call(scenario):
    config, source_path, app, outcome = scenario
    plan = plan_for(scenario)
    source_path.write_text("changed larger file" * 20, encoding="utf-8")
    with pytest.raises(ReportRunError, match="COVERAGE_SOURCE_(SIZE|CHANGED)"):
        generate_candidates(config, plan, transport_factory=lambda: pytest.fail("changed source must not send"))
    assert app.runs.current(outcome.target_date)["run_id"] == outcome.run_id


def test_crash_after_preview_success_recovers_without_model_or_scanner(scenario, monkeypatch):
    import src.services.coverage_repair as repair
    config, _, app, outcome = scenario
    plan = plan_for(scenario)
    transport = Transport()
    writer = repair._write_artifact
    monkeypatch.setattr(repair, "_write_artifact", lambda *args: (_ for _ in ()).throw(RuntimeError("synthetic crash")))
    with pytest.raises(RuntimeError, match="synthetic crash"):
        generate_candidates(config, plan, transport_factory=lambda: transport)
    assert len(transport.requests) == 1 and app.runs.current(outcome.target_date)["run_id"] == outcome.run_id
    monkeypatch.setattr(repair, "_write_artifact", writer)
    manifest = generate_candidates(config, plan, scanner_factory=lambda _: pytest.fail("resume must not scan"),
        transport_factory=lambda: pytest.fail("resume must not send"))
    assert manifest.exists() and len(transport.requests) == 1


def test_publication_crash_after_staging_recovers_only_through_explicit_publish(scenario, monkeypatch):
    config, _, app, outcome = scenario
    plan, manifest, transport = candidate_for(scenario)
    from src.services.report_runner.publication import ReportPublisher
    publisher = ReportPublisher.publish
    monkeypatch.setattr(ReportPublisher, "publish", lambda *args: (_ for _ in ()).throw(RuntimeError("staged crash")))
    with pytest.raises(RuntimeError, match="staged crash"):
        publish_candidates(config, manifest)
    state = json.loads((plan.parent / "state.json").read_text(encoding="utf-8"))
    publish_id = state["dates"][outcome.target_date]["publish_run_id"]
    event = json.loads((plan.parent / "events.jsonl").read_text(encoding="utf-8").splitlines()[-1])["body"]
    assert event["action"] == "publication_failed"
    assert event["details"]["publish_run_id"] == publish_id
    assert event["details"]["calls_delta"] == 0
    assert app.runs.get(publish_id)["phase"] == "db_committed"
    assert publish_id not in {run["run_id"] for run in app.runs.pending()}
    assert app.runs.current(outcome.target_date)["run_id"] == outcome.run_id
    monkeypatch.setattr(ReportPublisher, "publish", publisher)
    publish_candidates(config, manifest)
    assert app.runs.current(outcome.target_date)["run_id"] == publish_id
    assert app.runs.calls(publish_id) == [] and len(transport.requests) == 1


def test_repair_cli_requires_explicit_scope_and_rejects_legacy_flag_mix():
    import subprocess, sys
    script = Path(__file__).parents[1] / "scripts/backfill_daily_reports.py"
    for flags in (("--repair-coverage", "--dry-run"), ("--repair-coverage", "--only-failed")):
        result = subprocess.run([sys.executable, str(script), *flags], capture_output=True, text=True)
        assert result.returncode == 2
        assert "Traceback" not in result.stderr


def test_event_replay_recovers_partial_tail_and_stale_view_without_erasing_history(tmp_path):
    directory = tmp_path / "job"
    job_id, plan_hash = "coverage_" + "a" * 32, "b" * 64
    with ReportLock(directory / "job", scope_id=job_id):
        journal = RepairEvents(directory, job_id, plan_hash)
        first = journal.commit("run_registered", target_date="2026-09-22", run_id="c" * 32,
            updates={"run_id": "c" * 32, "status": "generating"})
        assert journal.commit("run_registered", target_date="2026-09-22", run_id="c" * 32,
            updates={"run_id": "c" * 32, "status": "generating"}) == first
        prefix = (directory / "events.jsonl").read_bytes()
        with (directory / "events.jsonl").open("ab") as stream:
            stream.write(b'{"interrupted":')
        (directory / "state.json").write_text("{", encoding="utf-8")
        replay = RepairEvents(directory, job_id, plan_hash)
        assert replay.state["dates"]["2026-09-22"]["run_id"] == "c" * 32
        assert (directory / "events.jsonl").read_bytes().startswith(prefix)
        assert next(directory.glob("events.tail.*.bin")).read_bytes() == b'{"interrupted":'
        assert replay.events[-1]["body"]["action"] == "tail_recovered"
        assert RepairEvents(directory, job_id, plan_hash).events == replay.events
