"""src/cli 拆分后的行为等价性测试。"""

from __future__ import annotations

import pytest

from src.cli.parser import build_parser
from src.cli.doctor import run_doctor_cmd
from src.cli.list_reports import list_reports


@pytest.mark.parametrize(
    ("argv", "subcommand"),
    [
        (["daily", "-i", "x"], "daily"),
        (["daily", "--no-save"], "daily"),
        (["weekly", "2026-W05", "--source", "scan"], "weekly"),
        (["monthly", "2026-01", "--source", "db"], "monthly"),
        (["list"], "list"),
        (["doctor"], "doctor"),
        (["check-config"], "doctor"),
    ],
)
def test_parser_accepts_equivalent_commands(
    argv: list[str], subcommand: str
) -> None:
    assert build_parser().parse_args(argv).subcommand == subcommand


def test_parser_weekly_requires_source() -> None:
    with pytest.raises(SystemExit):
        build_parser().parse_args(["weekly"])


def test_parser_preserves_daily_flags() -> None:
    args = build_parser().parse_args(
        ["daily", "--no-save", "--date", "2026-02-05"]
    )

    assert args.no_save is True
    assert args.date == "2026-02-05"


def test_list_reports_shows_empty_hint() -> None:
    printed: list[str] = []
    console = type(
        "Console",
        (),
        {"print": lambda self, *args, **kwargs: printed.append(args[0])},
    )()
    store = type("Store", (), {"list_all_reports": lambda self: []})()

    list_reports(console, store=store)

    assert any("暂无日报数据" in text for text in printed)


def test_run_doctor_cmd_passes_strict() -> None:
    printed: list[str] = []
    console = type(
        "Console",
        (),
        {"print": lambda self, *args, **kwargs: printed.append(args[0])},
    )()
    seen: list[bool] = []

    def collect(*, strict: bool = False):
        seen.append(strict)
        return type(
            "Result",
            (),
            {
                "info": {"LLM Provider": "deepseek"},
                "warnings": ["w"],
                "errors": [],
            },
        )()

    ok = run_doctor_cmd(console=console, collect=collect, strict=True)

    assert ok is True
    assert seen == [True]
    assert any("所有检查通过" in text for text in printed)
