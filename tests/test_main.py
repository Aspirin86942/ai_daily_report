"""main.py 薄入口的分派、退出码与 bootstrap 测试。"""

from argparse import Namespace
import os
from pathlib import Path
import subprocess
import sys

import pytest
import rich.console as rich_console_module

import main
from src.cli import daily as daily_cli
from src.cli import doctor as doctor_cli
from src.cli import list_reports as list_cli
from src.cli import monthly as monthly_cli
from src.cli import weekly as weekly_cli
from src.core.healthcheck import HealthCheckResult
from src.core import healthcheck as healthcheck_module
from src.services import sqlite_store as sqlite_store_module


def _patch_console(monkeypatch) -> list[str]:
    printed: list[str] = []

    class StubConsole:
        def print(self, *args, **kwargs) -> None:
            printed.append(args[0] if args else "")

    monkeypatch.setattr(rich_console_module, "Console", StubConsole)
    return printed


def test_main_dispatches_list_with_sqlite_store(monkeypatch):
    calls: dict[str, int] = {"init": 0, "list_all_reports": 0}

    class StubSQLiteStore:
        def __init__(self) -> None:
            calls["init"] += 1

        def list_all_reports(self) -> list[str]:
            calls["list_all_reports"] += 1
            return []

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(sqlite_store_module, "SQLiteStore", StubSQLiteStore)
    monkeypatch.setattr(sys, "argv", ["main.py", "list"])

    status = main.main()

    assert status == 0
    assert calls == {"init": 1, "list_all_reports": 1}
    assert any("已有日报列表" in text for text in printed)
    assert any("暂无日报数据" in text for text in printed)


def test_main_module_does_not_export_command_handlers() -> None:
    assert not hasattr(main, "generate_daily_report")
    assert not hasattr(main, "SQLiteStore")
    assert not hasattr(main, "Console")


def test_main_dispatches_strict_doctor(monkeypatch):
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

    monkeypatch.setattr(healthcheck_module, "collect_healthcheck", collect)
    monkeypatch.setattr(sys, "argv", ["main.py", "doctor", "--strict"])

    status = main.main()

    assert status == 0
    assert strict_values == [True]
    assert any("环境检查" in str(text) for text in printed)
    assert any("所有检查通过" in str(text) for text in printed)


@pytest.mark.parametrize(
    ("handler_module", "handler_name", "argv"),
    [
        (daily_cli, "generate_daily_report", ["daily", "--input", "work"]),
        (
            weekly_cli,
            "generate_weekly_report_cmd",
            ["weekly", "2026-W01", "--source", "db"],
        ),
        (
            monthly_cli,
            "generate_monthly_report_cmd",
            ["monthly", "2026-01", "--source", "db"],
        ),
    ],
)
def test_main_returns_one_when_report_command_fails(
    monkeypatch,
    handler_module,
    handler_name: str,
    argv: list[str],
):
    monkeypatch.setattr(handler_module, handler_name, lambda args, *, console: False)
    monkeypatch.setattr(sys, "argv", ["main.py", *argv])

    assert main.main() == 1


@pytest.mark.parametrize(
    ("handler_module", "handler_name", "argv"),
    [
        (daily_cli, "generate_daily_report", ["daily", "--input", "work"]),
        (
            weekly_cli,
            "generate_weekly_report_cmd",
            ["weekly", "2026-W01", "--source", "db"],
        ),
        (
            monthly_cli,
            "generate_monthly_report_cmd",
            ["monthly", "2026-01", "--source", "db"],
        ),
    ],
)
def test_main_returns_zero_when_report_command_succeeds(
    monkeypatch,
    handler_module,
    handler_name: str,
    argv: list[str],
):
    monkeypatch.setattr(handler_module, handler_name, lambda args, *, console: True)
    monkeypatch.setattr(sys, "argv", ["main.py", *argv])

    assert main.main() == 0


def test_main_returns_doctor_status(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["main.py", "doctor"])
    monkeypatch.setattr(
        doctor_cli,
        "run_doctor_cmd",
        lambda *, console, collect, strict=False: False,
    )
    assert main.main() == 1

    monkeypatch.setattr(
        doctor_cli,
        "run_doctor_cmd",
        lambda *, console, collect, strict=False: True,
    )
    assert main.main() == 0


def test_main_returns_zero_without_subcommand(monkeypatch):
    monkeypatch.setattr(sys, "argv", ["main.py"])

    assert main.main() == 0


def test_main_returns_one_for_unexpected_exception(monkeypatch):
    def fail(*args, **kwargs) -> None:
        raise RuntimeError("unexpected failure")

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(sys, "argv", ["main.py", "list"])
    monkeypatch.setattr(list_cli, "list_reports", fail)

    assert main.main() == 1
    assert any("unexpected failure" in text for text in printed)


def test_main_returns_130_for_keyboard_interrupt(monkeypatch):
    def interrupt(*args, **kwargs) -> None:
        raise KeyboardInterrupt

    printed = _patch_console(monkeypatch)
    monkeypatch.setattr(sys, "argv", ["main.py", "list"])
    monkeypatch.setattr(list_cli, "list_reports", interrupt)

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
