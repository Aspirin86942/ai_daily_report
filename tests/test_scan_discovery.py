"""测试文件发现边界。"""

import json
from datetime import date, datetime
from pathlib import Path
from types import SimpleNamespace

import pytest

from src.services.scan_discovery import DiscoveredFile, FileDiscoveryService


def test_bootstrap_full_scan_filters_extensions_patterns_and_excluded_dirs(
    tmp_path: Path,
):
    """文件发现服务应保留现有扩展名、忽略模式和排除目录行为。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    included_dir = work_dir / "included"
    included_dir.mkdir()
    excluded_dir = work_dir / "excluded"
    excluded_dir.mkdir()

    (included_dir / "keep.MD").write_text("keep", encoding="utf-8")
    (included_dir / "~$draft.md").write_text("ignore", encoding="utf-8")
    (included_dir / "scratch.tmp").write_text("ignore", encoding="utf-8")
    (excluded_dir / "blocked.md").write_text("blocked", encoding="utf-8")

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md", ".tmp"],
            "ignored_patterns": ["~$*", "*.tmp"],
            "excluded_dirs": [str(excluded_dir)],
            "discovery_backend": "python",
        },
    )

    files = discovery.bootstrap_full_scan(date.today(), date.today())

    assert [item.path.relative_to(work_dir).as_posix() for item in files] == [
        "included/keep.MD"
    ]


def test_bootstrap_full_scan_skips_files_outside_date_range(tmp_path: Path):
    """文件发现服务应按修改时间范围过滤文件。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    (work_dir / "recent.md").write_text("recent", encoding="utf-8")

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "python",
        },
    )

    files = discovery.bootstrap_full_scan(date(2000, 1, 1), date(2000, 1, 2))

    assert files == []


def test_bootstrap_full_scan_rejects_missing_work_dir_before_backend(
    tmp_path: Path,
    monkeypatch,
):
    """工作目录不可达时必须显式失败，不能伪装成空扫描。"""
    missing_work_dir = tmp_path / "missing"
    calls = []

    def fake_run(*args, **kwargs):
        calls.append((args, kwargs))
        return SimpleNamespace(returncode=0, stdout="[]", stderr="")

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=missing_work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "rust",
            "rust_discovery_bin": "target/release/ai-daily-discovery",
        },
    )

    with pytest.raises(FileNotFoundError, match="work_dir does not exist"):
        discovery.bootstrap_full_scan(date.today(), date.today())

    assert calls == []


@pytest.mark.parametrize("excluded_dirs", [None, ""])
def test_bootstrap_full_scan_treats_empty_excluded_dirs_as_empty_list(
    tmp_path: Path,
    excluded_dirs,
):
    """discovery 边界应容忍空排除目录，不能把 None 或空字符串传成有效路径。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    sample = work_dir / "report.md"
    sample.write_text("hello", encoding="utf-8")

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": excluded_dirs,
            "discovery_backend": "python",
        },
    )

    [item] = discovery.bootstrap_full_scan(date.today(), date.today())

    assert item.path == sample


def test_bootstrap_full_scan_returns_discovered_file_metadata(tmp_path: Path):
    """启动发现应返回可写入库存表的文件元数据。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    sample = work_dir / "report.TXT"
    sample.write_text("hello", encoding="utf-8")

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".txt"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "python",
        },
    )

    [item] = discovery.bootstrap_full_scan(date.today(), date.today())

    assert isinstance(item, DiscoveredFile)
    assert item.path == sample
    assert item.file_identity.startswith("bootstrap:")
    assert item.file_identity == f"bootstrap:{str(sample.resolve()).lower()}"
    assert item.extension == ".txt"
    assert item.size_bytes == sample.stat().st_size
    assert item.source_version == (
        f"mtime_ns={sample.stat().st_mtime_ns}:size={sample.stat().st_size}"
    )


def test_bootstrap_full_scan_uses_rust_backend_when_configured(
    tmp_path: Path,
    monkeypatch,
):
    """Rust backend 成功时，应把 stdout JSON 转成现有 DiscoveredFile 契约。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    sample = work_dir / "report.md"
    sample.write_text("hello", encoding="utf-8")
    stat_result = sample.stat()
    payload = [
        {
            "file_identity": f"bootstrap:{str(sample.resolve()).lower()}",
            "path": str(sample.resolve()),
            "extension": ".md",
            "modified_at": datetime.fromtimestamp(stat_result.st_mtime).isoformat(),
            "size_bytes": stat_result.st_size,
            "source_version": (
                f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
            ),
        }
    ]
    calls = []

    def fake_run(*args, **kwargs):
        calls.append((args, kwargs))
        assert kwargs["encoding"] == "utf-8"
        assert kwargs["errors"] == "strict"
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps(payload),
            stderr="",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "rust",
            "rust_discovery_bin": "target/release/ai-daily-discovery",
            "discovery_timeout_seconds": 5,
        },
    )

    [item] = discovery.bootstrap_full_scan(date.today(), date.today())

    assert calls
    request = json.loads(calls[0][1]["input"])
    assert request["work_dir"] == str(work_dir)
    assert request["start_date"] == date.today().isoformat()
    assert request["end_date"] == date.today().isoformat()
    assert request["allowed_extensions"] == [".md"]
    assert request["ignored_patterns"] == []
    assert request["excluded_dirs"] == []
    assert item.path == sample.resolve()
    assert item.extension == ".md"
    assert item.source_version == (
        f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
    )


def test_bootstrap_full_scan_defaults_to_rust_backend(
    tmp_path: Path,
    monkeypatch,
):
    """未显式配置 backend 时，discovery 服务也应优先尝试 Rust。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    payload = []
    calls = []

    def fake_run(*args, **kwargs):
        calls.append((args, kwargs))
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps(payload),
            stderr="",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "rust_discovery_bin": "target/release/ai-daily-discovery",
            "discovery_timeout_seconds": 5,
        },
    )

    files = discovery.bootstrap_full_scan(date.today(), date.today())

    assert files == []
    assert calls


def test_bootstrap_full_scan_falls_back_to_python_when_rust_fails(
    tmp_path: Path,
    monkeypatch,
):
    """Rust 进程失败不能中断扫描；fallback 应保持现有 Python discovery 行为。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    sample = work_dir / "fallback.md"
    sample.write_text("fallback", encoding="utf-8")

    def fake_run(*args, **kwargs):
        return SimpleNamespace(
            returncode=2,
            stdout="",
            stderr="boom",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "rust",
            "rust_discovery_bin": "target/release/ai-daily-discovery",
            "discovery_timeout_seconds": 5,
        },
    )

    [item] = discovery.bootstrap_full_scan(date.today(), date.today())

    assert item.path == sample
    assert item.file_identity == f"bootstrap:{str(sample.resolve()).lower()}"


def test_bootstrap_full_scan_falls_back_when_rust_json_contract_is_invalid(
    tmp_path: Path,
    monkeypatch,
):
    """Rust stdout 合约错误要在 Python 边界拦住，避免坏数据进入 inventory。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    sample = work_dir / "fallback.md"
    sample.write_text("fallback", encoding="utf-8")

    def fake_run(*args, **kwargs):
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps([{"path": str(sample)}]),
            stderr="",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "rust",
            "rust_discovery_bin": "target/release/ai-daily-discovery",
            "discovery_timeout_seconds": 5,
        },
    )

    [item] = discovery.bootstrap_full_scan(date.today(), date.today())

    assert item.path == sample


def test_bootstrap_full_scan_rejects_unsupported_discovery_backend(
    tmp_path: Path,
    monkeypatch,
):
    """backend 拼写错误必须显式失败，不能静默退回 Python discovery。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    (work_dir / "fallback.md").write_text("fallback", encoding="utf-8")
    calls = []

    def fake_run(*args, **kwargs):
        calls.append((args, kwargs))
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps([]),
            stderr="",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "rsut",
            "rust_discovery_bin": "target/release/ai-daily-discovery",
        },
    )

    with pytest.raises(ValueError, match="Unsupported discovery_backend: rsut"):
        discovery.bootstrap_full_scan(date.today(), date.today())

    assert calls == []


def test_bootstrap_full_scan_rejects_unnormalized_discovery_backend(
    tmp_path: Path,
    monkeypatch,
):
    """service 边界只接受归一化后的 backend 字面量。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    (work_dir / "fallback.md").write_text("fallback", encoding="utf-8")
    calls = []

    def fake_run(*args, **kwargs):
        calls.append((args, kwargs))
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps([]),
            stderr="",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "Rust",
            "rust_discovery_bin": "target/release/ai-daily-discovery",
        },
    )

    with pytest.raises(ValueError, match="Unsupported discovery_backend: Rust"):
        discovery.bootstrap_full_scan(date.today(), date.today())

    assert calls == []


@pytest.mark.parametrize(
    "bad_item",
    [
        None,
        {
            "file_identity": None,
            "path": "__PATH__",
            "extension": ".md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": 999,
            "source_version": "mtime_ns=1:size=999",
        },
        {
            "file_identity": "__IDENTITY__",
            "path": "",
            "extension": ".md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": 999,
            "source_version": "mtime_ns=1:size=999",
        },
        {
            "file_identity": "__IDENTITY__",
            "path": "__PATH__",
            "extension": "md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": 999,
            "source_version": "mtime_ns=1:size=999",
        },
        {
            "file_identity": "__IDENTITY__",
            "path": "__PATH__",
            "extension": ".md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": True,
            "source_version": "mtime_ns=1:size=True",
        },
        {
            "file_identity": "__IDENTITY__",
            "path": "__PATH__",
            "extension": ".md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": 999,
            "source_version": "mtime_ns=:size=999",
        },
        {
            "file_identity": "__IDENTITY__",
            "path": "__PATH__",
            "extension": ".md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": 999,
            "source_version": "mtime_ns=abc:size=999",
        },
        {
            "file_identity": "__IDENTITY__",
            "path": "__PATH__",
            "extension": ".md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": 999,
            "source_version": "mtime_ns=1:size=-1",
        },
        {
            "file_identity": "__IDENTITY__",
            "path": "__PATH__",
            "extension": ".md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": 999,
            "source_version": "mtime_ns=1:size=998",
        },
        {
            "file_identity": "__IDENTITY__",
            "path": "__PATH__",
            "extension": ".md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": 1,
            "source_version": "mtime_ns=²:size=1",
        },
        {
            "file_identity": "__IDENTITY__",
            "path": "__PATH__",
            "extension": ".md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": 1,
            "source_version": "mtime_ns=١:size=1",
        },
        {
            "file_identity": "__IDENTITY__",
            "path": "__PATH__",
            "extension": ".md",
            "modified_at": "__MODIFIED_AT__",
            "size_bytes": 4,
            "source_version": "mtime_ns=4:size=４",
        },
    ],
)
def test_bootstrap_full_scan_falls_back_when_rust_item_contract_is_invalid(
    tmp_path: Path,
    monkeypatch,
    bad_item,
):
    """Rust 输出不能靠 str/int 宽松转换修正，坏合约必须触发 Python fallback。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    sample = work_dir / "fallback.md"
    sample.write_text("fallback", encoding="utf-8")
    stat_result = sample.stat()

    if isinstance(bad_item, dict):
        bad_item = {
            key: str(sample.resolve()) if value == "__PATH__" else value
            for key, value in bad_item.items()
        }
        bad_item = {
            key: f"bootstrap:{str(sample.resolve()).lower()}"
            if value == "__IDENTITY__"
            else value
            for key, value in bad_item.items()
        }
        bad_item = {
            key: datetime.fromtimestamp(stat_result.st_mtime).isoformat()
            if value == "__MODIFIED_AT__"
            else value
            for key, value in bad_item.items()
        }

    def fake_run(*args, **kwargs):
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps([bad_item]),
            stderr="",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "rust",
            "rust_discovery_bin": "target/release/ai-daily-discovery",
            "discovery_timeout_seconds": 5,
        },
    )

    [item] = discovery.bootstrap_full_scan(date.today(), date.today())

    assert item.path == sample
    assert item.file_identity == f"bootstrap:{str(sample.resolve()).lower()}"
    assert item.extension == ".md"
    assert item.size_bytes == stat_result.st_size
    assert item.source_version == (
        f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
    )
