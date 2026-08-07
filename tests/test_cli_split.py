"""src/cli 拆分后的行为等价性测试。"""

from __future__ import annotations

import pytest

from src.cli.parser import build_parser


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
