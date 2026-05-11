"""测试 ParserSupervisor 最小契约。"""

from pathlib import Path

import pytest

from src.models.schemas import FileContext
from src.services.scan_worker_pool import ParserSupervisor


def test_parser_supervisor_uses_direct_parse_path(tmp_path: Path):
    """parse_file 应复用 direct_parse 结果并包装为成功 FileContext。"""
    sample = tmp_path / "notes.txt"
    sample.write_text("hello supervisor", encoding="utf-8")
    supervisor = ParserSupervisor(
        file_timeout_seconds=30,
        file_timeout_by_extension={},
    )
    captured_limits: list[dict] = []

    def direct_parse(file_path: Path, limits: dict) -> str:
        captured_limits.append(limits)
        assert file_path == sample
        return "parsed content"

    context = supervisor.parse_file(
        file_path=sample,
        file_type=".txt",
        limits={"text_max_chars": 100},
        direct_parse=direct_parse,
    )

    assert captured_limits == [{"text_max_chars": 100}]
    assert context == FileContext(
        file_path=str(sample),
        file_type=".txt",
        content="parsed content",
        error=None,
    )


def test_parser_supervisor_returns_timeout_error_for_extension_override():
    """timeout fallback 文案应使用扩展名覆盖后的秒数。"""
    supervisor = ParserSupervisor(
        file_timeout_seconds=30,
        file_timeout_by_extension={".pdf": 12},
    )

    context = supervisor.handle_worker_timeout(
        file_path=Path("report.pdf"),
        file_type=".pdf",
    )

    assert context == FileContext(
        file_path="report.pdf",
        file_type=".pdf",
        content="",
        error="timeout: file parse exceeded 12s",
    )


@pytest.mark.parametrize(
    ("file_timeout_seconds", "overrides", "file_type", "expected_raw"),
    [
        ("bad", {}, ".txt", "bad"),
        (30, {".pdf": 0}, ".pdf", 0),
        (30, {".pdf": "abc"}, ".pdf", "abc"),
    ],
)
def test_parser_supervisor_logs_warning_for_invalid_timeout_config(
    caplog: pytest.LogCaptureFixture,
    file_timeout_seconds: object,
    overrides: dict[str, object],
    file_type: str,
    expected_raw: object,
):
    """非法 timeout 配置回退默认值时必须记录 warning。"""
    supervisor = ParserSupervisor(
        file_timeout_seconds=file_timeout_seconds,
        file_timeout_by_extension=overrides,
    )

    with caplog.at_level("WARNING"):
        timeout = supervisor.resolve_timeout(file_type)

    assert timeout == 30.0
    assert any(
        str(expected_raw) in record.message and file_type in record.message
        for record in caplog.records
    )


def test_parser_supervisor_builds_missing_result_fallback():
    """缺少子进程结果时应返回稳定的可审计错误。"""
    supervisor = ParserSupervisor(
        file_timeout_seconds=30,
        file_timeout_by_extension={},
    )

    context = supervisor.handle_missing_result(
        file_path=Path("report.txt"),
        file_type=".txt",
    )

    assert context == FileContext(
        file_path="report.txt",
        file_type=".txt",
        content="",
        error="subprocess exited without result",
    )


def test_parser_supervisor_builds_invalid_payload_fallback():
    """无效 payload 时应返回稳定的可审计错误。"""
    supervisor = ParserSupervisor(
        file_timeout_seconds=30,
        file_timeout_by_extension={},
    )

    context = supervisor.handle_invalid_payload(
        file_path=Path("report.txt"),
        file_type=".txt",
    )

    assert context == FileContext(
        file_path="report.txt",
        file_type=".txt",
        content="",
        error="subprocess returned invalid payload",
    )
