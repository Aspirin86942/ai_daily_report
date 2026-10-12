"""Read non-secret task configuration and emit an auditable Windows task plan."""

from datetime import datetime
import json
import os
from pathlib import Path
import sqlite3
import sys
import xml.etree.ElementTree as ET
from zoneinfo import ZoneInfo

PROJECT_ROOT = Path(__file__).resolve().parents[1]
if str(PROJECT_ROOT) not in sys.path:
    sys.path.insert(0, str(PROJECT_ROOT))

NAMESPACE = "http://schemas.microsoft.com/windows/2004/02/mit/task"
ET.register_namespace("", NAMESPACE)


def task_xml(plan: dict, *, sid: str, start_day: str) -> str:
    def element(parent, name, value=None, **attributes):
        node = ET.SubElement(parent, f"{{{NAMESPACE}}}{name}", attributes)
        if value is not None:
            node.text = str(value)
        return node

    task = ET.Element(f"{{{NAMESPACE}}}Task", {"version": "1.3"})
    registration = element(task, "RegistrationInfo")
    element(registration, "Description", plan["owner_marker"])
    element(registration, "Author", sid)
    triggers = element(task, "Triggers")
    daily = element(triggers, "CalendarTrigger", id="DailyShanghai")
    element(daily, "Enabled", "true")
    element(daily, "StartBoundary", f"{start_day}T{plan['schedule']['local_time']}:00+08:00")
    calendar = element(daily, "ScheduleByDay")
    element(calendar, "DaysInterval", "1")
    if plan["schedule"]["logon_catchup"]:
        logon = element(triggers, "LogonTrigger", id="LogonCatchup")
        element(logon, "Enabled", "true")
        element(logon, "UserId", sid)
        element(logon, "Delay", "PT1M")
    principals = element(task, "Principals")
    principal = element(principals, "Principal", id="DailyReportUser")
    element(principal, "UserId", sid)
    element(principal, "LogonType", "InteractiveToken")
    element(principal, "RunLevel", "LeastPrivilege")
    settings = element(task, "Settings")
    values = {"MultipleInstancesPolicy": "IgnoreNew", "DisallowStartIfOnBatteries": not plan["schedule"]["allow_on_battery"],
        "StopIfGoingOnBatteries": plan["schedule"]["stop_on_battery"], "StartWhenAvailable": True,
        "WakeToRun": plan["schedule"]["wake_to_run"], "AllowStartOnDemand": True, "Enabled": True,
        "ExecutionTimeLimit": f"PT{plan['schedule']['execution_limit_minutes']}M"}
    for key, value in values.items():
        element(settings, key, str(value).lower() if isinstance(value, bool) else value)
    if plan["schedule"]["restart_count"]:
        restart = element(settings, "RestartOnFailure")
        element(restart, "Interval", f"PT{plan['schedule']['restart_interval_minutes']}M")
        element(restart, "Count", plan["schedule"]["restart_count"])
    actions = element(task, "Actions", Context="DailyReportUser")
    execute = element(actions, "Exec")
    element(execute, "Command", plan["pythonw"])
    element(execute, "Arguments", plan["arguments"])
    element(execute, "WorkingDirectory", plan["project_root"])
    return ET.tostring(task, encoding="unicode", xml_declaration=False)


def read_plan(*, sid: str) -> dict:
    from src.core.config import config
    from src.services.report_runner.evidence_daily import EvidenceDailyService

    root = config.project_root.resolve()
    pythonw = root / ".venv/Scripts/pythonw.exe"
    main = root / "main.py"
    if not pythonw.is_file() or not main.is_file():
        raise RuntimeError("TASK_RUNTIME_MISSING")
    if sys.version_info[:3] != (3, 13, 13):
        raise RuntimeError("TASK_PYTHON_VERSION_MISMATCH")
    schedule = config.schedule_settings.model_dump()
    if any(char in schedule["task_name"] for char in '\\/:*?"<>|'):
        raise RuntimeError("TASK_NAME_INVALID")
    plan = {"project_root": str(root), "pythonw": str(pythonw), "python": str(root / ".venv/Scripts/python.exe"),
        "main": str(main), "arguments": f'"{main}" daily --scheduled', "schedule": schedule,
        "task_name": schedule["task_name"], "task_path": "\\", "user_sid": sid,
        "owner_marker": f"AI Daily Report | checkout={root}", "data_dir": str(config.data_dir),
        "work_dir": str(config.work_dir), "reports_dir": str(config.reports_dir),
        "credential_present": bool(os.environ.get("DEEPSEEK_API_KEY")),
        "capabilities_configured": config.model_request_settings.context_window_tokens is not None
            and config.model_request_settings.verified_max_output_tokens is not None,
        "model_check_verified": False}
    db = config.db_dir / "reports.sqlite3"
    if db.exists():
        connection = sqlite3.connect(db.resolve().as_uri() + "?mode=ro", uri=True)
        try:
            if connection.execute("SELECT 1 FROM sqlite_master WHERE name='report_runs'").fetchone():
                identity = EvidenceDailyService(config)._config_fingerprint()
                plan["model_check_verified"] = connection.execute("""SELECT 1 FROM report_runs WHERE run_kind='model_check'
                    AND status='succeeded' AND config_fingerprint=? LIMIT 1""", (identity,)).fetchone() is not None
        finally:
            connection.close()
    plan["xml"] = task_xml(plan, sid=sid, start_day=datetime.now(ZoneInfo("Asia/Shanghai")).date().isoformat())
    return plan


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument("--sid", required=True)
    args = parser.parse_args()
    try:
        print(json.dumps(read_plan(sid=args.sid), ensure_ascii=False))
    except Exception as error:
        print(json.dumps({"error": type(error).__name__}), file=sys.stderr)
        raise SystemExit(1)
