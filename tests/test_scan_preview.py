from datetime import UTC, date, datetime
from hashlib import sha256
import json
import os
from pathlib import Path
from types import SimpleNamespace

import pytest

from src.cli.daily import generate_daily_report
from src.cli.parser import build_parser
from src.models.scan_sources import ScanSource
from src.services.native_scanner import NativeScanner, ScanRequest
from src.services.report_runner.context import RunContext
from src.services.scan_preview import run_preview, scan_quality


def source(**overrides):
    values = dict(
        source_id="src_" + sha256(b"report.md").hexdigest(), relative_path="report.md",
        extension=".md", size_bytes=7, modified_at_utc="2026-10-11T01:00:00Z",
        observed_at_utc="2026-10-12T01:00:00Z", content_sha256=sha256(b"content").hexdigest(),
        excerpt_sha256=sha256(b"content").hexdigest(), excerpt="content", parser_backend="light_text_v2",
        worker_lane="rust_core", parse_status="success", parse_truncated=False, excerpt_truncated=False,
        coverage_limits={}, reason=None,
    )
    values.update(overrides)
    return ScanSource.model_validate(values)


def test_source_requires_traceable_identity_and_matching_excerpt():
    source()
    for change in ({"excerpt": "changed"}, {"source_id": "src_" + "0" * 64},
                   {"content_sha256": None}, {"parse_status": "error"},
                   {"modified_at_utc": "2026-10-11T01:00:00"}):
        with pytest.raises(ValueError):
            source(**change)


@pytest.mark.parametrize("status,sources,warnings,quality", [
    ("ok", [], [], "no_changes"),
    ("error", [], [], "unavailable"),
    ("partial", [], ["cloud"], "unavailable"),
    ("ok", [source()], [], "complete"),
    ("ok", [source(parse_truncated=True)], [], "partial"),
    ("ok", [source(extension=".csv")], [], "partial"),
    ("ok", [], ["WORKER_RSS_UNAVAILABLE"], "no_changes"),
])
def test_quality_preserves_missing_evidence(status, sources, warnings, quality):
    result = SimpleNamespace(envelope=SimpleNamespace(status=status,
        warnings=[SimpleNamespace(error_code=code) for code in warnings], error=None), sources=sources)
    assert scan_quality(result) == quality


def test_preview_retries_a_changed_source_and_keeps_disappeared_evidence(tmp_path):
    from src.services.native_scanner import ScanResult

    context = RunContext.create(now=datetime(2026, 10, 12, 2, tzinfo=UTC))
    cfg = SimpleNamespace(data_dir=tmp_path / "data", work_dir=tmp_path)
    envelope = SimpleNamespace(status="ok", warnings=[], error=None,
        summary=SimpleNamespace(model_dump=lambda **_: {"source_file_count": 0}))
    changed = source(parse_status="error", content_sha256=None, excerpt="",
        excerpt_sha256=sha256(b"").hexdigest(), reason="source_version_changed")
    results = iter([ScanResult(envelope, None, (changed,)), ScanResult(envelope, None, ())])
    requests = []

    def scan(request):
        requests.append(request)
        return next(results)

    preview = run_preview(context, cfg, scanner=SimpleNamespace(build_context=scan))
    manifest = json.loads(preview.manifest_path.read_text(encoding="utf-8"))
    assert len(requests) == 2 and preview.quality == "unavailable"
    assert preview.source_count == 1 and manifest["model_calls"] == 0
    assert manifest["coverage_gaps"][0]["code"] == "SOURCE_UNAVAILABLE"
    assert manifest["sources"][0]["reason"] == "source_unstable_disappeared_on_retry"


def test_dry_run_does_not_construct_the_report_runner(monkeypatch, tmp_path):
    import src.services.scan_preview as preview

    def forbidden():
        raise AssertionError("model/report runner must not be constructed")

    monkeypatch.setattr(preview, "run_preview", lambda *_: preview.ScanPreview("complete", 1, 1, tmp_path / "preview.json"))
    console = SimpleNamespace(print=lambda *args, **kwargs: None)
    args = build_parser().parse_args(["daily", "--target-date", "2026-01-01", "--dry-run"])
    assert generate_daily_report(args, console=console, runner_factory=forbidden)


def test_native_sources_survive_cache_hit_and_capture_preserved_mtime_change(tmp_path):
    root = Path(__file__).parents[1]
    work = tmp_path / "中文 工作"
    work.mkdir()
    path = work / "report.md"
    original = "核对采购申请清单 A"
    replacement = "核对采购申请清单 B"
    path.write_text(original, encoding="utf-8")
    stamp = datetime(2026, 10, 11, 1, tzinfo=UTC).timestamp()
    os.utime(path, (stamp, stamp))
    cfg = SimpleNamespace(
        work_dir=work, data_dir=tmp_path / "data", reports_dir=tmp_path / "reports",
        db_dir=tmp_path / "data/db", log_dir=tmp_path / "logs",
        index_db_path=tmp_path / "data/db/scan_index_v3.sqlite3",
        office_worker_path=root / "rust/target/release/ai-daily-office-parser.exe",
        scanner_settings=lambda: {"allowed_extensions": [".md"], "total_deadline_ms": 30000, "total_max_chars": 50000},
    )
    cfg.db_dir.mkdir(parents=True)
    scanner = NativeScanner(cfg)
    context = RunContext.create(now=datetime(2026, 10, 12, 2, tzinfo=UTC), target_date=date(2026, 10, 11))
    request = ScanRequest("daily", context.target_date, context.target_date,
                          window_start_utc=context.window_start_utc, window_end_utc=context.window_end_utc)
    first = scanner.build_context(request)
    second = scanner.build_context(request)
    assert first.envelope.status in {"ok", "partial"}
    assert len(first.sources) == len(second.sources) == 1
    assert first.sources[0].excerpt == second.sources[0].excerpt
    assert first.sources[0].content_sha256 == sha256(original.encode()).hexdigest()
    assert second.evidence.reuse_kind in {"context_snapshot", "parse_cache"}
    path.write_text(replacement, encoding="utf-8")
    os.utime(path, (stamp, stamp))
    third = scanner.build_context(request)
    assert third.sources[0].source_id == first.sources[0].source_id
    assert third.sources[0].content_sha256 == sha256(replacement.encode()).hexdigest()
    assert replacement in third.sources[0].excerpt
    preview = run_preview(context, cfg, scanner=scanner)
    manifest = json.loads(preview.manifest_path.read_text(encoding="utf-8"))
    assert manifest["model_calls"] == 0
    assert "excerpt" not in manifest["sources"][0]
    assert not (cfg.reports_dir).exists()
    assert not (cfg.db_dir / "reports.sqlite3").exists()
