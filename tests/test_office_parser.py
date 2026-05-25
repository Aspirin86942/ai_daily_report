import json
from pathlib import Path
from types import SimpleNamespace

import pytest

from src.models.schemas import FileContext
from src.services.office_parser import (
    OFFICE_RUST_FILE_TYPES,
    PYTHON_OFFICE_BACKEND,
    PYTHON_SHAREPOINT_TEXT_BACKEND,
    RUST_OFFICE_BACKEND,
    OfficeParseAudit,
    RustOfficeParserRunner,
    parse_office_with_fallback,
    parse_with_sharepoint_text,
)


def test_office_rust_file_types_include_legacy_office_extensions():
    assert OFFICE_RUST_FILE_TYPES == {
        ".docx",
        ".xlsx",
        ".pptx",
        ".doc",
        ".xls",
        ".ppt",
    }


def test_rust_runner_returns_file_context_from_valid_payload(tmp_path, monkeypatch):
    sample = tmp_path / "report.xlsx"
    sample.write_bytes(b"fake")

    completed = SimpleNamespace(
        returncode=0,
        stdout=(
            '{"file_path":"'
            + str(sample)
            + '","file_type":".xlsx","content":"ok","error":null,'
            '"parser_backend":"rust_office_oxide_v1","truncated":false}'
        ),
        stderr="",
    )

    def fake_run(*args, **kwargs):
        assert str(args[0][0]).endswith("ai-daily-office-parser")
        assert kwargs["timeout"] == 12.0
        assert '"path": "' in kwargs["input"]
        assert '"file_type": ".xlsx"' in kwargs["input"]
        return completed

    monkeypatch.setattr("src.services.office_parser.subprocess.run", fake_run)

    context, duration_ms = RustOfficeParserRunner(
        "rust/office_parser/target/release/ai-daily-office-parser"
    ).parse(sample, ".xlsx", {"document_excerpt_max_chars": 6000}, 12)

    assert context == FileContext(
        file_path=str(sample),
        file_type=".xlsx",
        content="ok",
        error=None,
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )
    assert duration_ms >= 0


def test_rust_runner_returns_error_context_for_invalid_json(tmp_path, monkeypatch):
    sample = tmp_path / "bad.docx"
    sample.write_bytes(b"fake")
    completed = SimpleNamespace(returncode=0, stdout="not-json", stderr="")
    monkeypatch.setattr(
        "src.services.office_parser.subprocess.run",
        lambda *args, **kwargs: completed,
    )

    context, _ = RustOfficeParserRunner("parser").parse(
        sample,
        ".docx",
        {"document_excerpt_max_chars": 6000},
        12,
    )

    assert context.content == ""
    assert context.error is not None
    assert context.error.startswith("RUST_OFFICE_INVALID_JSON:")
    assert context.parser_backend == RUST_OFFICE_BACKEND


@pytest.mark.parametrize(
    ("payload_override", "expected_detail"),
    [
        ({"file_path": "/tmp/other.xlsx"}, "file_path mismatch"),
        ({"file_type": ".docx"}, "file_type mismatch"),
        ({"parser_backend": PYTHON_OFFICE_BACKEND}, "parser_backend mismatch"),
    ],
)
def test_rust_runner_returns_error_context_for_semantic_payload_mismatch(
    tmp_path,
    monkeypatch,
    payload_override,
    expected_detail,
):
    sample = tmp_path / "report.xlsx"
    sample.write_bytes(b"fake")
    payload = {
        "file_path": str(sample),
        "file_type": ".xlsx",
        "content": "ok",
        "error": None,
        "parser_backend": RUST_OFFICE_BACKEND,
        "truncated": False,
    }
    payload.update(payload_override)
    completed = SimpleNamespace(
        returncode=0,
        stdout=json.dumps(payload),
        stderr="",
    )
    monkeypatch.setattr(
        "src.services.office_parser.subprocess.run",
        lambda *args, **kwargs: completed,
    )

    context, _ = RustOfficeParserRunner("parser").parse(
        sample,
        ".xlsx",
        {"document_excerpt_max_chars": 6000},
        12,
    )

    assert context.content == ""
    assert context.error is not None
    assert context.error.startswith("RUST_OFFICE_INVALID_PAYLOAD:")
    assert expected_detail in context.error
    assert context.parser_backend == RUST_OFFICE_BACKEND


def test_rust_runner_returns_timeout_context(tmp_path, monkeypatch):
    sample = tmp_path / "slow.pptx"
    sample.write_bytes(b"fake")

    def fake_run(*args, **kwargs):
        raise TimeoutError("expired")

    monkeypatch.setattr("src.services.office_parser.subprocess.run", fake_run)

    context, _ = RustOfficeParserRunner("parser").parse(
        sample,
        ".pptx",
        {"document_excerpt_max_chars": 6000},
        9,
    )

    assert context.error == "RUST_OFFICE_TIMEOUT: file parse exceeded 9s"
    assert context.parser_backend == RUST_OFFICE_BACKEND


def test_parse_office_with_fallback_uses_python_when_rust_fails(tmp_path):
    sample = tmp_path / "report.docx"
    sample.write_bytes(b"fake")
    rust_context = FileContext(
        file_path=str(sample),
        file_type=".docx",
        content="",
        error="RUST_OFFICE_PARSE_FAILED: bad zip",
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )

    class FakeRunner:
        def parse(self, file_path, file_type, limits, timeout_seconds):
            return rust_context, 5

    def fake_python_fallback(file_path, file_type, limits):
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="python fallback",
            error=None,
            parser_backend="python_office_v1",
            truncated=False,
        )

    outcome = parse_office_with_fallback(
        file_path=sample,
        file_type=".docx",
        limits={"document_excerpt_max_chars": 6000},
        scanner_cfg={
            "office_parser_backend": RUST_OFFICE_BACKEND,
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": ["python_office_v1"],
            "office_fallback_after_timeout": False,
        },
        timeout_seconds=12,
        rust_runner=FakeRunner(),
        python_fallback=fake_python_fallback,
    )

    assert outcome.context.content == "python fallback"
    assert outcome.audit == OfficeParseAudit(
        attempted_backend=RUST_OFFICE_BACKEND,
        fallback_backend="python_office_v1",
        fallback_reason="RUST_OFFICE_PARSE_FAILED: bad zip",
        rust_duration_ms=5,
        fallback_duration_ms=0,
    )


def test_parse_office_with_fallback_does_not_fallback_after_timeout_by_default(tmp_path):
    sample = tmp_path / "slow.xlsx"
    sample.write_bytes(b"fake")
    rust_context = FileContext(
        file_path=str(sample),
        file_type=".xlsx",
        content="",
        error="RUST_OFFICE_TIMEOUT: file parse exceeded 3s",
        parser_backend=RUST_OFFICE_BACKEND,
        truncated=False,
    )

    class FakeRunner:
        def parse(self, file_path, file_type, limits, timeout_seconds):
            return rust_context, 3

    outcome = parse_office_with_fallback(
        file_path=sample,
        file_type=".xlsx",
        limits={},
        scanner_cfg={
            "office_parser_backend": RUST_OFFICE_BACKEND,
            "office_parser_fallback_enabled": True,
            "office_parser_fallback_order": ["python_office_v1"],
            "office_fallback_after_timeout": False,
        },
        timeout_seconds=3,
        rust_runner=FakeRunner(),
        python_fallback=lambda file_path, file_type, limits: pytest.fail(
            "fallback should not run"
        ),
    )

    assert outcome.context.error == "RUST_OFFICE_TIMEOUT: file parse exceeded 3s"
    assert outcome.audit.fallback_backend == ""


def test_parse_office_with_sharepoint_backend_uses_sharepoint_for_docx(
    tmp_path,
    monkeypatch,
):
    sample = tmp_path / "report.docx"
    sample.write_bytes(b"fake")
    calls = []

    def fake_sharepoint(file_path, file_type, limits):
        calls.append((file_path, file_type, limits))
        return FileContext(
            file_path=str(file_path),
            file_type=file_type,
            content="sharepoint text",
            error=None,
            parser_backend=PYTHON_SHAREPOINT_TEXT_BACKEND,
            truncated=False,
        )

    monkeypatch.setattr(
        "src.services.office_parser.parse_with_sharepoint_text",
        fake_sharepoint,
    )

    outcome = parse_office_with_fallback(
        file_path=sample,
        file_type=".docx",
        limits={"document_excerpt_max_chars": 6000},
        scanner_cfg={
            "office_parser_backend": PYTHON_SHAREPOINT_TEXT_BACKEND,
            "office_parser_fallback_order": [PYTHON_OFFICE_BACKEND],
        },
        timeout_seconds=12,
    )

    assert outcome.context.content == "sharepoint text"
    assert outcome.context.parser_backend == PYTHON_SHAREPOINT_TEXT_BACKEND
    assert outcome.audit.attempted_backend == PYTHON_SHAREPOINT_TEXT_BACKEND
    assert calls == [(sample, ".docx", {"document_excerpt_max_chars": 6000})]


def test_parse_with_sharepoint_text_reports_missing_dependency(tmp_path):
    sample = tmp_path / "legacy.doc"
    sample.write_bytes(b"fake")

    def fake_import(name):
        raise ModuleNotFoundError(name)

    context = parse_with_sharepoint_text(
        sample,
        ".doc",
        {"document_excerpt_max_chars": 6000},
        import_module=fake_import,
    )

    assert context.content == ""
    assert context.error == "PYTHON_SHAREPOINT_TEXT_UNAVAILABLE: sharepoint2text"
    assert context.parser_backend == "python_sharepoint_text_v1"
