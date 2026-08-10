"""Single-source CPython runtime contract for Windows builds and deployment."""

import platform
import tomllib
from pathlib import Path

import yaml


PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXPECTED_PYTHON_VERSION = "3.13.13"
WINDOWS_WORKFLOWS = {
    ".github/workflows/ci.yml": "windows-production",
    ".github/workflows/windows-release.yml": "build-verify-package",
}


def test_test_suite_runs_on_the_supported_cpython() -> None:
    assert platform.python_implementation() == "CPython"
    assert platform.python_version() == EXPECTED_PYTHON_VERSION


def test_project_metadata_uses_the_exact_python_version() -> None:
    version_file = PROJECT_ROOT / ".python-version"
    pyproject = tomllib.loads(
        (PROJECT_ROOT / "pyproject.toml").read_text(encoding="utf-8")
    )

    assert version_file.read_text(encoding="utf-8").splitlines() == [
        EXPECTED_PYTHON_VERSION
    ]
    assert (
        pyproject["project"]["requires-python"]
        == f"=={EXPECTED_PYTHON_VERSION}"
    )


def test_windows_workflows_consume_the_version_file() -> None:
    for relative_path, job_name in WINDOWS_WORKFLOWS.items():
        workflow = yaml.safe_load(
            (PROJECT_ROOT / relative_path).read_text(encoding="utf-8")
        )
        job = workflow["jobs"][job_name]
        setup_python = next(
            step
            for step in job["steps"]
            if step.get("uses") == "actions/setup-python@v5"
        )

        assert EXPECTED_PYTHON_VERSION in job["name"]
        assert setup_python["with"]["python-version-file"] == (
            ".python-version"
        )
        assert "python-version" not in setup_python["with"]

    ci = yaml.safe_load(
        (PROJECT_ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8"
        )
    )
    assert set(ci["jobs"]) == {"windows-production"}
    assert "linux-compatibility" not in ci["jobs"]


def test_deployment_validates_creator_and_venv_before_work() -> None:
    script = (PROJECT_ROOT / "scripts" / "deploy_windows.ps1").read_text(
        encoding="utf-8"
    )
    package_script = (
        PROJECT_ROOT / "scripts" / "package_windows.ps1"
    ).read_text(encoding="utf-8")

    version_read = script.index(
        "Get-Content -LiteralPath $pythonVersionFile -Raw"
    )
    creator_check = script.index(
        'Assert-CPythonVersion -Label "Creator Python" -FilePath $Python'
    )
    existing_venv_check = script.index(
        'Assert-CPythonVersion -Label "Existing .venv Python"'
    )
    dependency_install = script.index("Install Python dependencies from")
    rust_build = script.index('Invoke-CheckedCommand -Label "Build Rust workspace"')
    strict_doctor = script.index('Invoke-CheckedCommand -Label "Run deployment doctor"')

    assert version_read < creator_check < existing_venv_check
    assert existing_venv_check < dependency_install < rust_build < strict_doctor
    assert "platform.python_implementation()" in script
    assert "platform.python_version()" in script
    assert "Expected: CPython $expectedPythonVersion" in script
    assert "Actual:" in script
    assert "Repair:" in script
    assert "Existing .venv directories are not removed automatically" in script
    assert "$venvPathExisted -and -not $venvExisted" in script
    assert "if (-not $venvPathExisted)" in script
    assert "Remove-Item -LiteralPath $venvDir" not in script
    assert "Join-Path $repoRoot '.python-version'" in package_script
