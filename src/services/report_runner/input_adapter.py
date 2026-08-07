"""Daily 交互输入 adapter：source gate 后才调用。"""

from __future__ import annotations

from typing import Protocol

from rich.console import Console


class DailyInputAdapter(Protocol):
    def read(self) -> str:
        ...


class ConsoleDailyInputAdapter:
    """沿用现有 CLI 的多行输入语义。"""

    def __init__(self, console: Console | None = None) -> None:
        self._console = console or Console()

    def read(self) -> str:
        self._console.print("\n[bold cyan]请描述今日工作内容:[/bold cyan]")
        self._console.print(
            "[dim](输入完成后按 Ctrl+Z (Windows) 或 Ctrl+D (Linux/Mac) 结束)[/dim]\n"
        )
        lines: list[str] = []
        try:
            while True:
                lines.append(input())
        except EOFError:
            pass
        return "\n".join(lines).strip()
