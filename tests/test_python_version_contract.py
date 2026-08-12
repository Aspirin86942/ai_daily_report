"""Single-source CPython runtime contract for Windows builds and deployment."""

import platform
import tomllib
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXPECTED_PYTHON_VERSION = "3.13.13"


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
    assert pyproject["tool"]["maturin"]["module-name"] == (
        "ai_daily_scanner_native"
    )

# End of runtime contract assertions.

\n