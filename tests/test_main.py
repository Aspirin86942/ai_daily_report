"""CLI 入口的 request 映射、呈现和退出码测试。"""

from argparse import Namespace
from datetime import date as real_date
import os
from pathlib import Path
import subprocess
import sys
from typing import Literal

import pytest

import main
from src.core.healthcheck import HealthCheckResult
from src.models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
from src.services.report_runner.outcomes import (
    DatabaseEvidence,
    ErrorCode,
    PublicationReceipt,
    ReportError,
    ReportRunFailure,
    ReportRunSuccess,
    ScanEvidence,
)
from src.services.report_runner.period import ResolvedPeriod
from src.services.report_runner.requests import (
    DailyReportRunRequest,
    MonthlyReportRunRequest,
    WeeklyReportRunRequest,
)

ReportMode = Literal["daily", "weekly", "monthly"]
ReportSource = Literal["db", "scan"]


def _patch_console(monkeypatch) -> list[str]:
    printed: list[str] = []
    monkeypatch.setattr(
        main.console,
        "print",
        lambda *args, **kwargs: printed.append(args[0] if args else ""),
    )
    return printed


def _report_success(
    mode: ReportMode,
    source: ReportSource,
    markdown: str,
) -> ReportRunSuccess:
    as_of_date = real_date(2026, 5, 25)
    if mode == "daily":
        report = DailyReportData(
            date="2026-05-25",
            completed_work="完成日报",
            work_summary="日报摘要",
            next_plan="后续计划",
        )
        start_date = real_date(2026, 5, 24)
        end_date = as_of_date
        display_label = "2026-05-25"
    elif mode == "weekly":
        report = WeeklyReportData(
            week_label="2026-W20",
            date_range="2026-05-11 ~ 2026-05-17",
            completed_work="完成周报",
            self_growth="",
            improvement_actions="",
            work_summary="",
            next_plan="",
            support_needed="",
            other_notes="",
        )
        start_date = real_date(2026, 5, 11)
        end_date = real_date(2026, 5, 17)
        display_label = "2026-W20"
    else:
        report = MonthlyReportData(
            year_month="2026-05",
            overview="月报概览",
            completed_work="完成月报",
            work_summary="月报总结",
            next_plan="下月计划",
        )
        start_date = real_date(2026, 5, 1)
        end_date = real_date(2026, 5, 31)
        display_label = "2026-05"

    evidence = (
        ScanEvidence(
            status="ok",
            source_file_count=2,
            success_count=2,
            scan_run_id=1,
            context_run_id=1,
        )
        if source == "scan"
        else DatabaseEvidence(report_count=1, missing_days=[])
    )
    return ReportRunSuccess(
        mode=mode,
        source=source,
        status="ok",
        period=ResolvedPeriod(
            mode=mode,
            source=source,
            start_date=start_date,
            end_date=end_date,
            display_label=display_label,
            as_of_date=as_of_date,
        ),
        report=report,
        markdown=markdown,
        warnings=[],
        source_evidence=evidence,
        publication=PublicationReceipt(
            requested=False,
            sqlite_state="not_attempted",
            markdown_state="not_attempted",
        ),
    )


def _generation_failure(
    mode: ReportMode,
    source: ReportSource,
) -> ReportRunFailure:
    success = _report_success(mode, source, markdown="")
    return ReportRunFailure(
        mode=mode,
        source=source,
        period=success.period,
        phase="generation",
        error=ReportError(
            error_code=ErrorCode.LLM_GENERATION_FAILED,
            message="LLM unavailable",
            retryable=False,
        ),
        warnings=[],
        source_evidence=success.source_evidence,
        publication=PublicationReceipt(
            requested=False,
            sqlite_state="not_attempted",
            markdown_state="not_attempted",
        ),
    )


def test_daily_command_maps_namespace_to_report_runner_request(monkeypatch):
    captured: list[object] = []

    class FixedDate(real_date):
        @classmethod
        def today(cls) -> real_date:
            return real_date(2026, 5, 25)

    class StubRunner:
        def run(self, request):
            captured.append(request)
            return _report_success("daily", "scan", "# 日报")

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(main, "date", FixedDate)
    monkeypatch.setattr(main, "_build_report_runner", lambda: StubRunner())
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    success = main.generate_daily_report(
        Namespace(input="今天工作", no_save=True, date="2026-05-20")
    )

    assert success is True
    request = captured[0]
    assert isinstance(request, DailyReportRunRequest)
    assert request.as_of_date == real_date(2026, 5, 25)
    assert request.user_input == "今天工作"
    assert request.save is False
    assert request.report_date_override == "2026-05-20"
    assert any("日报预览" in text for text in printed)


def test_weekly_command_maps_namespace_to_report_runner_request(monkeypatch):
    captured: list[object] = []

    class FixedDate(real_date):
        @classmethod
        def today(cls) -> real_date:
            return real_date(2026, 5, 25)

    class StubRunner:
        def run(self, request):
            captured.append(request)
            return _report_success("weekly", "db", "# 周报")

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(main, "date", FixedDate)
    monkeypatch.setattr(main, "_build_report_runner", lambda: StubRunner())
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    success = main.generate_weekly_report_cmd(
        Namespace(
            week="2026-W20",
            source="db",
            input="周补充",
            no_save=True,
        )
    )

    assert success is True
    request = captured[0]
    assert isinstance(request, WeeklyReportRunRequest)
    assert request.as_of_date == real_date(2026, 5, 25)
    assert request.source == "db"
    assert request.week_label == "2026-W20"
    assert request.supplemental_input == "周补充"
    assert request.save is False
    assert any("周报预览" in text for text in printed)


def test_monthly_command_maps_namespace_to_report_runner_request(monkeypatch):
    captured: list[object] = []

    class FixedDate(real_date):
        @classmethod
        def today(cls) -> real_date:
            return real_date(2026, 5, 25)

    class StubRunner:
        def run(self, request):
            captured.append(request)
            return _report_success("monthly", "scan", "# 月报")

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(main, "date", FixedDate)
    monkeypatch.setattr(main, "_build_report_runner", lambda: StubRunner())
    monkeypatch.setattr(main, "Markdown", lambda text: text)

    success = main.generate_monthly_report_cmd(
        Namespace(
            month="2026-05",
            source="scan",
            input="月补充",
            no_save=False,
        )
    )

    assert success is True
    request = captured[0]
    assert isinstance(request, MonthlyReportRunRequest)
    assert request.as_of_date == real_date(2026, 5, 25)
    assert request.source == "scan"
    assert request.year_month == "2026-05"
    assert request.supplemental_input == "月补充"
    assert request.save is True
    assert any("月报预览" in text for text in printed)


def test_list_reports_uses_sqlite_store(monkeypatch):
    calls: dict[str, int] = {"init": 0, "list_all_reports": 0}

    class StubSQLiteStore:
        def __init__(self) -> None:
            calls["init"] += 1

        def list_all_reports(self) -> list[str]:
            calls["list_all_reports"] += 1
            return []

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(main, "SQLiteStore", StubSQLiteStore)

    main.list_reports()

    assert calls == {"init": 1, "list_all_reports": 1}
    assert any("已有日报列表" in text for text in printed)
    assert any("暂无日报数据" in text for text in printed)


def test_build_parser_accepts_report_doctor_and_alias_commands():
    parser = main.build_parser()

    daily_args = parser.parse_args(["daily", "--input", "work", "--no-save"])
    weekly_args = parser.parse_args(["weekly", "2026-W20", "--source", "db"])
    monthly_args = parser.parse_args(["monthly", "2026-05", "--source", "scan"])
    doctor_args = parser.parse_args(["doctor"])
    strict_args = parser.parse_args(["doctor", "--strict"])
    alias_args = parser.parse_args(["check-config"])

    assert daily_args.subcommand == "daily"
    assert daily_args.input == "work"
    assert daily_args.no_save is True
    assert weekly_args.week == "2026-W20"
    assert weekly_args.source == "db"
    assert monthly_args.month == "2026-05"
    assert monthly_args.source == "scan"
    assert doctor_args.subcommand == "doctor"
    assert doctor_args.strict is False
    assert strict_args.strict is True
    assert alias_args.subcommand == "doctor"
    assert alias_args.strict is False


def test_run_doctor_cmd_uses_healthcheck(monkeypatch):
    printed = _patch_console(monkeypatch)
    strict_values: list[bool] = []

    def collect(*, strict: bool = False) -> HealthCheckResult:
        strict_values.append(strict)
        return HealthCheckResult(
            info={
                "LLM Provider": "deepseek",
                "工作目录": str(Path("D:/work")),
                "LLM 模型": "deepseek-chat",
                "最大并发": "4",
                "SQLite DB": "data/db/reports.sqlite3",
                "API Key": "sk-test-12...",
            },
            warnings=["缺少敏感配置文件: config/.secrets.yaml"],
            errors=[],
        )

    monkeypatch.setattr(main, "collect_healthcheck", collect)

    success = main.run_doctor_cmd(strict=True)

    assert success is True
    assert strict_values == [True]
    assert any("环境检查" in str(text) for text in printed)
    assert any("LLM Provider" in str(text) for text in printed)
    assert any("警告" in str(text) for text in printed)
    assert any("所有检查通过" in str(text) for text in printed)


@pytest.mark.parametrize(
    ("command", "args", "mode", "source"),
    [
        (
            main.generate_daily_report,
            Namespace(input="work", no_save=True, date=None),
            "daily",
            "scan",
        ),
        (
            main.generate_weekly_report_cmd,
            Namespace(
                week="2026-W01",
                source="scan",
                input=None,
                no_save=True,
            ),
            "weekly",
            "scan",
        ),
        (
            main.generate_monthly_report_cmd,
            Namespace(
                month="2026-01",
                source="scan",
                input=None,
                no_save=True,
            ),
            "monthly",
            "scan",
        ),
    ],
    ids=["daily", "weekly", "monthly"],
)
def test_report_command_presents_typed_generation_failure(
    monkeypatch,
    command,
    args: Namespace,
    mode: ReportMode,
    source: ReportSource,
):
    class StubRunner:
        def run(self, request):
            return _generation_failure(mode, source)

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(main, "_build_report_runner", lambda: StubRunner())

    success = command(args)

    assert success is False
    assert any("生成失败: LLM unavailable" in text for text in printed)


@pytest.mark.parametrize(
    ("handler_name", "argv"),
    [
        ("generate_daily_report", ["daily", "--input", "work"]),
        (
            "generate_weekly_report_cmd",
            ["weekly", "2026-W01", "--source", "db"],
        ),
        (
            "generate_monthly_report_cmd",
            ["monthly", "2026-01", "--source", "db"],
        ),
    ],
)
def test_main_returns_one_when_report_command_fails(
    monkeypatch,
    handler_name: str,
    argv: list[str],
):
    monkeypatch.setattr(main, handler_name, lambda args: False)
    monkeypatch.setattr(sys, "argv", ["main.py", *argv])

    assert main.main() == 1


@pytest.mark.parametrize(
    ("handler_name", "argv"),
    [
        ("generate_daily_report", ["daily", "--input", "work"]),
        (
            "generate_weekly_report_cmd",
            ["weekly", "2026-W01", "--source", "db"],
        ),
        (
            "generate_monthly_report_cmd",
            ["monthly", "2026-01", "--source", "db"],
        ),
    ],
)
def test_main_returns_zero_when_report_command_succeeds(
    monkeypatch,
    handler_name: str,
    argv: list[str],
):
    monkeypatch.setattr(main, handler_name, lambda args: True)
    monkeypatch.setattr(sys, "argv", ["main.py", *argv])

    assert main.main() == 0


def test_main_returns_doctor_status(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["main.py", "doctor"])
    monkeypatch.setattr(main, "run_doctor_cmd", lambda *, strict=False: False)
    assert main.main() == 1

    monkeypatch.setattr(main, "run_doctor_cmd", lambda *, strict=False: True)
    assert main.main() == 0


def test_main_returns_zero_without_subcommand(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["main.py"])

    assert main.main() == 0


def test_main_returns_one_for_unexpected_exception(monkeypatch):
    def fail() -> None:
        raise RuntimeError("unexpected failure")

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(sys, "argv", ["main.py", "list"])
    monkeypatch.setattr(main, "list_reports", fail)

    assert main.main() == 1
    assert any("unexpected failure" in text for text in printed)


def test_main_returns_130_for_keyboard_interrupt(monkeypatch):
    def interrupt() -> None:
        raise KeyboardInterrupt

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(sys, "argv", ["main.py", "list"])
    monkeypatch.setattr(main, "list_reports", interrupt)

    assert main.main() == 130
    assert any("操作已取消" in text for text in printed)


def test_cli_help_exits_zero():
    completed = subprocess.run(
        [sys.executable, str(Path(main.__file__).resolve()), "--help"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )

    assert completed.returncode == 0
    assert "usage:" in completed.stdout


def _run_main_with_sitecustomize(
    tmp_path: Path,
    source: str,
    *arguments: str,
) -> subprocess.CompletedProcess[str]:
    """在隔离启动钩子下运行真实 CLI 进程。"""
    (tmp_path / "sitecustomize.py").write_text(source, encoding="utf-8")
    env = os.environ.copy()
    env["PYTHONPATH"] = os.pathsep.join(
        filter(None, [str(tmp_path), env.get("PYTHONPATH", "")])
    )
    return subprocess.run(
        [sys.executable, str(Path(main.__file__).resolve()), *arguments],
        capture_output=True,
        text=True,
        encoding="utf-8",
        env=env,
        check=False,
    )


def test_cli_doctor_bootstraps_without_rich_or_file_logger(tmp_path):
    """doctor 必须在完整 UI/业务依赖导入前进入轻量诊断路径。"""
    completed = _run_main_with_sitecustomize(
        tmp_path,
        "\n".join(
            [
                "import builtins",
                "import sys",
                "import types",
                "real_import = builtins.__import__",
                "def guarded_import(name, *args, **kwargs):",
                "    if name == 'rich' or name.startswith('rich.') or name == 'src.core.logger':",
                "        raise RuntimeError(f'forbidden eager import: {name}')",
                "    return real_import(name, *args, **kwargs)",
                "builtins.__import__ = guarded_import",
                "healthcheck = types.ModuleType('src.core.healthcheck')",
                "class Result:",
                "    info = {'轻量入口': '可用'}",
                "    warnings = []",
                "    errors = []",
                "healthcheck.collect_healthcheck = lambda: Result()",
                "sys.modules['src.core.healthcheck'] = healthcheck",
            ]
        ),
        "doctor",
    )

    assert completed.returncode == 0
    assert "轻量入口: 可用" in completed.stdout
    assert "forbidden eager import" not in completed.stdout + completed.stderr


def test_cli_strict_doctor_uses_the_lightweight_entrypoint(tmp_path):
    completed = _run_main_with_sitecustomize(
        tmp_path,
        "\n".join(
            [
                "import sys",
                "import types",
                "healthcheck = types.ModuleType('src.core.healthcheck')",
                "class Result:",
                "    warnings = []",
                "    errors = []",
                "def collect_healthcheck(*, strict=False):",
                "    result = Result()",
                "    result.info = {'严格模式': str(strict)}",
                "    return result",
                "healthcheck.collect_healthcheck = collect_healthcheck",
                "sys.modules['src.core.healthcheck'] = healthcheck",
            ]
        ),
        "doctor",
        "--strict",
    )

    assert completed.returncode == 0
    assert "严格模式: True" in completed.stdout


def test_cli_doctor_redacts_bootstrap_exception_text(tmp_path):
    """启动期异常可能含 YAML 原文，doctor 不得直接回显。"""
    fake_secret = "dummy-secret-must-not-leak"
    completed = _run_main_with_sitecustomize(
        tmp_path,
        "\n".join(
            [
                "import sys",
                "import types",
                "healthcheck = types.ModuleType('src.core.healthcheck')",
                "def fail():",
                f"    raise ValueError('{fake_secret}')",
                "healthcheck.collect_healthcheck = fail",
                "sys.modules['src.core.healthcheck'] = healthcheck",
            ]
        ),
        "doctor",
    )

    combined_output = completed.stdout + completed.stderr
    assert completed.returncode == 1
    assert "doctor 无法启动 (ValueError)" in combined_output
    assert fake_secret not in combined_output


def test_cli_invalid_week_exits_nonzero():
    completed = subprocess.run(
        [
            sys.executable,
            str(Path(main.__file__).resolve()),
            "weekly",
            "invalid-week",
            "--source",
            "db",
        ],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )

    assert completed.returncode == 1
    assert "错误:" in completed.stdout
