"""测试文件扫描器"""

from datetime import date, timedelta
from pathlib import Path
from types import SimpleNamespace

import pytest

import src.services.file_scanner as file_scanner_module
from src.services.file_scanner import FileScanner
from src.services.scan_discovery import FileDiscoveryService


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
    }
    if scanner_overrides:
        scanner_cfg.update(scanner_overrides)

    fake_config = SimpleNamespace(scanner_config=scanner_cfg, work_dir=work_dir)
    monkeypatch.setattr(file_scanner_module, "config", fake_config)
    return FileScanner()


def test_file_scanner_init(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    """测试扫描器初始化"""
    scanner = _make_scanner(tmp_path, monkeypatch)
    assert scanner.work_dir.exists()
    assert scanner.scanner_cfg["max_workers"] > 0


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
    scanner = _make_scanner(tmp_path, monkeypatch, {"allowed_extensions": [".txt"]})
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


def test_scan_files_delegates_bootstrap_discovery(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
):
    """scan_files 应把启动阶段的文件发现委托给 FileDiscoveryService。"""
    scanner = _make_scanner(tmp_path, monkeypatch, {"allowed_extensions": [".txt"]})
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
    """存在 cached 与 uncached 候选时，两类上下文都应进入结果。"""
    scanner = _make_scanner(tmp_path, monkeypatch, {"allowed_extensions": [".txt"]})
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
        lambda candidates: {
            "cached": [cached_file],
            "uncached": [uncached_file],
            "total_candidates": 2,
        },
    )
    monkeypatch.setattr(
        scanner,
        "_get_cached_contexts",
        lambda cached_files: [
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
        lambda candidates: {
            "cached": [cached_file],
            "uncached": [],
            "total_candidates": 1,
        },
    )
    monkeypatch.setattr(scanner, "_get_cached_contexts", lambda cached_files: [])

    result = scanner.scan_files(date.today(), date.today())

    assert result.total_files == 1
    assert result.success_count == 0
    assert result.error_count == 1
    assert result.contexts[0].file_path == str(cached_file)
    assert "cache hit missing context" in (result.contexts[0].error or "")
