"""Scanner 单文件 timeout 的纯归一化语义。"""

from __future__ import annotations

DEFAULT_FILE_TIMEOUT_SECONDS = 30.0


def normalize_file_timeout(value: object) -> tuple[float, bool]:
    """返回归一化秒数及原始值是否有效，不执行日志或其他 I/O。"""
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        return DEFAULT_FILE_TIMEOUT_SECONDS, False
    if parsed <= 0:
        return DEFAULT_FILE_TIMEOUT_SECONDS, False
    return parsed, True
