from datetime import date
from pathlib import Path

from src.models.schemas import FileContext
from src.services.office_parser import OfficeParseAudit
from src.services.scan_index_store import CacheProbe, InventoryItem
from src.services.scanner_parse_cache import (
    build_reparse_detail,
    build_reparse_exception_detail,
    get_cached_contexts,
    write_parse_cache,
)


class FakeParseCacheStore:
    def __init__(self, cached_rows=None):
        self.cached_rows = cached_rows or {}
        self.upserts = []

    def load_parse_cache(self, file_identity, parser_profile, source_version=""):
        return self.cached_rows[(file_identity, parser_profile, source_version)]

    def upsert_parse_cache(self, **kwargs):
        self.upserts.append(kwargs)


def _inventory_item(path: Path) -> InventoryItem:
    return InventoryItem(
        file_identity=f"bootstrap:{str(path).lower()}",
        path=path,
        extension=path.suffix.lower(),
        modified_date=date(2026, 6, 11),
        size_bytes=10,
        source_version="mtime_ns=1:size=10",
    )


def _cache_probe(item: InventoryItem) -> CacheProbe:
    return CacheProbe(
        file_identity=item.file_identity,
        parser_profile="profile-key",
        source_version=item.source_version,
        cache_status="miss",
        cache_miss_reason="new_file",
        previous_source_version="mtime_ns=0:size=10",
    )


def test_get_cached_contexts_restores_file_context_metadata():
    item = _inventory_item(Path("/work/report.md"))
    store = FakeParseCacheStore(
        {
            (item.file_identity, "profile-key", item.source_version): {
                "content_excerpt": "cached",
                "parse_status": "success",
                "parse_error": "",
                "parser_backend": "light_text_v1",
                "truncated": 1,
            }
        }
    )

    [context] = get_cached_contexts(store, [item], "profile-key")

    assert context == FileContext(
        file_path="/work/report.md",
        file_type=".md",
        content="cached",
        error=None,
        parser_backend="light_text_v1",
        truncated=True,
    )


def test_write_parse_cache_omits_error_content_and_preserves_parser_metadata():
    item = _inventory_item(Path("/work/bad.md"))
    store = FakeParseCacheStore()
    context = FileContext(
        file_path="/work/bad.md",
        file_type=".md",
        content="do not cache failed content",
        error="parse failed",
        parser_backend="not_parsed",
        truncated=True,
    )

    write_parse_cache(store, item, "profile-key", context)

    assert store.upserts == [
        {
            "file_identity": item.file_identity,
            "parser_profile": "profile-key",
            "content_excerpt": "",
            "parse_status": "error",
            "parse_error": "parse failed",
            "source_version": item.source_version,
            "parser_backend": "not_parsed",
            "truncated": True,
        }
    ]


def test_build_reparse_detail_includes_office_audit_and_worker_lane():
    item = _inventory_item(Path("/work/report.docx"))
    context = FileContext(
        file_path="/work/report.docx",
        file_type=".docx",
        content="parsed",
        error=None,
        parser_backend="python_office_v1",
        truncated=False,
    )
    office_audit = OfficeParseAudit(
        attempted_backend="rust_office_oxide_v1",
        fallback_backend="python_office_v1",
        fallback_reason="RUST_OFFICE_START_FAILED",
        rust_duration_ms=3,
        fallback_duration_ms=7,
        failure_class="environment_unavailable",
    )

    detail = build_reparse_detail(
        item=item,
        cache_probe=_cache_probe(item),
        duration_ms=17,
        context=context,
        office_parse_audits={"/work/report.docx": office_audit},
        infer_worker_lane=lambda file_type, parsed_context: "subprocess",
    )

    assert detail.path == "/work/report.docx"
    assert detail.extension == ".docx"
    assert detail.parse_status == "success"
    assert detail.parser_backend == "python_office_v1"
    assert detail.worker_lane == "subprocess"
    assert detail.attempted_backend == "rust_office_oxide_v1"
    assert detail.fallback_backend == "python_office_v1"
    assert detail.failure_class == "environment_unavailable"
    assert detail.rust_duration_ms == 3
    assert detail.fallback_duration_ms == 7


def test_build_reparse_exception_detail_uses_not_parsed_lane():
    item = _inventory_item(Path("/work/bad.txt"))

    detail = build_reparse_exception_detail(
        item=item,
        cache_probe=_cache_probe(item),
        parse_error="boom",
        not_parsed_backend="not_parsed",
    )

    assert detail.path == "/work/bad.txt"
    assert detail.parse_status == "error"
    assert detail.parse_error == "boom"
    assert detail.parser_backend == "not_parsed"
    assert detail.worker_lane == "not_parsed"
    assert detail.truncated is False
