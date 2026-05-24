"""测试文件扫描器"""

from datetime import date, datetime, timedelta
from pathlib import Path
from types import SimpleNamespace

import pytest

import src.services.file_scanner as file_scanner_module
from src.services.file_scanner import FileScanner
from src.services.scan_discovery import DiscoveredFile, FileDiscoveryService


def _make_scanner(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    scanner_overrides: dict | None = None,
) -> FileScanner:
    """构造不依赖本机工作目录的扫描器。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    scanner_cfg = {
        "allowed_extensions": [
            ".xlsx",
            ".xls",
            ".pptx",
            ".pdf",
            ".txt",
            ".md",
            ".docx",
            ".csv",
            ".json",
            ".log",
        ],
        "ignored_patterns": ["~$*", "*.tmp"],
        "excluded_dirs": [],
        "max_workers": 2,
        "excel_max_rows": 50,
        "pdf_max_pages": 5,
        "text_max_chars": 6000,
        "summary_excel_max_rows": 10,
        "summary_pdf_max_pages": 2,
        "summary_text_max_chars": 2000,
        "total_max_chars": 50000,
        "max_file_size_mb": 50,
        "file_timeout_seconds": 30,
        "file_timeout_by_extension": {},
        "index_db_path": str(tmp_path / "data" / "db" / "scan_index.sqlite3"),
        "parser_profile_version": "v1",
    }
    if scanner_overrides:
        scanner_cfg.update(scanner_overrides)

    fake_config = SimpleNamespace(scanner_config=scanner_cfg, work_dir=work_dir)
    monkeypatch.setattr(file_scanner_module, "config", fake_config)
    return FileScanner()


def _build_discovered_file(
    sample: Path,
    source_version: str,
) -> DiscoveredFile:
    """构造带稳定元数据的发现结果，便于覆盖缓存相关路径。"""
    return DiscoveredFile(
        file_identity=f"bootstrap:{str(sample.resolve()).lower()}",
        path=sample,
        extension=sample.suffix.lower(),
        modified_at=datetime.combine(date.today(), datetime.min.time()),
        size_bytes=sample.stat().st_size,
        source_version=source_version,
    )


def test_file_scanner_init(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """测试扫描器初始化"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    assert scanner.work_dir.exists()
    assert scanner.scanner_cfg["max_workers"] > 0
    assert scanner.parser_supervisor is not None
    assert (
        scanner.scan_index_store.db_path
        == tmp_path / "data" / "db" / "scan_index.sqlite3"
    )


def test_scan_files_default_dates(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """测试 scan_files 默认日期参数"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    result = scanner.scan_files()
    assert result.total_files >= 0
    assert result.success_count + result.error_count == result.total_files


def test_scan_files_with_date_range(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """测试 scan_files 指定日期范围"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    today = date.today()
    yesterday = today - timedelta(days=1)
    result = scanner.scan_files(start_date=yesterday, end_date=today)
    assert result.total_files >= 0


def test_scan_files_summary_mode(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """测试 scan_files summary_mode 使用缩减限制"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    today = date.today()
    yesterday = today - timedelta(days=1)
    result = scanner.scan_files(start_date=yesterday, end_date=today, summary_mode=True)
    assert result.total_files >= 0


def test_scan_files_empty_range(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """测试不存在文件的日期范围"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    # 使用很远的过去日期，应该没有文件
    old_start = date(2000, 1, 1)
    old_end = date(2000, 1, 2)
    result = scanner.scan_files(start_date=old_start, end_date=old_end)
    assert result.total_files == 0
    assert result.success_count == 0
    assert scanner.scan_index_store.latest_scan_run() == {
        "discovered_count": 0,
        "reused_count": 0,
        "reparsed_count": 0,
    }


def test_scan_files_empty_range_clears_inventory_snapshot(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """空扫描也应覆盖 inventory 快照，避免沿用上一轮发现结果。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".txt"], "worker_lane_mode": "subprocess"},
    )
    sample = scanner.work_dir / "report.txt"
    sample.write_text("hello", encoding="utf-8")
    discovered = [_build_discovered_file(sample, "mtime_ns=1:size=5")]

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
            content="hello",
            error=None,
        ),
    )

    scanner.scan_files(date.today(), date.today())
    assert scanner.scan_index_store.query_inventory(date.today(), date.today())

    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: [],
    )

    scanner.scan_files(date.today(), date.today())

    assert scanner.scan_index_store.query_inventory(date.today(), date.today()) == []


def test_scan_today_files_default_date_range(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """测试 scan_today_files 默认日期范围封装"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    result = scanner.scan_today_files()
    assert result.total_files >= 0


def test_get_files_in_range(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """测试 _get_files_in_range 方法"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    today = date.today()
    yesterday = today - timedelta(days=1)
    files = scanner._get_files_in_range(yesterday, today)
    assert isinstance(files, list)
    for f in files:
        assert isinstance(f, Path)

    # 验证排除目录配置生效（如果配置了排除目录）
    excluded_dirs = scanner.scanner_cfg.get("excluded_dirs", [])
    if excluded_dirs:
        for excluded_dir in excluded_dirs:
            excluded_path = Path(excluded_dir).resolve()
            for f in files:
                # 确保没有文件来自排除目录
                try:
                    f.resolve().relative_to(excluded_path)
                    assert False, f"文件 {f} 不应该来自排除目录 {excluded_dir}"
                except ValueError:
                    pass  # 正常，文件不在排除目录中


def test_extract_content_allows_success(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """成功路径应当构造 error=None 的 FileContext"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    sample = tmp_path / "example.txt"
    sample.write_text("example content", encoding="utf-8")
    context = scanner._extract_content(sample)
    assert context.error is None
    assert "example" in context.content


def test_get_files_in_range_matches_extensions_case_insensitively_and_uses_globs(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """扩展名匹配应忽略大小写，忽略规则应按 glob 生效。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".md", ".tmp"]},
    )
    (scanner.work_dir / "KEEP.MD").write_text("keep", encoding="utf-8")
    (scanner.work_dir / "~$lock.md").write_text("ignored", encoding="utf-8")
    (scanner.work_dir / "scratch.tmp").write_text("ignored", encoding="utf-8")

    files = scanner._get_files_in_range(date.today(), date.today())

    assert [path.name for path in files] == ["KEEP.MD"]


def test_extract_content_supports_csv_json_and_log_as_text(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """常见轻量文本数据文件应进入扫描上下文。"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    samples = {
        "sample.csv": "name,value\nalpha,1\n",
        "sample.json": '{"name": "alpha"}',
        "sample.log": "INFO finished",
    }

    for filename, content in samples.items():
        sample = tmp_path / filename
        sample.write_text(content, encoding="utf-8")
        context = scanner._extract_content(sample)
        assert context.error is None
        assert "alpha" in context.content or "finished" in context.content


def test_extract_content_skips_files_over_size_limit(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """超出文件大小上限时应跳过解析并留下可审计错误。"""
    scanner = _make_scanner(tmp_path, monkeypatch, {"max_file_size_mb": 0.001})
    sample = tmp_path / "large.txt"
    sample.write_text("x" * 4096, encoding="utf-8")

    context = scanner._extract_content(sample)

    assert context.content == ""
    assert context.error is not None
    assert context.error.startswith("file too large:")


def test_extract_content_truncates_text_without_reading_unbounded_content(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """文本类文件应按字符预算读取并截断，避免先整文件读入。"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    sample = tmp_path / "long.log"
    sample.write_text("a" * 100, encoding="utf-8")

    context = scanner._extract_content(sample, {"text_max_chars": 10})

    assert context.error is None
    assert context.content == "a" * 10 + "\n...(内容过长已截断)"


def test_resolve_file_timeout_uses_extension_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """单文件超时应支持按扩展名覆盖。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {
            "file_timeout_seconds": 30,
            "file_timeout_by_extension": {".pdf": 45},
        },
    )

    assert scanner._resolve_file_timeout(".PDF") == 45
    assert scanner._resolve_file_timeout(".txt") == 30


def test_extract_content_with_timeout_returns_auditable_timeout_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """超时控制入口应返回稳定 timeout 错误，而不是抛异常中断扫描。"""
    scanner = _make_scanner(tmp_path, monkeypatch, {"file_timeout_seconds": 12})
    sample = tmp_path / "slow.txt"
    sample.write_text("slow", encoding="utf-8")

    monkeypatch.setattr(
        scanner,
        "_run_extract_subprocess",
        lambda file_path, limits, timeout_seconds: (None, True),
    )

    context = scanner._extract_content_with_timeout(
        sample,
        {
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
        },
    )

    assert context.file_path == str(sample)
    assert context.file_type == ".txt"
    assert context.content == ""
    assert context.error == "timeout: file parse exceeded 12s"


def test_extract_content_with_timeout_uses_supervisor_extension_override(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """timeout 错误文案应通过 supervisor 统一格式化，并支持扩展名覆盖。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {
            "file_timeout_seconds": 30,
            "file_timeout_by_extension": {".pdf": 45},
        },
    )
    sample = tmp_path / "slow.pdf"
    sample.write_text("slow", encoding="utf-8")

    monkeypatch.setattr(
        scanner,
        "_run_extract_subprocess",
        lambda file_path, limits, timeout_seconds: (None, True),
    )

    context = scanner._extract_content_with_timeout(
        sample,
        {
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
        },
    )

    assert scanner.parser_supervisor.resolve_timeout(".pdf") == 45
    assert context.error == "timeout: file parse exceeded 45s"


def test_extract_content_with_timeout_returns_missing_result_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """子进程退出但未返回结果时，scanner 应通过 supervisor 返回稳定错误文本。"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    sample = tmp_path / "missing.txt"
    sample.write_text("missing", encoding="utf-8")

    monkeypatch.setattr(
        scanner,
        "_run_extract_subprocess",
        lambda file_path, limits, timeout_seconds: (None, False),
    )

    context = scanner._extract_content_with_timeout(
        sample,
        {
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
        },
    )

    assert context.error == "subprocess exited without result"


def test_run_extract_subprocess_returns_invalid_payload_error_via_supervisor(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
):
    """无效 payload 路径应保持稳定错误文本，并通过 supervisor 构造 fallback。"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    sample = tmp_path / "invalid.txt"
    sample.write_text("invalid", encoding="utf-8")

    class FakeQueue:
        def __init__(self, maxsize: int):
            self.maxsize = maxsize

        def get_nowait(self):
            return {"file_path": str(sample), "file_type": ".txt"}

    class FakeProcess:
        def __init__(self, target, args):
            self._alive = False

        def start(self):
            return None

        def join(self, timeout=None):
            return None

        def is_alive(self):
            return self._alive

        def terminate(self):
            self._alive = False

    class FakeContext:
        def Queue(self, maxsize: int):
            return FakeQueue(maxsize)

        def Process(self, target, args):
            return FakeProcess(target, args)

    monkeypatch.setattr(file_scanner_module.mp, "get_context", lambda mode: FakeContext())

    context, timed_out = scanner._run_extract_subprocess(
        sample,
        {"text_max_chars": 10},
        30,
    )

    assert timed_out is False
    assert context is not None
    assert context.error == "subprocess returned invalid payload"


def test_extract_content_with_timeout_runs_real_subprocess(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """真实子进程路径应能返回解析结果。"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    sample = tmp_path / "real.txt"
    sample.write_text("real subprocess content", encoding="utf-8")

    context = scanner._extract_content_with_timeout(
        sample,
        {
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
        },
    )

    assert context.error is None
    assert "real subprocess content" in context.content


def test_scan_files_records_timeout_and_continues(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """单文件超时应计入错误并继续处理其他文件。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".txt"], "worker_lane_mode": "subprocess"},
    )
    (scanner.work_dir / "fast.txt").write_text("fast", encoding="utf-8")
    (scanner.work_dir / "slow.txt").write_text("slow", encoding="utf-8")

    def fake_extract(file_path: Path, limits: dict) -> file_scanner_module.FileContext:
        if file_path.name == "slow.txt":
            return file_scanner_module.FileContext(
                file_path=str(file_path),
                file_type=".txt",
                content="",
                error="timeout: file parse exceeded 30s",
            )
        return file_scanner_module.FileContext(
            file_path=str(file_path),
            file_type=".txt",
            content="fast",
            error=None,
        )

    monkeypatch.setattr(scanner, "_extract_content_with_timeout", fake_extract)

    result = scanner.scan_files(date.today(), date.today())

    assert result.total_files == 2
    assert result.success_count == 1
    assert result.error_count == 1
    assert any(ctx.error and ctx.error.startswith("timeout:") for ctx in result.contexts)
    detail = scanner.scan_index_store.latest_scan_run_detail()
    assert detail["success_count"] == 1
    assert detail["error_count"] == 1
    assert detail["timeout_count"] == 1
    extension_metrics = scanner.scan_index_store.list_extension_metrics(detail["run_id"])
    assert len(extension_metrics) == 1
    assert extension_metrics[0].extension == ".txt"
    assert extension_metrics[0].file_count == 2
    assert extension_metrics[0].success_count == 1
    assert extension_metrics[0].error_count == 1
    assert extension_metrics[0].timeout_count == 1


def test_scan_files_delegates_bootstrap_discovery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """scan_files 应把启动阶段的文件发现委托给 FileDiscoveryService。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".txt"], "worker_lane_mode": "subprocess"},
    )
    expected = [scanner.work_dir / "delegated.txt"]
    expected[0].write_text("delegated", encoding="utf-8")
    calls: list[tuple[date, date]] = []

    def fake_bootstrap(
        self: FileDiscoveryService, start_date: date, end_date: date
    ) -> list[Path]:
        calls.append((start_date, end_date))
        return expected

    monkeypatch.setattr(
        FileDiscoveryService,
        "bootstrap_full_scan",
        fake_bootstrap,
    )
    monkeypatch.setattr(
        scanner,
        "_extract_content_with_timeout",
        lambda file_path, limits: file_scanner_module.FileContext(
            file_path=str(file_path),
            file_type=".txt",
            content="delegated",
            error=None,
        ),
    )

    result = scanner.scan_files(date.today(), date.today())

    assert calls == [(date.today(), date.today())]
    assert result.total_files == 1
    assert result.success_count == 1


def test_scan_files_counts_cached_and_uncached_contexts(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """存在 cached 与 uncached 候选时，两类上下文都应进入结果并落库指标。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".txt"], "worker_lane_mode": "subprocess"},
    )
    cached_file = scanner.work_dir / "cached.txt"
    uncached_file = scanner.work_dir / "uncached.txt"
    cached_file.write_text("cached", encoding="utf-8")
    uncached_file.write_text("uncached", encoding="utf-8")

    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: [cached_file, uncached_file],
    )
    monkeypatch.setattr(
        scanner.scan_planner,
        "plan_candidates",
        lambda candidates, start_date=None, end_date=None, cache_lookup=None: {
            "cached": [next(item for item in candidates if item.path == cached_file)],
            "uncached": [next(item for item in candidates if item.path == uncached_file)],
            "total_candidates": 2,
        },
    )
    monkeypatch.setattr(
        scanner,
        "_get_cached_contexts",
        lambda cached_files, parser_profile: [
            file_scanner_module.FileContext(
                file_path=str(cached_file),
                file_type=".txt",
                content="cached content",
                error=None,
            )
        ],
    )
    monkeypatch.setattr(
        scanner,
        "_extract_content_with_timeout",
        lambda file_path, limits: file_scanner_module.FileContext(
            file_path=str(file_path),
            file_type=".txt",
            content="uncached content",
            error=None,
        ),
    )

    result = scanner.scan_files(date.today(), date.today())

    assert result.total_files == 2
    assert result.success_count == 2
    assert result.error_count == 0
    assert [context.file_path for context in result.contexts] == [
        str(cached_file),
        str(uncached_file),
    ]
    assert scanner.scan_index_store.latest_scan_run() == {
        "discovered_count": 2,
        "reused_count": 1,
        "reparsed_count": 1,
    }
    detail = scanner.scan_index_store.latest_scan_run_detail()
    assert detail["success_count"] == 2
    assert detail["error_count"] == 0
    extension_metrics = scanner.scan_index_store.list_extension_metrics(detail["run_id"])
    assert len(extension_metrics) == 1
    assert extension_metrics[0].extension == ".txt"
    assert extension_metrics[0].file_count == 1
    assert extension_metrics[0].success_count == 1


def test_scan_files_emits_auditable_error_when_cached_context_missing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """缓存命中但未返回上下文时，不应静默漏算。"""
    scanner = _make_scanner(tmp_path, monkeypatch, {"allowed_extensions": [".txt"]})
    cached_file = scanner.work_dir / "cached.txt"
    cached_file.write_text("cached", encoding="utf-8")

    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: [cached_file],
    )
    monkeypatch.setattr(
        scanner.scan_planner,
        "plan_candidates",
        lambda candidates, start_date=None, end_date=None, cache_lookup=None: {
            "cached": [next(item for item in candidates if item.path == cached_file)],
            "uncached": [],
            "total_candidates": 1,
        },
    )
    monkeypatch.setattr(
        scanner,
        "_get_cached_contexts",
        lambda cached_files, parser_profile: [],
    )

    result = scanner.scan_files(date.today(), date.today())

    assert result.total_files == 1
    assert result.success_count == 0
    assert result.error_count == 1
    assert result.contexts[0].file_path == str(cached_file)
    assert "cache hit missing context" in (result.contexts[0].error or "")


def test_scan_files_reuses_fresh_parse_cache_without_parsing(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """fresh cache 命中时应直接复用缓存，不再调用解析器。"""
    scanner = _make_scanner(tmp_path, monkeypatch, {"allowed_extensions": [".txt"]})
    cached_file = scanner.work_dir / "cached.txt"
    cached_file.write_text("cached content", encoding="utf-8")
    source_version = "mtime_ns=1:size=14"
    discovered = [_build_discovered_file(cached_file, source_version)]
    parser_profile_key = scanner.scan_planner.serialize_parser_profile(
        scanner.scan_planner.build_parser_profile(summary_mode=False)
    )
    scanner.scan_index_store.upsert_parse_cache(
        file_identity=discovered[0].file_identity,
        parser_profile=parser_profile_key,
        content_excerpt="cached excerpt",
        parse_status="success",
        parse_error="",
        source_version=source_version,
    )

    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )

    parse_calls: list[Path] = []

    def fake_extract(file_path: Path, limits: dict) -> file_scanner_module.FileContext:
        parse_calls.append(file_path)
        return file_scanner_module.FileContext(
            file_path=str(file_path),
            file_type=".txt",
            content="parsed unexpectedly",
            error=None,
        )

    monkeypatch.setattr(scanner, "_extract_content_with_timeout", fake_extract)
    monkeypatch.setattr(
        scanner,
        "_get_cached_contexts",
        lambda cached_items, parser_profile: [
            file_scanner_module.FileContext(
                file_path=str(cached_file),
                file_type=".txt",
                content="cached excerpt",
                error=None,
            )
        ],
    )

    result = scanner.scan_files(date.today(), date.today())

    assert parse_calls == []
    assert result.total_files == 1
    assert result.success_count == 1
    assert result.error_count == 0
    assert result.contexts[0].content == "cached excerpt"


def test_scan_files_reparses_when_source_version_changes(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """source_version 变化后，即使 file_identity 不变也必须重解析。"""
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
        "_get_cached_contexts",
        lambda cached_items, parser_profile: [],
    )
    parse_calls: list[Path] = []

    def fake_extract(file_path: Path, limits: dict) -> file_scanner_module.FileContext:
        parse_calls.append(file_path)
        return file_scanner_module.FileContext(
            file_path=str(file_path),
            file_type=".txt",
            content="new parsed content",
            error=None,
        )

    monkeypatch.setattr(scanner, "_extract_content_with_timeout", fake_extract)

    result = scanner.scan_files(date.today(), date.today())

    assert parse_calls == [sample]
    assert result.total_files == 1
    assert result.success_count == 1
    assert result.contexts[0].content == "new parsed content"
    assert (
        scanner.scan_index_store.load_parse_cache(
            file_identity,
            parser_profile_key,
            source_version="mtime_ns=2:size=11",
        )["content_excerpt"]
        == "new parsed content"
    )


def test_get_files_in_range_still_returns_paths(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """兼容层 _get_files_in_range 对外仍应返回 Path 列表。"""
    scanner = _make_scanner(tmp_path, monkeypatch, {"allowed_extensions": [".txt"]})
    sample = scanner.work_dir / "report.txt"
    sample.write_text("hello", encoding="utf-8")
    discovered = [_build_discovered_file(sample, "mtime_ns=1:size=5")]
    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )

    files = scanner._get_files_in_range(date.today(), date.today())

    assert files == [sample]


def test_scan_files_writes_error_cache_when_parser_raises(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """解析器抛异常时应留下可复用的 error cache，而不是只记运行时日志。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".txt"], "worker_lane_mode": "subprocess"},
    )
    sample = scanner.work_dir / "broken.txt"
    sample.write_text("broken", encoding="utf-8")
    discovered = [_build_discovered_file(sample, "mtime_ns=3:size=6")]
    parser_profile_key = scanner.scan_planner.serialize_parser_profile(
        scanner.scan_planner.build_parser_profile(summary_mode=False)
    )

    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )

    def raising_extract(file_path: Path, limits: dict) -> file_scanner_module.FileContext:
        raise RuntimeError("boom")

    monkeypatch.setattr(scanner, "_extract_content_with_timeout", raising_extract)

    result = scanner.scan_files(date.today(), date.today())

    assert result.total_files == 1
    assert result.success_count == 0
    assert result.error_count == 1
    assert "boom" in (result.contexts[0].error or "")
    assert (
        scanner.scan_index_store.load_parse_cache(
            discovered[0].file_identity,
            parser_profile_key,
            source_version=discovered[0].source_version,
        )
        == {
            "content_excerpt": "",
            "parse_status": "error",
            "parse_error": "boom",
        }
    )


def test_scan_files_reparses_when_only_error_cache_exists_for_same_source_version(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """同版本只有 error cache 时，下一次扫描仍应重解析并用 success 覆盖。"""
    scanner = _make_scanner(
        tmp_path,
        monkeypatch,
        {"allowed_extensions": [".txt"], "worker_lane_mode": "subprocess"},
    )
    sample = scanner.work_dir / "retry.txt"
    sample.write_text("retry", encoding="utf-8")
    discovered = [_build_discovered_file(sample, "mtime_ns=4:size=5")]
    parser_profile_key = scanner.scan_planner.serialize_parser_profile(
        scanner.scan_planner.build_parser_profile(summary_mode=False)
    )

    monkeypatch.setattr(
        scanner.discovery_service,
        "bootstrap_full_scan",
        lambda start_date, end_date: discovered,
    )

    def raising_extract(file_path: Path, limits: dict) -> file_scanner_module.FileContext:
        raise RuntimeError("boom once")

    monkeypatch.setattr(scanner, "_extract_content_with_timeout", raising_extract)

    first_result = scanner.scan_files(date.today(), date.today())

    assert first_result.total_files == 1
    assert first_result.error_count == 1
    assert (
        scanner.scan_index_store.load_parse_cache(
            discovered[0].file_identity,
            parser_profile_key,
            source_version=discovered[0].source_version,
        )
        == {
            "content_excerpt": "",
            "parse_status": "error",
            "parse_error": "boom once",
        }
    )

    parse_calls: list[Path] = []

    def success_extract(file_path: Path, limits: dict) -> file_scanner_module.FileContext:
        parse_calls.append(file_path)
        return file_scanner_module.FileContext(
            file_path=str(file_path),
            file_type=".txt",
            content="parsed on retry",
            error=None,
        )

    monkeypatch.setattr(scanner, "_extract_content_with_timeout", success_extract)

    second_result = scanner.scan_files(date.today(), date.today())

    assert parse_calls == [sample]
    assert second_result.total_files == 1
    assert second_result.success_count == 1
    assert second_result.error_count == 0
    assert second_result.contexts[0].content == "parsed on retry"
    assert (
        scanner.scan_index_store.load_parse_cache(
            discovered[0].file_identity,
            parser_profile_key,
            source_version=discovered[0].source_version,
        )
        == {
            "content_excerpt": "parsed on retry",
            "parse_status": "success",
            "parse_error": "",
        }
    )


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
