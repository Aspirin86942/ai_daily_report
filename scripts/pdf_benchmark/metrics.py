"""PDF parser gate 两侧共用的质量度量。"""

from __future__ import annotations

import unicodedata
from difflib import SequenceMatcher


def normalize_for_fidelity(text: str) -> str:
    """忽略版面换行/空格，只比较归一化后的实际字符。"""
    return unicodedata.normalize("NFC", "".join(text.split()))


def printable_ratio(text: str) -> float:
    """计算可显示字符与正常布局空白所占比例。"""
    if not text:
        return 0.0
    accepted = sum(
        character.isprintable() or character in "\r\n\t"
        for character in text
    )
    return accepted / len(text)


def ground_truth_ratio(text: str, ground_truth: str) -> float:
    """计算忽略布局空白后的字符保真率。"""
    return SequenceMatcher(
        None,
        normalize_for_fidelity(text),
        normalize_for_fidelity(ground_truth),
    ).ratio()
