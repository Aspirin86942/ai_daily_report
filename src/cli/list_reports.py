"""list 子命令：列出已有日报。"""

from __future__ import annotations

from collections import defaultdict
from typing import Protocol


class _Console(Protocol):
    def print(self, *objects: object, **kwargs: object) -> None:
        ...


class _ReportListStore(Protocol):
    def list_all_reports(self) -> list[str]:
        ...


def list_reports(console: _Console, *, store: _ReportListStore) -> None:
    """按月倒序列出已有日报。"""
    console.print("\n[bold green]===== 已有日报列表 =====[/bold green]\n")
    dates = store.list_all_reports()
    if not dates:
        console.print("[yellow]暂无日报数据[/yellow]")
        return

    by_month: dict[str, list[str]] = defaultdict(list)
    for date_str in dates:
        by_month[date_str[:7]].append(date_str)

    for month in sorted(by_month, reverse=True):
        console.print(f"[bold cyan]{month}[/bold cyan]")
        for date_str in sorted(by_month[month], reverse=True):
            console.print(f"  - {date_str}")
        console.print()

    console.print(f"[green]共 {len(dates)} 份日报[/green]")
