"""测试轻量 text-like 解析器。"""

from pathlib import Path

from src.services.light_text_parser import (
    LIGHT_TEXT_PARSER_BACKEND,
    LightTextParserOptions,
    parse_text_like_file,
)


def _options(**overrides) -> LightTextParserOptions:
    values = {
        "read_head_bytes": 64,
        "read_tail_bytes": 64,
        "max_output_chars": 200,
        "encoding": "utf-8",
        "parser_backend_version": LIGHT_TEXT_PARSER_BACKEND,
    }
    values.update(overrides)
    return LightTextParserOptions(**values)


def test_parse_markdown_reads_bounded_head_and_marks_truncated(tmp_path: Path):
    sample = tmp_path / "large.md"
    sample.write_text("# Title\n\nfirst paragraph\n\nsecond paragraph", encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".md",
        {"text_max_chars": 200},
        _options(read_head_bytes=18),
    )

    assert context.error is None
    assert context.parser_backend == "light_text_v1"
    assert context.truncated is True
    assert "truncated: true" in context.content
    assert "# Title" in context.content
    assert "second" not in context.content


def test_parse_log_reads_tail_excerpt(tmp_path: Path):
    sample = tmp_path / "app.log"
    sample.write_text("old line\nmiddle line\nlatest line", encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".log",
        {"text_max_chars": 200},
        _options(read_tail_bytes=16),
    )

    assert context.error is None
    assert context.truncated is True
    assert "excerpt_source: tail" in context.content
    assert "latest line" in context.content
    assert "old line" not in context.content


def test_parse_json_outputs_top_level_keys(tmp_path: Path):
    sample = tmp_path / "payload.json"
    sample.write_text('{"name": "demo", "items": [1, 2]}', encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".json",
        {"text_max_chars": 200},
        _options(read_head_bytes=256),
    )

    assert context.error is None
    assert "JSON object preview" in context.content
    assert "top_level_keys: items, name" in context.content


def test_parse_truncated_json_falls_back_to_text_excerpt(tmp_path: Path):
    sample = tmp_path / "payload.json"
    sample.write_text('{"name": "demo", "items": [1, 2]}', encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".json",
        {"text_max_chars": 200},
        _options(read_head_bytes=10),
    )

    assert context.error is None
    assert context.truncated is True
    assert "warning: JSON_PREVIEW_FALLBACK" in context.content
    assert '{"name"' in context.content


def test_parse_valid_json_prefix_with_trailing_bytes_falls_back(tmp_path: Path):
    sample = tmp_path / "payload.json"
    valid_prefix = '{"a": 1}'
    sample.write_text(f"{valid_prefix} trailing bytes", encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".json",
        {"text_max_chars": 200},
        _options(read_head_bytes=len(valid_prefix)),
    )

    assert context.error is None
    assert context.truncated is True
    assert "warning: JSON_PREVIEW_FALLBACK" in context.content
    assert "JSON object preview" not in context.content


def test_parse_csv_outputs_header_and_preview_rows(tmp_path: Path):
    sample = tmp_path / "table.csv"
    sample.write_text("name,amount\nalpha,10\nbeta,20\n", encoding="utf-8")

    context = parse_text_like_file(
        sample,
        ".csv",
        {"text_max_chars": 200},
        _options(read_head_bytes=256),
    )

    assert context.error is None
    assert "CSV preview" in context.content
    assert "header: name | amount" in context.content
    assert "row 1: alpha | 10" in context.content


def test_parse_decode_failure_returns_auditable_error(tmp_path: Path):
    sample = tmp_path / "bad.txt"
    sample.write_bytes(b"\xff\xfe\xfa")

    context = parse_text_like_file(
        sample,
        ".txt",
        {"text_max_chars": 200},
        _options(read_head_bytes=256),
    )

    assert context.content == ""
    assert context.parser_backend == "light_text_v1"
    assert context.error is not None
    assert context.error.startswith("TEXT_DECODE_FAILED:")
