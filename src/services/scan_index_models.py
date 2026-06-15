"""Typed models shared by scan index storage helpers."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date
from pathlib import Path


@dataclass(slots=True)
class InventoryItem:
    """库存查询返回的 typed 文件元数据。"""

    file_identity: str
    path: Path
    extension: str
    modified_date: date
    size_bytes: int
    source_version: str


@dataclass(frozen=True, slots=True)
class CacheProbe:
    """解释一次 parse cache freshness 判断结果。"""

    file_identity: str
    parser_profile: str
    source_version: str
    cache_status: str
    cache_miss_reason: str
    previous_source_version: str | None = None
