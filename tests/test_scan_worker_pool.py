"""测试 ParserSupervisor 最小契约。"""

from pathlib import Path

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
