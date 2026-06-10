from datetime import date, datetime
from pathlib import Path

from src.models.schemas import FileContext
from src.services.cold_scanner_run import ColdScannerRun
from src.services.scan_discovery import DiscoveredFile
from src.services.scan_index_store import CacheProbe, InventoryItem


class FakeDiscoveryService:
    def __init__(self, discovered_files):
        self.discovered_files = discovered_files
        self.calls = []

    def bootstrap_full_scan(self, start_date, end_date):
        self.calls.append((start_date, end_date))
        return self.discovered_files


class FakeScanIndexStore:
    def __init__(self, inventory_items=None, cache_probes=None, run_id=77):
        self.inventory_items = inventory_items or []
        self.cache_probes = cache_probes or {}
        self.run_id = run_id
        self.replaced_inventory = []
        self.saved_metrics = []

    def replace_inventory(self, rows):
        self.replaced_inventory.append(rows)

    def query_inventory(self, start_date, end_date):
        return self.inventory_items

    def probe_parse_cache(self, file_identity, parser_profile, source_version=""):
        return self.cache_probes[file_identity]

    def save_scan_run_metrics(self, run_metrics):
        self.saved_metrics.append(run_metrics)
        return self.run_id


class FakeScanPlanner:
    def __init__(self, planned_candidates):
        self.planned_candidates = planned_candidates
        self.summary_modes = []
        self.plan_calls = []

    def build_parser_profile(self, summary_mode):
        self.summary_modes.append(summary_mode)
        return {
            "total_max_chars": 1000,
            "summary_mode": summary_mode,
            "text_max_chars": 120,
        }

    def serialize_parser_profile(self, parser_profile):
        return "profile-key"

    def plan_candidates(self, candidates, start_date, end_date, cache_lookup):
        self.plan_calls.append(
            {
                "candidates": candidates,
                "start_date": start_date,
                "end_date": end_date,
                "cache_lookup": cache_lookup,
            }
        )
        return self.planned_candidates


class FakeScannerAdapter:
    def __init__(
        self,
        *,
        discovered_files,
        inventory_items=None,
        cache_probes=None,
        planned_candidates=None,
        cached_contexts=None,
        uncached_context=None,
        run_id=77,
    ):
        self.work_dir = Path("work")
        self.scanner_cfg = {"max_workers": 1}
        self.discovery_service = FakeDiscoveryService(discovered_files)
        self.scan_index_store = FakeScanIndexStore(
            inventory_items=inventory_items,
            cache_probes=cache_probes,
            run_id=run_id,
        )
        self.scan_planner = FakeScanPlanner(
            planned_candidates
            or {"cached": [], "uncached": [], "total_candidates": 0}
        )
        self.cached_contexts = cached_contexts or []
        self.uncached_context = uncached_context
        self.last_reparse_details = ["stale"]
        self._office_parse_audits = {"stale": object()}
        self.cached_context_calls = []
        self.parse_calls = []
        self.recorded_reparse_details = []
        self.written_parse_cache = []
        self.recorded_exceptions = []

    def _normalize_discovered_files(self, discovered_files):
        return discovered_files

    def _get_cached_contexts(self, cached_files, parser_profile):
        self.cached_context_calls.append((cached_files, parser_profile))
        return self.cached_contexts

    def _extract_uncached_content_with_duration(self, item, limits):
        self.parse_calls.append((item, limits))
        if isinstance(self.uncached_context, Exception):
            raise self.uncached_context
        return self.uncached_context

    def _record_reparse_detail(self, item, cache_probe, duration_ms, context):
        self.recorded_reparse_details.append((item, cache_probe, duration_ms, context))

    def _write_parse_cache(self, item, parser_profile, context):
        self.written_parse_cache.append((item, parser_profile, context))

    def _record_reparse_exception(self, item, cache_probe, parse_error):
        self.recorded_exceptions.append((item, cache_probe, parse_error))

    def _item_path(self, item):
        return Path(item.path)

    def _item_identity(self, item):
        return item.file_identity

    def _item_extension(self, item):
        return item.extension

    def _item_source_version(self, item):
        return item.source_version


def _discovered_file(path: Path, source_version: str) -> DiscoveredFile:
    return DiscoveredFile(
        file_identity=f"bootstrap:{str(path).lower()}",
        path=path,
        extension=path.suffix.lower(),
        modified_at=datetime.combine(date(2026, 6, 11), datetime.min.time()),
        size_bytes=10,
        source_version=source_version,
    )


def _inventory_item(path: Path, source_version: str) -> InventoryItem:
    return InventoryItem(
        file_identity=f"bootstrap:{str(path).lower()}",
        path=path,
        extension=path.suffix.lower(),
        modified_date=date(2026, 6, 11),
        size_bytes=10,
        source_version=source_version,
    )


def _cache_probe(item: InventoryItem, status: str, reason: str) -> CacheProbe:
    return CacheProbe(
        file_identity=item.file_identity,
        parser_profile="profile-key",
        source_version=item.source_version,
        cache_status=status,
        cache_miss_reason=reason,
    )


def test_cold_scanner_run_persists_empty_run_and_clears_runtime_state():
    scanner = FakeScannerAdapter(discovered_files=[], run_id=123)

    result = ColdScannerRun(scanner).scan_files(
        start_date=date(2026, 6, 10),
        end_date=date(2026, 6, 11),
        summary_mode=False,
    )

    assert scanner.discovery_service.calls == [
        (date(2026, 6, 10), date(2026, 6, 11))
    ]
    assert scanner.scan_index_store.replaced_inventory == [[]]
    assert scanner.last_reparse_details == []
    assert scanner._office_parse_audits == {}
    assert result.total_files == 0
    assert result.success_count == 0
    assert result.error_count == 0
    assert result.contexts == []
    assert result.scan_run_id == 123

    [metrics] = scanner.scan_index_store.saved_metrics
    assert metrics.discovered_count == 0
    assert metrics.reused_count == 0
    assert metrics.reparsed_count == 0
    assert metrics.success_count == 0
    assert metrics.error_count == 0


def test_cold_scanner_run_orchestrates_inventory_cache_parse_and_aggregation():
    cached_path = Path("cached.md")
    uncached_path = Path("uncached.md")
    discovered = [
        _discovered_file(cached_path, "mtime_ns=1:size=10"),
        _discovered_file(uncached_path, "mtime_ns=2:size=10"),
    ]
    cached_item = _inventory_item(cached_path, "mtime_ns=1:size=10")
    uncached_item = _inventory_item(uncached_path, "mtime_ns=2:size=10")
    cached_context = FileContext(
        file_path=str(cached_path),
        file_type=".md",
        content="cached",
        error=None,
    )
    parsed_context = FileContext(
        file_path=str(uncached_path),
        file_type=".md",
        content="parsed",
        error=None,
        parser_backend="light_text_v1",
    )
    cache_probes = {
        cached_item.file_identity: _cache_probe(cached_item, "fresh", ""),
        uncached_item.file_identity: _cache_probe(uncached_item, "miss", "new_file"),
    }
    scanner = FakeScannerAdapter(
        discovered_files=discovered,
        inventory_items=[cached_item, uncached_item],
        cache_probes=cache_probes,
        planned_candidates={
            "cached": [cached_item],
            "uncached": [uncached_item],
            "total_candidates": 2,
        },
        cached_contexts=[cached_context],
        uncached_context=(parsed_context, 17),
        run_id=456,
    )

    result = ColdScannerRun(scanner).scan_files(
        start_date=date(2026, 6, 10),
        end_date=date(2026, 6, 11),
        summary_mode=True,
    )

    assert scanner.scan_planner.summary_modes == [True]
    assert scanner.scan_index_store.replaced_inventory == [
        [
            {
                "file_identity": discovered[0].file_identity,
                "path": str(cached_path),
                "extension": ".md",
                "modified_date": "2026-06-11",
                "size_bytes": 10,
                "source_version": "mtime_ns=1:size=10",
            },
            {
                "file_identity": discovered[1].file_identity,
                "path": str(uncached_path),
                "extension": ".md",
                "modified_date": "2026-06-11",
                "size_bytes": 10,
                "source_version": "mtime_ns=2:size=10",
            },
        ]
    ]
    assert scanner.scan_planner.plan_calls[0]["cache_lookup"] == {
        cached_item.file_identity: True,
        uncached_item.file_identity: False,
    }
    assert scanner.cached_context_calls == [([cached_item], "profile-key")]
    assert scanner.parse_calls == [
        (uncached_item, {"text_max_chars": 120})
    ]
    assert scanner.recorded_reparse_details == [
        (uncached_item, cache_probes[uncached_item.file_identity], 17, parsed_context)
    ]
    assert scanner.written_parse_cache == [
        (uncached_item, "profile-key", parsed_context)
    ]
    assert result.scan_run_id == 456
    assert result.total_files == 2
    assert result.success_count == 2
    assert result.error_count == 0
    assert [context.content for context in result.contexts] == ["cached", "parsed"]

    [metrics] = scanner.scan_index_store.saved_metrics
    assert metrics.discovered_count == 2
    assert metrics.reused_count == 1
    assert metrics.reparsed_count == 1
    assert metrics.success_count == 2
    assert metrics.error_count == 0

