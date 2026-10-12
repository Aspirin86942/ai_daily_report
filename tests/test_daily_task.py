import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import xml.etree.ElementTree as ET

import pytest

from daily_task_plan import NAMESPACE, task_xml
from src.models.report_settings import ScheduleSettings


ROOT = Path(__file__).parents[1]


def task_plan(tmp_path):
    root = tmp_path / "中文 工作目录"
    root.mkdir()
    return {"project_root": str(root), "pythonw": str(root / ".venv/Scripts/pythonw.exe"),
        "arguments": f'"{root / "main.py"}" daily --scheduled', "owner_marker": f"AI Daily Report | checkout={root}",
        "data_dir": str(root / "data"), "task_name": "AI Daily Report Test", "task_path": "\\",
        "credential_present": True, "capabilities_configured": True, "model_check_verified": True,
        "schedule": ScheduleSettings().model_dump()}


def test_task_xml_uses_explicit_shanghai_time_and_bounded_interactive_execution(tmp_path):
    plan = task_plan(tmp_path)
    document = ET.fromstring(task_xml(plan, sid="S-1-5-21-1000", start_day="2026-10-12"))
    ns = {"t": NAMESPACE}
    value = lambda path: document.findtext(path, namespaces=ns)
    assert value("t:Actions/t:Exec/t:Arguments") == plan["arguments"]
    assert value("t:Actions/t:Exec/t:WorkingDirectory") == plan["project_root"]
    assert value("t:Actions/t:Exec/t:Command") == plan["pythonw"]
    assert value("t:Triggers/t:CalendarTrigger/t:StartBoundary") == "2026-10-12T09:00:00+08:00"
    assert value("t:Triggers/t:LogonTrigger/t:Delay") == "PT1M"
    assert value("t:Principals/t:Principal/t:LogonType") == "InteractiveToken"
    assert value("t:Principals/t:Principal/t:RunLevel") == "LeastPrivilege"
    for field, expected in {"MultipleInstancesPolicy": "IgnoreNew", "DisallowStartIfOnBatteries": "false",
        "StopIfGoingOnBatteries": "false", "StartWhenAvailable": "true", "WakeToRun": "false",
        "ExecutionTimeLimit": "PT20M", "RestartOnFailure/Interval": "PT15M", "RestartOnFailure/Count": "2"}.items():
        assert value("t:Settings/" + "/".join("t:" + part for part in field.split("/"))) == expected


@pytest.mark.skipif(os.name != "nt", reason="Windows task cmdlet adapter")
def test_task_api_fake_covers_preview_apply_idempotence_foreign_owner_and_removal(tmp_path):
    shell = shutil.which("pwsh") or shutil.which("powershell")
    plan = task_plan(tmp_path)
    plan["xml"] = task_xml(plan, sid="S-1-5-21-1000", start_day="2026-10-12")
    plan_path = tmp_path / "plan.json"
    plan_path.write_text(json.dumps(plan, ensure_ascii=False), encoding="utf-8")
    script = tmp_path / "task-test.ps1"
    script.write_text(r'''
param($Common, $PlanPath)
. $Common
$plan = Get-Content -Raw -LiteralPath $PlanPath -Encoding utf8 | ConvertFrom-Json
$script:exists = $false
$script:registrations = 0
$script:xml = ''
function Get-ScheduledTask { param($TaskPath) if ($script:exists) { [pscustomobject]@{ TaskName = $plan.task_name } } }
function Export-ScheduledTask { param($TaskName, $TaskPath) return $script:xml }
function Register-ScheduledTask { param($TaskName, $TaskPath, $Xml, [switch]$Force)
    $script:registrations += 1; $script:exists = $true; $script:xml = $Xml }
function Unregister-ScheduledTask { param($TaskName, $TaskPath, [switch]$Confirm) $script:exists = $false }
function Disable-ScheduledTask { param($TaskName, $TaskPath) throw 'Unexpected mismatch' }
$preview = Install-ReportDailyTask -Plan $plan
if ($preview.Applied -or $script:registrations -ne 0) { throw 'Preview wrote a task' }
$applied = Install-ReportDailyTask -Plan $plan -Apply
if (-not $applied.Applied -or $script:registrations -ne 1) { throw 'Apply missing' }
$again = Install-ReportDailyTask -Plan $plan -Apply
if ($again.Action -ne 'unchanged' -or $script:registrations -ne 1) { throw 'Repeated registration' }
$script:xml = $script:xml.Replace($plan.owner_marker, 'unrelated project')
$blocked = $false
try { Install-ReportDailyTask -Plan $plan -Apply | Out-Null } catch {
    $blocked = $_.Exception.Message -eq 'TASK_NAME_BELONGS_TO_ANOTHER_PROJECT'
}
if (-not $blocked) { throw 'Foreign task was not protected' }
$script:xml = $plan.xml
Remove-ReportDailyTask -Plan $plan | Out-Null
if (-not $script:exists) { throw 'Remove preview deleted a task' }
Remove-ReportDailyTask -Plan $plan -Apply | Out-Null
if ($script:exists) { throw 'Remove apply failed' }
$plan.credential_present = $false
$blocked = $false
try { Install-ReportDailyTask -Plan $plan -Apply | Out-Null } catch { $blocked = $_.Exception.Message -eq 'TASK_CREDENTIAL_NOT_VISIBLE' }
if (-not $blocked) { throw 'Credential prerequisite missing' }
'TASK_FAKE_CHECKS_PASSED'
''', encoding="utf-8-sig")
    result = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-File", str(script),
        "-Common", str(ROOT / "scripts/daily_task_common.ps1"), "-PlanPath", str(plan_path)],
        capture_output=True, encoding="utf-8", errors="replace", timeout=30)
    assert result.returncode == 0, result.stderr
    assert "TASK_FAKE_CHECKS_PASSED" in result.stdout


@pytest.mark.skipif(os.name != "nt", reason="pythonw requires Windows")
def test_pythonw_logs_failure_without_console_or_private_exception_text(tmp_path):
    script = tmp_path / "no-console.py"
    script.write_text('''import sys
from pathlib import Path
import runpy
sys.path.insert(0, sys.argv[1])
from src.core.config import config
config._log_dir = Path(sys.argv[2]) / "logs"
config._db_dir = Path(sys.argv[2]) / "db"
import logging
for handler in logging.getLogger("ai_daily_report").handlers[:]:
    logging.getLogger("ai_daily_report").removeHandler(handler)
    handler.close()
import src.cli.list_reports as listing
def fail(*args, **kwargs):
    raise RuntimeError("PRIVATE_EXCEPTION_DETAIL")
listing.list_reports = fail
sys.stdout = None
sys.stderr = None
sys.argv = [str(Path(sys.argv[1]) / "main.py"), "list"]
runpy.run_path(sys.argv[0], run_name="__main__")
''', encoding="utf-8")
    pythonw = Path(sys.executable).with_name("pythonw.exe")
    result = subprocess.run([str(pythonw), str(script), str(ROOT), str(tmp_path)], timeout=30)
    assert result.returncode == 1
    log = "".join(path.read_text(encoding="utf-8") for path in (tmp_path / "logs").glob("*.log"))
    assert "RuntimeError" in log and "PRIVATE_EXCEPTION_DETAIL" not in log


@pytest.mark.skipif(os.name != "nt", reason="Windows exported task normalization")
def test_readback_accepts_omitted_defaults_and_account_name_but_rejects_disabled_task(tmp_path):
    shell = shutil.which("pwsh") or shutil.which("powershell")
    plan = task_plan(tmp_path)
    plan["xml"] = task_xml(plan, sid="S-1-5-21-1000", start_day="2026-10-12")
    plan_path = tmp_path / "plan.json"
    plan_path.write_text(json.dumps(plan, ensure_ascii=False), encoding="utf-8")
    script = tmp_path / "normalize-task.ps1"
    script.write_text(r'''
param($Common, $PlanPath)
. $Common
$plan = Get-Content -Raw -LiteralPath $PlanPath -Encoding utf8 | ConvertFrom-Json
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$plan.xml = $plan.xml.Replace('S-1-5-21-1000', $identity.User.Value)
[xml]$exported = $plan.xml
$ns = [Xml.XmlNamespaceManager]::new($exported.NameTable)
$ns.AddNamespace('t', 'http://schemas.microsoft.com/windows/2004/02/mit/task')
foreach ($query in @('//t:RunLevel', '//t:WakeToRun', '//t:AllowStartOnDemand', '//t:Enabled')) {
    foreach ($node in @($exported.SelectNodes($query, $ns))) { $node.ParentNode.RemoveChild($node) | Out-Null }
}
foreach ($node in @($exported.SelectNodes('//t:UserId', $ns))) { $node.InnerText = $identity.Name }
$differences = @(Get-ReportTaskDifferences -Plan $plan -Xml $exported.OuterXml)
if ($differences.Count -ne 0) { throw 'Equivalent Windows export was rejected' }
$enabled = $exported.CreateElement('Enabled', $ns.LookupNamespace('t'))
$enabled.InnerText = 'false'
$exported.SelectSingleNode('/t:Task/t:Settings', $ns).AppendChild($enabled) | Out-Null
$differences = @(Get-ReportTaskDifferences -Plan $plan -Xml $exported.OuterXml)
if ($differences.Count -ne 1 -or $differences[0].Field -ne '/t:Task/t:Settings/t:Enabled') {
    throw 'Disabled task was not detected'
}
'TASK_NORMALIZATION_PASSED'
''', encoding="utf-8-sig")
    result = subprocess.run([shell, "-NoProfile", "-NonInteractive", "-File", str(script),
        "-Common", str(ROOT / "scripts/daily_task_common.ps1"), "-PlanPath", str(plan_path)],
        capture_output=True, encoding="utf-8", errors="replace", timeout=30)
    assert result.returncode == 0, result.stderr
    assert "TASK_NORMALIZATION_PASSED" in result.stdout
