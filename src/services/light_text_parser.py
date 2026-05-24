"""轻量 text-like 文件解析器。"""

from __future__ import annotations

import csv
import io
import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from ..models.schemas import FileContext


LIGHT_TEXT_PARSER_BACKEND = "light_text_v1"
DEFAULT_DIRECT_TEXT_READ_BYTES = 256 * 1024
DEFAULT_LOG_TAIL_READ_BYTES = 256 * 1024

LOG_FILE_TYPES = {".log"}
JSON_FILE_TYPES = {".json"}
CSV_FILE_TYPES = {".csv"}


@dataclass(frozen=True, slots=True)
class LightTextParserOptions:
    read_head_bytes: int = DEFAULT_DIRECT_TEXT_READ_BYTES
    read_tail_bytes: int = DEFAULT_LOG_TAIL_READ_BYTES
    max_output_chars: int = 6000
    encoding: str = "utf-8"
    parser_backend_version: str = LIGHT_TEXT_PARSER_BACKEND


@dataclass(frozen=True, slots=True)
class _RawExcerpt:
    text: str
    source: str
    truncated: bool


def parse_text_like_file(
    file_path: Path,
    file_type: str,
    limits: dict[str, Any],
    options: LightTextParserOptions,
) -> FileContext:
    """解析轻量 text-like 文件，避免为普通文本引入重依赖。"""
    normalized_type = file_type.lower()
    try:
        raw_excerpt = _read_bounded_excerpt(file_path, normalized_type, options)
    except UnicodeDecodeError as exc:
        return _build_context(
            file_path=file_path,
            file_type=normalized_type,
            options=options,
            content="",
            error=f"TEXT_DECODE_FAILED: {exc}",
            truncated=False,
        )
    except OSError as exc:
        return _build_context(
            file_path=file_path,
            file_type=normalized_type,
            options=options,
            content="",
            error=f"TEXT_READ_FAILED: {exc}",
            truncated=False,
        )

    max_output_chars = _resolve_max_output_chars(limits, options)
    if normalized_type in JSON_FILE_TYPES:
        content, truncated = _build_json_content(raw_excerpt, max_output_chars)
    elif normalized_type in CSV_FILE_TYPES:
        content, truncated = _build_csv_content(raw_excerpt, max_output_chars)
    else:
        content, truncated = _build_text_content(
            raw_excerpt,
            title="Text preview",
            max_output_chars=max_output_chars,
        )

    return _build_context(
        file_path=file_path,
        file_type=normalized_type,
        options=options,
        content=content,
        error=None,
        truncated=truncated,
    )


def _build_context(
    file_path: Path,
    file_type: str,
    options: LightTextParserOptions,
    content: str,
    error: str | None,
    truncated: bool,
) -> FileContext:
    return FileContext(
        file_path=str(file_path),
        file_type=file_type,
        content=content,
        error=error,
        parser_backend=options.parser_backend_version,
        truncated=truncated,
    )


def _read_bounded_excerpt(
    file_path: Path,
    file_type: str,
    options: LightTextParserOptions,
) -> _RawExcerpt:
    file_size = file_path.stat().st_size
    if file_type in LOG_FILE_TYPES:
        read_bytes = _coerce_non_negative_int(options.read_tail_bytes)
        start_offset = max(file_size - read_bytes, 0)
        with file_path.open("rb") as file:
            file.seek(start_offset)
            raw = file.read(read_bytes)
        truncated = start_offset > 0
        text = _decode_bounded(
            raw,
            options.encoding,
            trim_leading_fragment=truncated,
            trim_trailing_fragment=False,
        )
        return _RawExcerpt(text=text, source="tail", truncated=truncated)

    read_bytes = _coerce_non_negative_int(options.read_head_bytes)
    with file_path.open("rb") as file:
        raw = file.read(read_bytes)
    truncated = file_size > len(raw)
    text = _decode_bounded(
        raw,
        options.encoding,
        trim_leading_fragment=False,
        trim_trailing_fragment=truncated,
    )
    return _RawExcerpt(text=text, source="head", truncated=truncated)


def _decode_bounded(
    raw: bytes,
    encoding: str,
    *,
    trim_leading_fragment: bool,
    trim_trailing_fragment: bool,
) -> str:
    """严格解码，只裁剪读取边界切断的 UTF-8 半字符。"""
    encoding_name = encoding.lower().replace("_", "-")
    if encoding_name == "utf-8" and trim_leading_fragment:
        # tail 读取可能从 UTF-8 continuation byte 开始；只丢弃边界残片。
        raw = _trim_leading_utf8_fragment(raw)

    try:
        return raw.decode(encoding)
    except UnicodeDecodeError as exc:
        if (
            encoding_name == "utf-8"
            and trim_trailing_fragment
            and exc.end == len(raw)
            and exc.reason == "unexpected end of data"
        ):
            # head 读取可能截断多字节字符尾部；真实非法字节仍会继续抛错。
            return raw[: exc.start].decode(encoding)
        raise


def _trim_leading_utf8_fragment(raw: bytes) -> bytes:
    trimmed = 0
    while trimmed < len(raw) and trimmed < 3 and 0x80 <= raw[trimmed] <= 0xBF:
        trimmed += 1
    return raw[trimmed:]


def _build_json_content(
    raw_excerpt: _RawExcerpt,
    max_output_chars: int,
) -> tuple[str, bool]:
    if raw_excerpt.truncated:
        return _build_text_content(
            raw_excerpt,
            title="JSON preview",
            max_output_chars=max_output_chars,
            warning="JSON_PREVIEW_FALLBACK",
        )

    try:
        payload = json.loads(raw_excerpt.text)
    except json.JSONDecodeError:
        return _build_text_content(
            raw_excerpt,
            title="JSON preview",
            max_output_chars=max_output_chars,
            warning="JSON_PREVIEW_FALLBACK",
        )

    def builder(truncated: bool) -> str:
        lines = _metadata_lines(
            "JSON object preview" if isinstance(payload, dict) else "JSON preview",
            raw_excerpt.source,
            truncated,
        )
        if isinstance(payload, dict):
            keys = ", ".join(sorted(str(key) for key in payload.keys()))
            lines.append(f"top_level_keys: {keys}")
        elif isinstance(payload, list):
            lines.append(f"top_level_type: list")
            lines.append(f"top_level_items: {len(payload)}")
        else:
            lines.append(f"top_level_type: {type(payload).__name__}")
        lines.extend(["", raw_excerpt.text])
        return "\n".join(lines)

    return _finalize_content(builder, raw_excerpt.truncated, max_output_chars)


def _build_csv_content(
    raw_excerpt: _RawExcerpt,
    max_output_chars: int,
) -> tuple[str, bool]:
    try:
        rows = list(csv.reader(io.StringIO(raw_excerpt.text), strict=True))
    except csv.Error:
        return _build_text_content(
            raw_excerpt,
            title="CSV preview",
            max_output_chars=max_output_chars,
            warning="CSV_PREVIEW_FALLBACK",
        )

    def builder(truncated: bool) -> str:
        lines = _metadata_lines("CSV preview", raw_excerpt.source, truncated)
        if rows:
            lines.append(f"header: {_format_csv_row(rows[0])}")
            for index, row in enumerate(rows[1:4], start=1):
                lines.append(f"row {index}: {_format_csv_row(row)}")
        else:
            lines.append("header: ")
        return "\n".join(lines)

    return _finalize_content(builder, raw_excerpt.truncated, max_output_chars)


def _build_text_content(
    raw_excerpt: _RawExcerpt,
    *,
    title: str,
    max_output_chars: int,
    warning: str | None = None,
) -> tuple[str, bool]:
    def builder(truncated: bool) -> str:
        lines = _metadata_lines(title, raw_excerpt.source, truncated)
        if warning:
            lines.append(f"warning: {warning}")
        lines.extend(["", raw_excerpt.text])
        return "\n".join(lines)

    return _finalize_content(builder, raw_excerpt.truncated, max_output_chars)


def _metadata_lines(title: str, excerpt_source: str, truncated: bool) -> list[str]:
    return [
        title,
        f"excerpt_source: {excerpt_source}",
        f"truncated: {str(truncated).lower()}",
    ]


def _format_csv_row(row: list[str]) -> str:
    return " | ".join(cell.strip() for cell in row)


def _finalize_content(
    builder: Callable[[bool], str],
    read_truncated: bool,
    max_output_chars: int,
) -> tuple[str, bool]:
    content = builder(read_truncated)
    limited_content, output_truncated = _limit_content(content, max_output_chars)
    final_truncated = read_truncated or output_truncated
    if final_truncated != read_truncated:
        content = builder(final_truncated)
        limited_content, output_truncated = _limit_content(content, max_output_chars)
        final_truncated = read_truncated or output_truncated
    return limited_content, final_truncated


def _limit_content(content: str, max_output_chars: int) -> tuple[str, bool]:
    max_chars = _coerce_non_negative_int(max_output_chars)
    if len(content) <= max_chars:
        return content, False
    if max_chars == 0:
        return "", True
    return content[:max_chars], True


def _resolve_max_output_chars(
    limits: dict[str, Any],
    options: LightTextParserOptions,
) -> int:
    candidates = [_coerce_positive_int(options.max_output_chars, 6000)]
    for key in ("text_excerpt_max_chars", "text_max_chars"):
        if key in limits:
            candidates.append(_coerce_positive_int(limits[key], candidates[0]))
    return min(candidates)


def _coerce_non_negative_int(value: Any) -> int:
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        return 0
    return max(parsed, 0)


def _coerce_positive_int(value: Any, default: int) -> int:
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        return default
    return parsed if parsed > 0 else default
