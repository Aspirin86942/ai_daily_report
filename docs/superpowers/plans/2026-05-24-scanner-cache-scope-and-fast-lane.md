# Scanner Cache Scope And Fast Lane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make scanner excluded directories actually take effect, explain every warm-cache reparse, and avoid Windows `spawn` overhead for bounded text-like parsing.

**Architecture:** Keep the current scanner boundaries: config builds plain scanner settings, discovery filters candidates, index store owns cache truth, `FileScanner` orchestrates, and benchmark renders evidence. Add cache probe and reparse detail as observability surfaces without weakening `file_identity + parser_profile + source_version` freshness.

**Tech Stack:** Python 3.10+, pytest, SQLite, Dynaconf, existing `FileScanner`, `ScanIndexStore`, `ScanMetricsCollector`, and `scripts/benchmark_scanner.py`.

---

## File Structure

- Modify `src/core/config.py`
  - Add `excluded_dirs` to `Config.scanner_config`.
  - Keep conversion through `_to_builtin_value()` so scanner config remains pickleable on Windows `spawn`.
- Modify `tests/test_config.py`
  - Cover default `excluded_dirs=[]`.
  - Cover explicit `excluded_dirs` pass-through.
  - Keep pickle assertion.
- Modify `src/services/scan_index_store.py`
  - Add `CacheProbe` dataclass.
  - Add `probe_parse_cache()` method for read-only cache status explanation.
- Modify `tests/test_scan_index_store.py`
  - Cover `fresh`, `new_file`, `source_version_changed`, `parser_profile_changed`, and `error_cache`.
- Modify `src/services/scan_metrics.py`
  - Add `ReparseDetail` dataclass with stable `to_dict()`.
- Modify `tests/test_scan_metrics.py`
  - Cover `ReparseDetail.to_dict()`.
- Modify `src/services/file_scanner.py`
  - Reset and expose `last_reparse_details`.
  - Use `probe_parse_cache()` for planning-level reasons.
  - Record one `ReparseDetail` for each uncached file parse attempt.
  - Add text-like direct parse path for `.txt`, `.md`, `.csv`, `.json`, `.log`.
- Modify `tests/test_file_scanner.py`
  - Update tests that need subprocess behavior to set `worker_lane_mode="subprocess"`.
  - Add direct-lane and heavy-format fallback tests.
  - Add assertions for cache miss reasons.
- Modify `scripts/benchmark_scanner.py`
  - Add `reparse_details` to payload and Markdown.
- Modify `tests/test_benchmark_scanner.py`
  - Cover JSON payload and Markdown table for reparse details.

## Task 1: Pass `excluded_dirs` Through Config

**Files:**
- Modify: `tests/test_config.py`
- Modify: `src/core/config.py`

- [ ] **Step 1: Write failing config tests**

Append these assertions and test to `tests/test_config.py`.

```python
def test_scanner_config_exposes_scan_index_defaults_when_keys_absent():
    """旧配置缺少新增键时，应使用扫描索引和 parser profile 默认值。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".txt"],
            ignored_patterns=[],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
        )
    )

    scanner_config = cfg.scanner_config

    assert scanner_config["index_db_path"] == "data/db/scan_index.sqlite3"
    assert scanner_config["parser_profile_version"] == "v1"
    assert scanner_config["worker_lane_mode"] == "direct"
    assert scanner_config["excluded_dirs"] == []


def test_scanner_config_passes_excluded_dirs_as_builtin_list():
    """excluded_dirs 必须从 settings 透传到 scanner，并转成普通 list。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".txt"],
            ignored_patterns=[],
            excluded_dirs=["D:\\work\\skip", "D:\\work\\logs"],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
        )
    )

    scanner_config = cfg.scanner_config

    assert scanner_config["excluded_dirs"] == ["D:\\work\\skip", "D:\\work\\logs"]
    assert isinstance(scanner_config["excluded_dirs"], list)
```

Also add one assertion to `test_scanner_config_uses_builtin_containers_and_is_picklable()`:

```python
assert isinstance(scanner_config["excluded_dirs"], list)
```

- [ ] **Step 2: Run config tests and verify failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_config.py -q
```

Expected before implementation: failure with `KeyError: 'excluded_dirs'`.

- [ ] **Step 3: Implement config pass-through**

In `src/core/config.py`, update the initial `cfg` dict inside `Config.scanner_config`:

```python
cfg: Dict[str, Any] = {
    "allowed_extensions": self._to_builtin_value(
        self._settings.scanner.allowed_extensions
    ),
    "ignored_patterns": self._to_builtin_value(
        self._settings.scanner.ignored_patterns
    ),
    "excluded_dirs": self._to_builtin_value(
        getattr(self._settings.scanner, "excluded_dirs", [])
    ),
    "max_workers": self._settings.scanner.max_workers,
    "excel_max_rows": self._settings.scanner.excel_max_rows,
    "pdf_max_pages": self._settings.scanner.pdf_max_pages,
    "text_max_chars": self._settings.scanner.text_max_chars,
    "index_db_path": getattr(
        self._settings.scanner,
        "index_db_path",
        "data/db/scan_index.sqlite3",
    ),
    "parser_profile_version": getattr(
        self._settings.scanner,
        "parser_profile_version",
        "v1",
    ),
    "worker_lane_mode": getattr(
        self._settings.scanner,
        "worker_lane_mode",
        "direct",
    ),
}
```

- [ ] **Step 4: Run config tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_config.py -q
```

Expected: all tests in `tests/test_config.py` pass.

- [ ] **Step 5: Commit Task 1**

Run:

```powershell
git add src/core/config.py tests/test_config.py
git commit -m "fix: pass scanner excluded dirs through config"
```

## Task 2: Add Cache Probe Reasons

**Files:**
- Modify: `src/services/scan_index_store.py`
- Modify: `tests/test_scan_index_store.py`

- [ ] **Step 1: Write failing cache probe tests**

Append these tests to `tests/test_scan_index_store.py`.

```python
def test_probe_parse_cache_returns_fresh_for_exact_success(tmp_path: Path):
    """完全匹配的 success cache 应解释为 fresh。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    store.upsert_parse_cache(
        file_identity="file-1",
        parser_profile="profile-a",
        source_version="mtime=1:size=10",
        content_excerpt="cached",
        parse_status="success",
        parse_error="",
    )

    probe = store.probe_parse_cache(
        "file-1",
        "profile-a",
        source_version="mtime=1:size=10",
    )

    assert probe.cache_status == "fresh"
    assert probe.cache_miss_reason == ""
    assert probe.previous_source_version is None


def test_probe_parse_cache_returns_new_file_when_no_history(tmp_path: Path):
    """完全无历史 cache 时应解释为 new_file。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")

    probe = store.probe_parse_cache(
        "missing",
        "profile-a",
        source_version="mtime=1:size=10",
    )

    assert probe.cache_status == "miss"
    assert probe.cache_miss_reason == "new_file"
    assert probe.previous_source_version is None


def test_probe_parse_cache_returns_source_version_changed(tmp_path: Path):
    """同身份同 profile 但 source_version 不同时，应解释为 source_version_changed。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    store.upsert_parse_cache(
        file_identity="file-1",
        parser_profile="profile-a",
        source_version="mtime=1:size=10",
        content_excerpt="old",
        parse_status="success",
        parse_error="",
    )

    probe = store.probe_parse_cache(
        "file-1",
        "profile-a",
        source_version="mtime=2:size=10",
    )

    assert probe.cache_status == "miss"
    assert probe.cache_miss_reason == "source_version_changed"
    assert probe.previous_source_version == "mtime=1:size=10"


def test_probe_parse_cache_returns_parser_profile_changed(tmp_path: Path):
    """同身份存在 cache 但 profile 不同时，应解释为 parser_profile_changed。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    store.upsert_parse_cache(
        file_identity="file-1",
        parser_profile="profile-a",
        source_version="mtime=1:size=10",
        content_excerpt="old",
        parse_status="success",
        parse_error="",
    )

    probe = store.probe_parse_cache(
        "file-1",
        "profile-b",
        source_version="mtime=1:size=10",
    )

    assert probe.cache_status == "miss"
    assert probe.cache_miss_reason == "parser_profile_changed"
    assert probe.previous_source_version == "mtime=1:size=10"


def test_probe_parse_cache_returns_error_cache_for_exact_error(tmp_path: Path):
    """同版本只有 error cache 时，应解释为 error_cache 而不是 fresh。"""
    store = ScanIndexStore(db_path=tmp_path / "scan_index.sqlite3")
    store.upsert_parse_cache(
        file_identity="file-1",
        parser_profile="profile-a",
        source_version="mtime=1:size=10",
        content_excerpt="",
        parse_status="error",
        parse_error="boom",
    )

    probe = store.probe_parse_cache(
        "file-1",
        "profile-a",
        source_version="mtime=1:size=10",
    )

    assert probe.cache_status == "miss"
    assert probe.cache_miss_reason == "error_cache"
    assert probe.previous_source_version == "mtime=1:size=10"
```

- [ ] **Step 2: Run cache probe tests and verify failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_index_store.py -q
```

Expected before implementation: failure with `AttributeError: 'ScanIndexStore' object has no attribute 'probe_parse_cache'`.

- [ ] **Step 3: Implement `CacheProbe` and `probe_parse_cache()`**

In `src/services/scan_index_store.py`, add this dataclass after `InventoryItem`:

```python
@dataclass(frozen=True, slots=True)
class CacheProbe:
    """解释一次 parse cache freshness 判断结果。"""

    file_identity: str
    parser_profile: str
    source_version: str
    cache_status: str
    cache_miss_reason: str
    previous_source_version: str | None = None
```

Add this method to `ScanIndexStore` near `has_fresh_cache()`:

```python
def probe_parse_cache(
    self,
    file_identity: str,
    parser_profile: str,
    source_version: str = "",
) -> CacheProbe:
    """解释 parse cache 是否 fresh，以及不 fresh 的原因。"""
    with self._connect() as conn:
        rows = conn.execute(
            """
            SELECT parser_profile, source_version, parse_status, updated_at
            FROM parse_cache
            WHERE file_identity = ?
            ORDER BY updated_at DESC
            """,
            (file_identity,),
        ).fetchall()

    if not rows:
        return CacheProbe(
            file_identity=file_identity,
            parser_profile=parser_profile,
            source_version=source_version,
            cache_status="miss",
            cache_miss_reason="new_file",
        )

    exact_rows = [
        row
        for row in rows
        if str(row["parser_profile"]) == parser_profile
        and str(row["source_version"]) == source_version
    ]
    if exact_rows:
        latest_exact = exact_rows[0]
        if str(latest_exact["parse_status"]) == "success":
            return CacheProbe(
                file_identity=file_identity,
                parser_profile=parser_profile,
                source_version=source_version,
                cache_status="fresh",
                cache_miss_reason="",
            )
        return CacheProbe(
            file_identity=file_identity,
            parser_profile=parser_profile,
            source_version=source_version,
            cache_status="miss",
            cache_miss_reason="error_cache",
            previous_source_version=str(latest_exact["source_version"]),
        )

    same_profile_rows = [
        row
        for row in rows
        if str(row["parser_profile"]) == parser_profile
    ]
    same_profile_success = [
        row for row in same_profile_rows if str(row["parse_status"]) == "success"
    ]
    if same_profile_success:
        return CacheProbe(
            file_identity=file_identity,
            parser_profile=parser_profile,
            source_version=source_version,
            cache_status="miss",
            cache_miss_reason="source_version_changed",
            previous_source_version=str(same_profile_success[0]["source_version"]),
        )

    if same_profile_rows:
        return CacheProbe(
            file_identity=file_identity,
            parser_profile=parser_profile,
            source_version=source_version,
            cache_status="miss",
            cache_miss_reason="source_version_changed",
            previous_source_version=str(same_profile_rows[0]["source_version"]),
        )

    return CacheProbe(
        file_identity=file_identity,
        parser_profile=parser_profile,
        source_version=source_version,
        cache_status="miss",
        cache_miss_reason="parser_profile_changed",
        previous_source_version=str(rows[0]["source_version"]),
    )
```

- [ ] **Step 4: Run cache probe tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_index_store.py -q
```

Expected: all tests in `tests/test_scan_index_store.py` pass.

- [ ] **Step 5: Commit Task 2**

Run:

```powershell
git add src/services/scan_index_store.py tests/test_scan_index_store.py
git commit -m "feat: explain scanner parse cache misses"
```

## Task 3: Add Reparse Detail Model

**Files:**
- Modify: `src/services/scan_metrics.py`
- Modify: `tests/test_scan_metrics.py`

- [ ] **Step 1: Write failing reparse detail test**

Append this test to `tests/test_scan_metrics.py`.

```python
def test_reparse_detail_serializes_stable_payload():
    """重解析明细应输出 benchmark 需要的稳定字段。"""
    detail = ReparseDetail(
        path="D:\\work\\report.md",
        extension=".md",
        file_identity="bootstrap:d:\\work\\report.md",
        source_version="mtime=2:size=10",
        cache_status="miss",
        cache_miss_reason="source_version_changed",
        previous_source_version="mtime=1:size=10",
        parse_duration_ms=12,
        parse_status="success",
        parse_error="",
    )

    assert detail.to_dict() == {
        "path": "D:\\work\\report.md",
        "extension": ".md",
        "file_identity": "bootstrap:d:\\work\\report.md",
        "source_version": "mtime=2:size=10",
        "cache_status": "miss",
        "cache_miss_reason": "source_version_changed",
        "previous_source_version": "mtime=1:size=10",
        "parse_duration_ms": 12,
        "parse_status": "success",
        "parse_error": "",
    }
```

Update the imports at the top of `tests/test_scan_metrics.py`:

```python
from src.services.scan_metrics import ExtensionMetrics, ReparseDetail, ScanMetricsCollector
```

- [ ] **Step 2: Run scan metrics tests and verify failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_metrics.py -q
```

Expected before implementation: failure with `ImportError` or `NameError` for `ReparseDetail`.

- [ ] **Step 3: Implement `ReparseDetail`**

In `src/services/scan_metrics.py`, add this dataclass after `ExtensionMetrics`:

```python
@dataclass(slots=True)
class ReparseDetail:
    """单个重解析文件的 cache miss 与解析结果明细。"""

    path: str
    extension: str
    file_identity: str
    source_version: str
    cache_status: str
    cache_miss_reason: str
    previous_source_version: str | None = None
    parse_duration_ms: int = 0
    parse_status: str = "success"
    parse_error: str = ""

    def to_dict(self) -> dict[str, int | str | None]:
        """转成 benchmark JSON / Markdown 共用的稳定结构。"""
        return {
            "path": self.path,
            "extension": self.extension,
            "file_identity": self.file_identity,
            "source_version": self.source_version,
            "cache_status": self.cache_status,
            "cache_miss_reason": self.cache_miss_reason,
            "previous_source_version": self.previous_source_version,
            "parse_duration_ms": max(0, int(self.parse_duration_ms)),
            "parse_status": self.parse_status,
            "parse_error": self.parse_error,
        }
```

- [ ] **Step 4: Run scan metrics tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_scan_metrics.py -q
```

Expected: all tests in `tests/test_scan_metrics.py` pass.

- [ ] **Step 5: Commit Task 3**

Run:

```powershell
git add src/services/scan_metrics.py tests/test_scan_metrics.py
git commit -m "feat: add scanner reparse detail model"
```

## Task 4: Integrate Cache Probe And Text-Like Direct Lane In Scanner

**Files:**
- Modify: `src/services/file_scanner.py`
- Modify: `tests/test_file_scanner.py`

- [ ] **Step 1: Write failing scanner tests for direct lane and reparse reason**

Append these tests to `tests/test_file_scanner.py`.

```python
def test_scan_files_uses_direct_parse_for_text_like_files(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """direct 模式下 text-like 文件不应进入 subprocess 路径。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".md"], "worker_lane_mode": "direct"},
    )
    sample = scanner.work_dir / "direct.md"
    sample.write_text("direct content", encoding="utf-8")
    discovered = [_build_discovered_file(sample, "mtime_ns=1:size=14")]
    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )

    def fail_subprocess(file_path: Path, limits: dict):
        raise AssertionError("text-like file should not use subprocess")

    monkeypatch.setattr(scanner, "_extract_content_with_timeout", fail_subprocess)

    result = scanner.scan_files(date.today(), date.today())

    assert result.total_files == 1
    assert result.success_count == 1
    assert result.contexts[0].content == "direct content"
    assert [detail.cache_miss_reason for detail in scanner.last_reparse_details] == [
        "new_file"
    ]


def test_scan_files_keeps_subprocess_path_for_pdf_in_direct_mode(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """direct 模式只覆盖 text-like 文件，PDF 仍走 timeout/subprocess 入口。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".pdf"], "worker_lane_mode": "direct"},
    )
    sample = scanner.work_dir / "report.pdf"
    sample.write_text("not a real pdf", encoding="utf-8")
    discovered = [_build_discovered_file(sample, "mtime_ns=1:size=14")]
    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )
    subprocess_calls: list[Path] = []

    def fake_subprocess(file_path: Path, limits: dict) -> file_scanner_module.FileContext:
        subprocess_calls.append(file_path)
        return file_scanner_module.FileContext(
            file_path=str(file_path),
            file_type=".pdf",
            content="pdf parsed through subprocess",
            error=None,
        )

    monkeypatch.setattr(scanner, "_extract_content_with_timeout", fake_subprocess)

    result = scanner.scan_files(date.today(), date.today())

    assert subprocess_calls == [sample]
    assert result.success_count == 1
    assert result.contexts[0].content == "pdf parsed through subprocess"


def test_scan_files_records_source_version_changed_reparse_detail(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """source_version 变化时，重解析明细应保留原因和上一版本。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".txt"], "worker_lane_mode": "subprocess"},
    )
    sample = scanner.work_dir / "report.txt"
    sample.write_text("new content", encoding="utf-8")
    file_identity = f"bootstrap:{str(sample.resolve()).lower()}"
    parser_profile_key = scanner.scan_planner.serialize_parser_profile(
        scanner.scan_planner.build_parser_profile(summary_mode=False)
    )
    scanner.scan_index_store.upsert_parse_cache(
        file_identity=file_identity,
        parser_profile=parser_profile_key,
        content_excerpt="old cached content",
        parse_status="success",
        parse_error="",
        source_version="mtime_ns=1:size=11",
    )
    discovered = [_build_discovered_file(sample, "mtime_ns=2:size=11")]
    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )
    monkeypatch.setattr(
        scanner,
        "_extract_content_with_timeout",
        lambda file_path, limits: file_scanner_module.FileContext(
            file_path=str(file_path),
            file_type=".txt",
            content="new parsed content",
            error=None,
        ),
    )

    result = scanner.scan_files(date.today(), date.today())

    assert result.success_count == 1
    assert len(scanner.last_reparse_details) == 1
    detail = scanner.last_reparse_details[0]
    assert detail.cache_status == "miss"
    assert detail.cache_miss_reason == "source_version_changed"
    assert detail.previous_source_version == "mtime_ns=1:size=11"
    assert detail.parse_status == "success"
```

- [ ] **Step 2: Update existing tests that intentionally monkeypatch subprocess for text files**

For tests that monkeypatch `_extract_content_with_timeout` and expect `.txt` parsing to use that patched method, add `worker_lane_mode: "subprocess"` in `_make_scanner()` overrides.

Update these test constructors:

```python
scanner = _make_scanner(
    tmp_path,
    monkeypatch,
    {"allowed_extensions": [".txt"], "worker_lane_mode": "subprocess"},
)
```

Apply that pattern to tests whose assertions depend on `_extract_content_with_timeout` being called:

- `test_scan_files_empty_range_clears_inventory_snapshot`
- `test_scan_files_records_timeout_and_continues`
- `test_scan_files_delegates_bootstrap_discovery`
- `test_scan_files_counts_cached_and_uncached_contexts`
- `test_scan_files_reparses_when_source_version_changes`
- `test_scan_files_writes_error_cache_when_parser_raises`
- `test_scan_files_reparses_when_only_error_cache_exists_for_same_source_version`

- [ ] **Step 3: Run scanner tests and verify failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_file_scanner.py -q
```

Expected before implementation: new direct-lane and `last_reparse_details` tests fail.

- [ ] **Step 4: Implement scanner integration**

In `src/services/file_scanner.py`, update imports:

```python
from .scan_metrics import ReparseDetail, ScanMetricsCollector
```

In `FileScanner.__init__()`, initialize the latest detail list:

```python
self.last_reparse_details: list[ReparseDetail] = []
```

At the start of `scan_files()` after `metrics = ScanMetricsCollector.start()`, reset the list:

```python
self.last_reparse_details = []
```

In the `inventory_cache` block, replace `cache_lookup` with probe-backed lookup:

```python
cache_probes = {
    item.file_identity: self.scan_index_store.probe_parse_cache(
        item.file_identity,
        parser_profile_key,
        source_version=item.source_version,
    )
    for item in inventory_items
}
cache_lookup = {
    file_identity: probe.cache_status == "fresh"
    for file_identity, probe in cache_probes.items()
}
```

In the parse submit block, replace `_extract_content_with_duration` with `_extract_uncached_content_with_duration`:

```python
future_to_file = {
    executor.submit(
        self._extract_uncached_content_with_duration,
        item,
        limits,
    ): item
    for item in planned_candidates["uncached"]
}
```

After `metrics.record_extension_result(...)` and before `_write_parse_cache(...)`, record the detail:

```python
self._record_reparse_detail(
    inventory_item,
    cache_probes[self._item_identity(inventory_item)],
    duration_ms,
    context,
)
```

In the exception branch, add detail before `aggregator.add_exception(file_path, e)`:

```python
self._record_reparse_exception(
    inventory_item,
    cache_probes[self._item_identity(inventory_item)],
    str(e),
)
```

Add these helper methods to `FileScanner` near `_extract_content_with_duration()`:

```python
def _extract_uncached_content_with_duration(
    self,
    item: Path | InventoryItem,
    limits: Optional[dict] = None,
) -> tuple[FileContext, int]:
    """解析未缓存文件，并返回本 worker 内部 wall clock 耗时。"""
    started_at = perf_counter()
    context = self._extract_uncached_content(
        self._item_path(item),
        self._item_extension(item),
        limits,
    )
    duration_ms = int(round((perf_counter() - started_at) * 1000))
    return context, max(0, duration_ms)


def _extract_uncached_content(
    self,
    file_path: Path,
    file_type: str,
    limits: Optional[dict] = None,
) -> FileContext:
    """根据文件类型选择 direct text lane 或 subprocess timeout lane。"""
    if self._should_parse_direct(file_type):
        return self.parser_supervisor.parse_file(
            file_path=file_path,
            file_type=file_type,
            limits=limits or {},
            direct_parse=self._extract_content,
        )
    return self._extract_content_with_timeout(file_path, limits)


def _should_parse_direct(self, file_type: str) -> bool:
    """text-like 文件读取受限，direct 模式下避免 Windows spawn 固定开销。"""
    return (
        str(self.scanner_cfg.get("worker_lane_mode", "direct")).lower() == "direct"
        and file_type.lower() in TEXT_FILE_TYPES
    )
```

Add these detail helpers near `_write_parse_cache()`:

```python
def _record_reparse_detail(
    self,
    item: Path | InventoryItem,
    cache_probe,
    duration_ms: int,
    context: FileContext,
) -> None:
    """记录单个重解析文件的 cache miss 原因和解析结果。"""
    self.last_reparse_details.append(
        ReparseDetail(
            path=str(self._item_path(item)),
            extension=self._item_extension(item),
            file_identity=self._item_identity(item),
            source_version=self._item_source_version(item),
            cache_status=cache_probe.cache_status,
            cache_miss_reason=cache_probe.cache_miss_reason,
            previous_source_version=cache_probe.previous_source_version,
            parse_duration_ms=duration_ms,
            parse_status="error" if context.error else "success",
            parse_error=context.error or "",
        )
    )


def _record_reparse_exception(
    self,
    item: Path | InventoryItem,
    cache_probe,
    parse_error: str,
) -> None:
    """解析入口抛异常时，也要留下 benchmark 可见的重解析明细。"""
    self.last_reparse_details.append(
        ReparseDetail(
            path=str(self._item_path(item)),
            extension=self._item_extension(item),
            file_identity=self._item_identity(item),
            source_version=self._item_source_version(item),
            cache_status=cache_probe.cache_status,
            cache_miss_reason=cache_probe.cache_miss_reason,
            previous_source_version=cache_probe.previous_source_version,
            parse_duration_ms=0,
            parse_status="error",
            parse_error=parse_error,
        )
    )
```

- [ ] **Step 5: Run scanner tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_file_scanner.py -q
```

Expected: all tests in `tests/test_file_scanner.py` pass.

- [ ] **Step 6: Commit Task 4**

Run:

```powershell
git add src/services/file_scanner.py tests/test_file_scanner.py
git commit -m "feat: add scanner direct text parse lane"
```

## Task 5: Include Reparse Details In Benchmark Output

**Files:**
- Modify: `scripts/benchmark_scanner.py`
- Modify: `tests/test_benchmark_scanner.py`

- [ ] **Step 1: Write failing benchmark tests**

Update the import in `tests/test_benchmark_scanner.py`:

```python
from src.services.scan_metrics import ExtensionMetrics, ReparseDetail
```

In `test_build_benchmark_payload_uses_scan_result_and_metrics()`, pass `reparse_details`:

```python
reparse_details = [
    ReparseDetail(
        path="D:\\work\\report.md",
        extension=".md",
        file_identity="bootstrap:d:\\work\\report.md",
        source_version="mtime=2:size=10",
        cache_status="miss",
        cache_miss_reason="source_version_changed",
        previous_source_version="mtime=1:size=10",
        parse_duration_ms=12,
        parse_status="success",
        parse_error="",
    )
]

payload = build_benchmark_payload(
    scan_result=scan_result,
    run_detail=run_detail,
    extension_metrics=extension_metrics,
    reparse_details=reparse_details,
    start_date=date(2026, 5, 23),
    end_date=date(2026, 5, 24),
    summary_mode=True,
)

assert payload["reparse_details"] == [
    {
        "path": "D:\\work\\report.md",
        "extension": ".md",
        "file_identity": "bootstrap:d:\\work\\report.md",
        "source_version": "mtime=2:size=10",
        "cache_status": "miss",
        "cache_miss_reason": "source_version_changed",
        "previous_source_version": "mtime=1:size=10",
        "parse_duration_ms": 12,
        "parse_status": "success",
        "parse_error": "",
    }
]
```

In `test_render_markdown_report_contains_stage_and_extension_metrics()`, add this `reparse_details` payload key:

```python
"reparse_details": [
    {
        "path": "D:\\work\\report.md",
        "extension": ".md",
        "file_identity": "bootstrap:d:\\work\\report.md",
        "source_version": "mtime=2:size=10",
        "cache_status": "miss",
        "cache_miss_reason": "source_version_changed",
        "previous_source_version": "mtime=1:size=10",
        "parse_duration_ms": 12,
        "parse_status": "success",
        "parse_error": "",
    }
],
```

Add these Markdown assertions:

```python
assert "## Reparse Details" in markdown
assert "| .md | source_version_changed | 12 | success | D:\\work\\report.md |" in markdown
```

In `test_write_report_files_writes_utf8_json_and_markdown()`, add:

```python
"reparse_details": [],
```

- [ ] **Step 2: Run benchmark tests and verify failure**

Run:

```powershell
conda run -n test python -m pytest tests/test_benchmark_scanner.py -q
```

Expected before implementation: failure because `build_benchmark_payload()` does not accept `reparse_details`.

- [ ] **Step 3: Implement benchmark payload and Markdown output**

In `scripts/benchmark_scanner.py`, update imports:

```python
from src.services.scan_metrics import ExtensionMetrics, ReparseDetail  # noqa: E402
```

Update `build_benchmark_payload()` signature:

```python
def build_benchmark_payload(
    scan_result: ScanResult,
    run_detail: dict[str, int],
    extension_metrics: list[ExtensionMetrics],
    reparse_details: list[ReparseDetail],
    start_date: date,
    end_date: date,
    summary_mode: bool,
) -> dict[str, Any]:
```

Add this key to the returned payload:

```python
"reparse_details": [item.to_dict() for item in reparse_details],
```

In `render_markdown_report()`, read the details:

```python
reparse_details = payload.get("reparse_details", [])
```

Append this section after extension metrics:

```python
lines.extend(
    [
        "",
        "## Reparse Details",
        "",
        "| extension | cache_miss_reason | parse_duration_ms | parse_status | path |",
        "|---|---|---:|---|---|",
    ]
)
if reparse_details:
    for item in reparse_details:
        lines.append(
            "| {extension} | {cache_miss_reason} | {parse_duration_ms} | "
            "{parse_status} | {path} |".format(**item)
        )
else:
    lines.append("| (none) |  | 0 |  |  |")
```

In `run_benchmark()`, pass scanner details:

```python
return build_benchmark_payload(
    scan_result=scan_result,
    run_detail=run_detail,
    extension_metrics=extension_metrics,
    reparse_details=scanner.last_reparse_details,
    start_date=args.start_date,
    end_date=args.end_date,
    summary_mode=args.summary_mode,
)
```

- [ ] **Step 4: Run benchmark tests and verify pass**

Run:

```powershell
conda run -n test python -m pytest tests/test_benchmark_scanner.py -q
```

Expected: all tests in `tests/test_benchmark_scanner.py` pass.

- [ ] **Step 5: Commit Task 5**

Run:

```powershell
git add scripts/benchmark_scanner.py tests/test_benchmark_scanner.py
git commit -m "feat: include scanner reparse details in benchmark"
```

## Task 6: Final Verification And Benchmark Smoke

**Files:**
- No new code expected.
- Verify modified code and current scanner behavior.

- [ ] **Step 1: Run focused scanner-related tests**

Run:

```powershell
conda run -n test python -m pytest tests/test_config.py tests/test_scan_index_store.py tests/test_scan_metrics.py tests/test_file_scanner.py tests/test_benchmark_scanner.py -q
```

Expected: all selected tests pass.

- [ ] **Step 2: Run full test suite**

Run:

```powershell
conda run -n test python -m pytest tests -q
```

Expected: full suite passes.

- [ ] **Step 3: Run compile check**

Run:

```powershell
conda run -n test python -m compileall main.py src tests
```

Expected: command exits 0 and reports successful compilation.

- [ ] **Step 4: Run benchmark smoke into system temp directory**

Use a temp output directory so benchmark artifacts do not become scanner samples:

```powershell
$benchDir = Join-Path $env:TEMP 'ai_daily_report_benchmarks'
New-Item -ItemType Directory -Force -Path $benchDir | Out-Null
conda run -n test python scripts/benchmark_scanner.py --json-out (Join-Path $benchDir 'scanner_benchmark_post_fast_lane.json') --markdown-out (Join-Path $benchDir 'scanner_benchmark_post_fast_lane.md')
```

Expected:

- JSON includes top-level `reparse_details`.
- Markdown includes `## Reparse Details`.
- If `reparsed_count > 0`, every reparse has a visible `cache_miss_reason`.

- [ ] **Step 5: Inspect benchmark JSON for reparse reasons**

Run:

```powershell
Get-Content -Path (Join-Path $env:TEMP 'ai_daily_report_benchmarks\scanner_benchmark_post_fast_lane.json') -Encoding UTF8 | Select-String -Pattern '"reparse_details"|"cache_miss_reason"|"parse_duration_ms"'
```

Expected: output shows `reparse_details`; if the array is not empty, each item has `cache_miss_reason` and `parse_duration_ms`.

- [ ] **Step 6: Check working tree and commit any missed final edits**

Run:

```powershell
git status --short
```

Expected:

- No unstaged source/test/script changes from this implementation.
- Pre-existing user-local changes such as `config/settings.toml` should not be committed unless the user explicitly wants that local config change included.

If final implementation edits remain, inspect them and commit only implementation-related files:

```powershell
git add src/core/config.py src/services/scan_index_store.py src/services/scan_metrics.py src/services/file_scanner.py scripts/benchmark_scanner.py tests/test_config.py tests/test_scan_index_store.py tests/test_scan_metrics.py tests/test_file_scanner.py tests/test_benchmark_scanner.py
git commit -m "test: verify scanner cache scope fast lane"
```

## Final Acceptance Checklist

- [ ] `excluded_dirs` appears in `config.scanner_config`.
- [ ] `FileDiscoveryService` receives effective excluded dirs without hardcoded project paths.
- [ ] `probe_parse_cache()` explains `fresh`, `new_file`, `source_version_changed`, `parser_profile_changed`, and `error_cache`.
- [ ] `FileScanner.last_reparse_details` resets per scan and records every uncached parse attempt.
- [ ] `.txt`, `.md`, `.csv`, `.json`, and `.log` use direct parse only when `worker_lane_mode == "direct"`.
- [ ] PDF / Excel / PPT / Word still use subprocess timeout lane.
- [ ] Benchmark JSON and Markdown include reparse details.
- [ ] Full tests and compileall pass under `conda run -n test`.
