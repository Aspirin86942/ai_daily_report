"""轻量 text-like 文件解析器。"""

from __future__ import annotations

import csv
import io
import json
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ..models.schemas import FileContext


LIGHT_TEXT_PARSER_BACKEND = "light_text_v1"
DEFAULT_TEXT_MAX_CHARS = 6000
DEFAULT_SUMMARY_TEXT_MAX_CHARS = 2000
DEFAULT_DIRECT_TEXT_READ_BYTES = 256 * 1024
DEFAULT_LOG_TAIL_READ_BYTES = 256 * 1024

LOG_FILE_TYPES = {".log"}
JSON_FILE_TYPES = {".json"}
CSV_FILE_TYPES = {".csv"}
InvalidPositiveIntReporter = Callable[[str, Any, int, str], None]


@dataclass(frozen=True, slots=True)
class LightTextBudget:
    text_max_chars: int
    direct_text_read_bytes: int
    log_tail_read_bytes: int
    text_excerpt_max_chars: int


@dataclass(frozen=True, slots=True)
class LightTextParserOptions:
    read_head_bytes: int = DEFAULT_DIRECT_TEXT_READ_BYTES
    read_tail_bytes: int = DEFAULT_LOG_TAIL_READ_BYTES
    max_output_chars: int = 6000
    encoding: str = "utf-8"
    parser_backend_version: str = LIGHT_TEXT_PARSER_BACKEND


def build_light_text_budget(
    config: Mapping[str, Any],
    *,
    text_max_chars: Any,
    default_text_max_chars: int = DEFAULT_TEXT_MAX_CHARS,
    on_invalid: InvalidPositiveIntReporter | None = None,
) -> LightTextBudget:
    """统一归一化 light parser 预算，保证 profile 与运行时 cache key 一致。"""
    effective_text_max_chars = normalize_positive_int(
        text_max_chars,
        default_text_max_chars,
        key="text_max_chars",
        on_invalid=on_invalid,
    )
    direct_read_default = _normalize_config_positive_int(
        config,
        "direct_text_max_bytes",
        DEFAULT_DIRECT_TEXT_READ_BYTES,
        on_invalid,
    )
    return LightTextBudget(
        text_max_chars=effective_text_max_chars,
        direct_text_read_bytes=_normalize_config_positive_int(
            config,
            "direct_text_read_bytes",
            direct_read_default,
            on_invalid,
        ),
        log_tail_read_bytes=_normalize_config_positive_int(
            config,
            "log_tail_read_bytes",
            DEFAULT_LOG_TAIL_READ_BYTES,
            on_invalid,
        ),
        text_excerpt_max_chars=_normalize_config_positive_int(
            config,
            "text_excerpt_max_chars",
            effective_text_max_chars,
            on_invalid,
        ),
    )


def normalize_positive_int(
    value: Any,
    default: int,
    *,
    key: str | None = None,
    on_invalid: InvalidPositiveIntReporter | None = None,
) -> int:
    """把任意输入归一化为正整数；非法值使用调用方传入的默认值。"""
    try:
        parsed = int(value)
    except (TypeError, ValueError):
        if key and on_invalid:
            on_invalid(key, value, default, "invalid")
        return default

    if parsed <= 0:
        if key and on_invalid:
            on_invalid(key, value, default, "non_positive")
        return default
    return parsed


def _normalize_config_positive_int(
    config: Mapping[str, Any],
    key: str,
    default: int,
    on_invalid: InvalidPositiveIntReporter | None,
) -> int:
    raw_value = config.get(key, default)
    reporter_key = key if key in config else None
    return normalize_positive_int(
        raw_value,
        default,
        key=reporter_key,
        on_invalid=on_invalid,
    )


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
            context_offset = max(start_offset - 3, 0)
            file.seek(context_offset)
            prefix_and_raw = file.read(start_offset - context_offset + read_bytes)
        leading_context_length = start_offset - context_offset
        leading_context = prefix_and_raw[:leading_context_length]
        raw = prefix_and_raw[leading_context_length:]
        truncated = start_offset > 0
        text = _decode_bounded(
            raw,
            options.encoding,
            trim_leading_fragment=truncated,
            trim_trailing_fragment=False,
            leading_context=leading_context,
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
    leading_context: bytes = b"",
) -> str:
    """严格解码，只裁剪读取边界切断的 UTF-8 半字符。"""
    encoding_name = encoding.lower().replace("_", "-")
    if encoding_name == "utf-8" and trim_leading_fragment:
        # tail 读取可能从 UTF-8 continuation byte 开始；先用窗口前字节证明它确实是跨边界字符。
        raw = _trim_leading_utf8_fragment(raw, leading_context)

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


def _trim_leading_utf8_fragment(raw: bytes, leading_context: bytes) -> bytes:
    if not raw or not _is_utf8_continuation(raw[0]):
        return raw

    boundary_index = len(leading_context)
    combined = leading_context + raw[:3]
    for lead_index in range(max(0, boundary_index - 3), boundary_index):
        sequence_length = _utf8_sequence_length(combined[lead_index])
        if sequence_length == 0:
            continue

        sequence_end = lead_index + sequence_length
        if not lead_index < boundary_index < sequence_end:
            continue
        if sequence_end > len(combined):
            continue

        sequence = combined[lead_index:sequence_end]
        try:
            sequence.decode("utf-8")
        except UnicodeDecodeError:
            continue

        # 只有完整跨边界字符可证明时，才丢弃窗口内属于该字符的残片。
        raw_fragment_length = sequence_end - boundary_index
        return raw[raw_fragment_length:]

    return raw


def _is_utf8_continuation(value: int) -> bool:
    return 0x80 <= value <= 0xBF


def _utf8_sequence_length(first_byte: int) -> int:
    if 0xC2 <= first_byte <= 0xDF:
        return 2
    if 0xE0 <= first_byte <= 0xEF:
        return 3
    if 0xF0 <= first_byte <= 0xF4:
        return 4
    return 0


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
            lines.append("top_level_type: list")
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
    if raw_excerpt.truncated:
        return _build_text_content(
            raw_excerpt,
            title="Text preview",
            max_output_chars=max_output_chars,
            warning="CSV_PREVIEW_FALLBACK",
        )

    try:
        rows = list(csv.reader(io.StringIO(raw_excerpt.text), strict=True))
    except csv.Error:
        return _build_text_content(
            raw_excerpt,
            title="Text preview",
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
    return normalize_positive_int(value, default)
