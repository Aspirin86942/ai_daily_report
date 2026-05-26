# Python Wheel With Bundled Rust Binaries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Package the full `ai_daily_report` CLI as Linux and Windows wheels that include prebuilt Rust CLI binaries, so users can install from GitHub Release without Rust.

**Architecture:** Keep the existing Rust/Python subprocess boundary. Move Python code into an installable `ai_daily_report` package, add a Rust binary resolver for package data vs. developer config paths, and build platform wheels in GitHub Actions. Release versioning is `0.5.0`, tag `v0.5.0`, title `ver 0.5`.

**Tech Stack:** Python 3.10+, setuptools/wheel, pytest, Dynaconf, importlib.resources, Rust stable, GitHub Actions, existing Rust CLI crates.

---

## Scope Check

This plan intentionally keeps the Rust helpers as CLI binaries. It does not introduce PyO3, maturin, PyPI publishing, macOS wheels, or install-time Rust compilation. The work is large but one coherent release path: package structure, binary resolution, CI build, and release handoff all need to land together for an installable wheel to work.

## File Structure

Create:

- `pyproject.toml` - Python package metadata, dependencies, console script entry, package data declaration.
- `setup.py` - custom `bdist_wheel` command that marks wheels as platform wheels.
- `src/ai_daily_report/__init__.py` - package version.
- `src/ai_daily_report/__main__.py` - `python -m ai_daily_report` entry.
- `src/ai_daily_report/core/resources.py` - package resource helpers for templates and bundled examples.
- `src/ai_daily_report/core/runtime_paths.py` - runtime base directory and config directory resolution.
- `src/ai_daily_report/core/rust_binaries.py` - platform mapping and Rust binary resolver.
- `tests/test_package_entrypoints.py` - package import, parser, and version tests.
- `tests/test_resources.py` - package resource tests.
- `tests/test_runtime_paths.py` - config/data path tests.
- `tests/test_rust_binaries.py` - resolver tests.
- `.github/workflows/release-wheels.yml` - Linux/Windows wheel build and Release upload.

Move:

- `main.py` -> `src/ai_daily_report/main.py`
- `src/core/` -> `src/ai_daily_report/core/`
- `src/models/` -> `src/ai_daily_report/models/`
- `src/services/` -> `src/ai_daily_report/services/`
- `src/utils/` -> `src/ai_daily_report/utils/`
- `templates/` -> `src/ai_daily_report/templates/`
- `config/settings.example.yaml` -> copy into `src/ai_daily_report/config/settings.example.yaml`, while keeping the root copy for README and local development.

Modify:

- `main.py` - turn into a thin compatibility wrapper.
- `src/ai_daily_report/main.py` - package imports, version string, `doctor --no-api`, optional `--version`.
- `src/ai_daily_report/core/config.py` - package-aware settings loading and relative runtime paths.
- `src/ai_daily_report/core/healthcheck.py` - resource checks, no-API mode, Rust binary status.
- `src/ai_daily_report/core/llm.py` - template loading from package resources.
- `src/ai_daily_report/services/report_gen.py` - Jinja template loading from package resources.
- `src/ai_daily_report/services/scan_discovery.py` - discovery binary resolver integration.
- `src/ai_daily_report/services/office_parser.py` - office parser binary resolver integration.
- `src/ai_daily_report/services/scan_planner.py` - stable package binary profile marker.
- `config/settings.example.yaml` - version `0.5.0`, package binary defaults as `null`.
- `README.md`, `AGENTS.md`, `CLAUDE.md`, `docs/scanner-backends.md` - install/release/version guidance.
- Tests importing `src.*` - update to `ai_daily_report.*`.

---

### Task 1: Add Packaging Metadata And Entry Tests

**Files:**
- Create: `pyproject.toml`
- Create: `setup.py`
- Create: `src/ai_daily_report/__init__.py`
- Create: `src/ai_daily_report/__main__.py`
- Create: `tests/test_package_entrypoints.py`
- Modify later in this task: root `main.py`

- [ ] **Step 1: Write failing package entry tests**

Create `tests/test_package_entrypoints.py`:

```python
import importlib


def test_package_exposes_version():
    package = importlib.import_module("ai_daily_report")

    assert package.__version__ == "0.5.0"


def test_package_main_builds_parser():
    package_main = importlib.import_module("ai_daily_report.main")

    parser = package_main.build_parser()
    parsed = parser.parse_args(["--version"])

    assert parsed.show_version is True


def test_root_main_delegates_to_package():
    root_main = importlib.import_module("main")
    package_main = importlib.import_module("ai_daily_report.main")

    assert root_main.main is package_main.main
```

- [ ] **Step 2: Run the package entry tests to verify RED**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_package_entrypoints.py -q
```

Expected: fail with `ModuleNotFoundError: No module named 'ai_daily_report'`.

- [ ] **Step 3: Add package metadata**

Create `pyproject.toml`:

```toml
[build-system]
requires = ["setuptools>=69", "wheel"]
build-backend = "setuptools.build_meta"

[project]
name = "ai-daily-report"
version = "0.5.0"
description = "LLM assisted audit daily, weekly, and monthly report generator"
readme = "README.md"
requires-python = ">=3.10"
dependencies = [
  "pydantic>=2.0.0",
  "dynaconf>=3.2.0",
  "PyYAML>=6.0.0",
  "rich>=13.0.0",
  "pandas>=2.0.0",
  "openpyxl>=3.1.0",
  "python-pptx>=0.6.0",
  "pdfplumber>=0.10.0",
  "jinja2>=3.1.0",
  "python-docx>=1.1.0",
  "sharepoint-to-text>=1.1,<2",
  "openai>=1.0.0",
]

[project.scripts]
ai-daily-report = "ai_daily_report.main:main"

[tool.setuptools]
package-dir = {"" = "src"}
include-package-data = true

[tool.setuptools.packages.find]
where = ["src"]

[tool.setuptools.package-data]
ai_daily_report = [
  "templates/*.md",
  "config/*.yaml",
  "rust_bins/linux-x86_64/*",
  "rust_bins/win-amd64/*",
]
```

Create `setup.py`:

```python
from setuptools import setup
from wheel.bdist_wheel import bdist_wheel as _bdist_wheel


class bdist_wheel(_bdist_wheel):
    """Mark wheels as platform wheels because they include Rust executables."""

    def finalize_options(self):
        super().finalize_options()
        self.root_is_pure = False


setup(cmdclass={"bdist_wheel": bdist_wheel})
```

Create `src/ai_daily_report/__init__.py`:

```python
"""Installable package for the audit report generator."""

__version__ = "0.5.0"
```

Create `src/ai_daily_report/__main__.py`:

```python
from .main import main


if __name__ == "__main__":
    main()
```

- [ ] **Step 4: Move `main.py` into the package and keep root compatibility**

Run:

```bash
mkdir -p src/ai_daily_report
git mv main.py src/ai_daily_report/main.py
```

Create new root `main.py`:

```python
"""Compatibility wrapper for source-tree execution.

Installed users should prefer the `ai-daily-report` console script.
"""

from ai_daily_report.main import main


if __name__ == "__main__":
    main()
```

- [ ] **Step 5: Update package CLI version handling**

In `src/ai_daily_report/main.py`, change imports from `src.*` to `ai_daily_report.*`, import version, and update parser setup:

```python
from ai_daily_report import __version__
from ai_daily_report.core.healthcheck import collect_healthcheck
from ai_daily_report.core.logger import setup_logger
from ai_daily_report.core.llm import LLMClient
from ai_daily_report.models.schemas import ScanResult
from ai_daily_report.services.context_scheduler import (
    ContextScheduleRequest,
    ContextScheduleResult,
    ContextScheduler,
)
from ai_daily_report.services.report_gen import ReportGenerator
from ai_daily_report.services.sqlite_store import SQLiteStore
from ai_daily_report.utils.text_tools import parse_week_label, get_month_date_range
```

Inside `build_parser()`:

```python
parser = argparse.ArgumentParser(description=f"审计日报生成器 v{__version__}")
parser.add_argument(
    "--version",
    action="store_true",
    dest="show_version",
    help="显示版本号",
)
```

Inside `main()` after parsing args:

```python
if getattr(args, "show_version", False):
    console.print(f"ai-daily-report {__version__}")
    return
```

Inside `generate_daily_report()` banner:

```python
console.print(f"\n[bold green]===== 审计日报生成器 v{__version__} =====[/bold green]\n")
```

- [ ] **Step 6: Run package entry tests to verify GREEN**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_package_entrypoints.py -q
```

Expected: `3 passed`.

- [ ] **Step 7: Commit Task 1**

Run:

```bash
git add pyproject.toml setup.py main.py src/ai_daily_report/__init__.py src/ai_daily_report/__main__.py src/ai_daily_report/main.py tests/test_package_entrypoints.py
git commit -m "Add installable package entrypoints"
```

---

### Task 2: Move Python Modules Into `ai_daily_report`

**Files:**
- Move: `src/core/` -> `src/ai_daily_report/core/`
- Move: `src/models/` -> `src/ai_daily_report/models/`
- Move: `src/services/` -> `src/ai_daily_report/services/`
- Move: `src/utils/` -> `src/ai_daily_report/utils/`
- Modify: all Python imports in `src/ai_daily_report/`, `tests/`, `scripts/benchmark_scanner.py`

- [ ] **Step 1: Move module directories**

Run:

```bash
git mv src/core src/ai_daily_report/core
git mv src/models src/ai_daily_report/models
git mv src/services src/ai_daily_report/services
git mv src/utils src/ai_daily_report/utils
```

- [ ] **Step 2: Replace imports**

Run:

```bash
rg -l "from src\\.|import src\\.|src\\." src tests scripts | xargs perl -0pi -e 's/from src\\./from ai_daily_report\\./g; s/import src\\./import ai_daily_report\\./g; s/src\\./ai_daily_report\\./g'
```

Then inspect remaining old imports:

```bash
rg -n "from src\\.|import src\\.|src\\." src tests scripts
```

Expected: no matches.

- [ ] **Step 3: Fix package-relative imports if needed**

Where files under `src/ai_daily_report/` still use explicit `ai_daily_report.*` imports, keep them. Where local relative imports already exist, keep them. Do not introduce compatibility aliases under `src/`; the installed package should be the real import path.

- [ ] **Step 4: Run focused import tests**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_package_entrypoints.py tests/test_main.py -q
```

Expected: all selected tests pass.

- [ ] **Step 5: Run all tests after the move**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests -q
```

Expected: all tests pass. If tests fail only because they import old `src.*` paths, update those tests to `ai_daily_report.*` and rerun.

- [ ] **Step 6: Commit Task 2**

Run:

```bash
git add src tests scripts main.py
git commit -m "Move Python modules into package namespace"
```

---

### Task 3: Move Templates And Add Package Resource Helpers

**Files:**
- Create: `src/ai_daily_report/core/resources.py`
- Move: `templates/` -> `src/ai_daily_report/templates/`
- Modify: `src/ai_daily_report/core/llm.py`
- Modify: `src/ai_daily_report/services/report_gen.py`
- Modify: `src/ai_daily_report/core/healthcheck.py`
- Create: `tests/test_resources.py`
- Modify: tests reading root `templates/`

- [ ] **Step 1: Write failing resource tests**

Create `tests/test_resources.py`:

```python
from ai_daily_report.core.resources import (
    list_template_names,
    read_text_resource,
    template_root,
)


def test_template_resource_can_be_read():
    content = read_text_resource("templates/system_prompt.md")

    assert "JSON" in content or "日报" in content


def test_template_root_exposes_existing_template_files():
    root = template_root()

    assert (root / "report_template.md").is_file()
    assert "weekly_prompt.md" in list_template_names()
```

- [ ] **Step 2: Run resource tests to verify RED**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_resources.py -q
```

Expected: fail with `ModuleNotFoundError` for `ai_daily_report.core.resources`.

- [ ] **Step 3: Move templates into the package**

Run:

```bash
git mv templates src/ai_daily_report/templates
```

- [ ] **Step 4: Implement resource helpers**

Create `src/ai_daily_report/core/resources.py`:

```python
"""Package resource helpers.

These helpers keep installed wheels independent from the source tree layout.
"""

from __future__ import annotations

from importlib import resources
from pathlib import Path

PACKAGE_NAME = "ai_daily_report"


def package_root() -> Path:
    """Return the installed package root directory."""
    return Path(__file__).resolve().parents[1]


def resource_path(relative_path: str) -> Path:
    """Return a filesystem path for unpacked wheel resources."""
    return Path(resources.files(PACKAGE_NAME).joinpath(relative_path))


def read_text_resource(relative_path: str) -> str:
    """Read a UTF-8 package text resource."""
    return resource_path(relative_path).read_text(encoding="utf-8")


def template_root() -> Path:
    """Return the package template directory."""
    return resource_path("templates")


def list_template_names() -> list[str]:
    """List bundled Markdown template names."""
    return sorted(path.name for path in template_root().glob("*.md"))
```

- [ ] **Step 5: Update LLM template loading**

In `src/ai_daily_report/core/llm.py`, replace `Path(__file__).parent.parent.parent / "templates"` loading with:

```python
from .resources import read_text_resource
```

Use this body in the template-loading method or constructor section:

```python
self.prompt_templates = {
    "system_prompt": read_text_resource("templates/system_prompt.md"),
    "weekly_prompt": read_text_resource("templates/weekly_prompt.md"),
    "monthly_prompt": read_text_resource("templates/monthly_prompt.md"),
}
```

- [ ] **Step 6: Update ReportGenerator template loading**

In `src/ai_daily_report/services/report_gen.py`, import:

```python
from ai_daily_report.core.resources import template_root
```

Replace the existing template directory setup with:

```python
template_dir = template_root()
self.env = Environment(loader=FileSystemLoader(str(template_dir)))
```

- [ ] **Step 7: Update healthcheck template checks**

In `src/ai_daily_report/core/healthcheck.py`, import:

```python
from .resources import template_root
```

Replace root-template path checks with:

```python
templates_dir = template_root()
for template_name in TEMPLATE_FILES:
    template_path = templates_dir / template_name
    if not template_path.exists():
        result.errors.append(f"缺少包内模板文件: templates/{template_name}")
```

- [ ] **Step 8: Update tests that read root templates**

For tests that read `Path(__file__).resolve().parents[1] / "templates"`, switch to:

```python
from ai_daily_report.core.resources import template_root

prompt_path = template_root() / "weekly_prompt.md"
```

- [ ] **Step 9: Run resource and report tests**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_resources.py tests/test_report_gen.py tests/test_healthcheck.py -q
```

Expected: all selected tests pass.

- [ ] **Step 10: Commit Task 3**

Run:

```bash
git add src/ai_daily_report/templates src/ai_daily_report/core/resources.py src/ai_daily_report/core/llm.py src/ai_daily_report/services/report_gen.py src/ai_daily_report/core/healthcheck.py tests
git commit -m "Load templates from package resources"
```

---

### Task 4: Add Runtime Path And Config Resolution

**Files:**
- Create: `src/ai_daily_report/core/runtime_paths.py`
- Copy: `config/settings.example.yaml` -> `src/ai_daily_report/config/settings.example.yaml`
- Modify: `src/ai_daily_report/core/config.py`
- Modify: `src/ai_daily_report/core/healthcheck.py`
- Create: `tests/test_runtime_paths.py`
- Modify: `tests/test_config.py`

- [ ] **Step 1: Write failing runtime path tests**

Create `tests/test_runtime_paths.py`:

```python
from pathlib import Path

from ai_daily_report.core.runtime_paths import (
    default_runtime_root,
    resolve_config_dir,
    resolve_runtime_path,
)


def test_resolve_config_dir_prefers_env_override(tmp_path, monkeypatch):
    config_dir = tmp_path / "custom-config"
    monkeypatch.setenv("DAILY_REPORT_CONFIG_DIR", str(config_dir))

    assert resolve_config_dir() == config_dir


def test_resolve_config_dir_defaults_to_cwd_config(tmp_path, monkeypatch):
    monkeypatch.delenv("DAILY_REPORT_CONFIG_DIR", raising=False)
    monkeypatch.chdir(tmp_path)

    assert resolve_config_dir() == tmp_path / "config"


def test_relative_runtime_paths_are_cwd_relative(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)

    assert resolve_runtime_path("data/db") == tmp_path / "data/db"


def test_absolute_runtime_paths_are_preserved(tmp_path):
    absolute = tmp_path / "reports"

    assert resolve_runtime_path(str(absolute)) == absolute


def test_default_runtime_root_is_cwd(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)

    assert default_runtime_root() == tmp_path
```

- [ ] **Step 2: Run runtime path tests to verify RED**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_runtime_paths.py -q
```

Expected: fail with `ModuleNotFoundError` for `runtime_paths`.

- [ ] **Step 3: Add runtime path helper**

Create `src/ai_daily_report/core/runtime_paths.py`:

```python
"""Runtime filesystem path resolution for source and installed-wheel modes."""

from __future__ import annotations

import os
from pathlib import Path


def default_runtime_root() -> Path:
    """Use the current working directory for user data and local config."""
    return Path.cwd()


def resolve_config_dir() -> Path:
    """Return local config directory.

    DAILY_REPORT_CONFIG_DIR is for advanced deployments. The default keeps
    config beside the user's working directory instead of inside site-packages.
    """
    configured = os.getenv("DAILY_REPORT_CONFIG_DIR")
    if configured:
        return Path(configured).expanduser()
    return default_runtime_root() / "config"


def resolve_runtime_path(value: str | Path) -> Path:
    """Resolve runtime data paths relative to the current working directory."""
    path = Path(value).expanduser()
    if path.is_absolute():
        return path
    return default_runtime_root() / path
```

- [ ] **Step 4: Copy bundled example config**

Run:

```bash
mkdir -p src/ai_daily_report/config
cp config/settings.example.yaml src/ai_daily_report/config/settings.example.yaml
```

Then edit both `config/settings.example.yaml` and `src/ai_daily_report/config/settings.example.yaml`:

```yaml
app:
  name: "审计日报生成器"
  version: "0.5.0"
```

Change Rust binary defaults:

```yaml
scanner:
  discovery_backend: "rust"
  rust_discovery_bin:
  office_parser_backend: "rust_office_oxide_v1"
  rust_office_parser_bin:
```

- [ ] **Step 5: Update Config settings file order**

In `src/ai_daily_report/core/config.py`, import:

```python
from .resources import resource_path
from .runtime_paths import resolve_config_dir, resolve_runtime_path
```

Replace `_initialize()` with:

```python
def _initialize(self):
    """初始化配置。"""
    config_dir = resolve_config_dir()
    self._settings = self._build_settings(config_dir)
```

Replace `_settings_files()` with:

```python
@classmethod
def _settings_files(
    cls,
    config_dir: Path,
    system_name: str | None = None,
) -> list[str]:
    """返回 Dynaconf 的配置文件读取顺序。"""
    return [
        str(resource_path("config/settings.example.yaml")),
        str(config_dir / cls._settings_file_name(system_name)),
        str(config_dir / ".secrets.yaml"),
    ]
```

Replace path properties:

```python
@property
def data_dir(self) -> Path:
    return resolve_runtime_path(self._settings.paths.data_dir)


@property
def reports_dir(self) -> Path:
    return resolve_runtime_path(self._settings.paths.reports_dir)


@property
def db_dir(self) -> Path:
    return resolve_runtime_path(self._settings.paths.db_dir)
```

Keep `work_dir` as configured:

```python
@property
def work_dir(self) -> Path:
    return Path(self._settings.paths.work_dir).expanduser()
```

- [ ] **Step 6: Normalize optional Rust binary config values**

In `scanner_config`, replace default binary extraction with:

```python
"rust_discovery_bin": self._optional_string(
    getattr(self._settings.scanner, "rust_discovery_bin", None)
),
```

and:

```python
"rust_office_parser_bin": self._optional_string(
    getattr(self._settings.scanner, "rust_office_parser_bin", None)
),
```

Add helper to `Config`:

```python
@staticmethod
def _optional_string(value: Any) -> str | None:
    """Normalize empty Dynaconf/YAML strings to None."""
    if value is None:
        return None
    text = str(value).strip()
    return text or None
```

- [ ] **Step 7: Update healthcheck config file messages**

In `src/ai_daily_report/core/healthcheck.py`, use `resolve_config_dir()` for local config checks:

```python
from .runtime_paths import resolve_config_dir
```

Inside `_append_project_file_checks()`:

```python
config_dir = resolve_config_dir()
settings_file = config_dir / Config._settings_file_name()
secrets_file = config_dir / ".secrets.yaml"

if not settings_file.exists():
    result.warnings.append(
        f"缺少本机配置文件: {settings_file} (将使用包内 settings.example.yaml 默认值；正式使用前请复制并修改)"
    )
```

Keep missing secrets as a warning.

- [ ] **Step 8: Run config and runtime tests**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_runtime_paths.py tests/test_config.py tests/test_healthcheck.py -q
```

Expected: all selected tests pass.

- [ ] **Step 9: Commit Task 4**

Run:

```bash
git add config/settings.example.yaml src/ai_daily_report/config/settings.example.yaml src/ai_daily_report/core/runtime_paths.py src/ai_daily_report/core/config.py src/ai_daily_report/core/healthcheck.py tests/test_runtime_paths.py tests/test_config.py tests/test_healthcheck.py
git commit -m "Resolve runtime config paths for installed wheels"
```

---

### Task 5: Add Rust Binary Resolver

**Files:**
- Create: `src/ai_daily_report/core/rust_binaries.py`
- Create: `tests/test_rust_binaries.py`

- [ ] **Step 1: Write failing resolver tests**

Create `tests/test_rust_binaries.py`:

```python
from pathlib import Path

from ai_daily_report.core.rust_binaries import (
    BinaryResolution,
    binary_filename,
    map_platform_key,
    package_profile_value,
    resolve_rust_binary,
)


def test_map_platform_key_supports_linux_x86_64():
    assert map_platform_key(system_name="Linux", machine="x86_64") == "linux-x86_64"


def test_map_platform_key_supports_windows_amd64():
    assert map_platform_key(system_name="Windows", machine="AMD64") == "win-amd64"


def test_map_platform_key_rejects_unsupported_platform():
    assert map_platform_key(system_name="Darwin", machine="arm64") is None


def test_binary_filename_uses_exe_on_windows():
    assert binary_filename("discovery", "win-amd64") == "ai-daily-discovery.exe"
    assert binary_filename("office_parser", "linux-x86_64") == "ai-daily-office-parser"


def test_package_profile_value_is_stable():
    assert package_profile_value("office_parser", "linux-x86_64") == (
        "package:linux-x86_64/ai-daily-office-parser"
    )


def test_configured_path_takes_priority(tmp_path):
    binary = tmp_path / "ai-daily-discovery"
    binary.write_text("#!/bin/sh\n", encoding="utf-8")
    binary.chmod(0o755)

    resolution = resolve_rust_binary(
        binary_name="discovery",
        configured_path=str(binary),
        system_name="Linux",
        machine="x86_64",
    )

    assert resolution == BinaryResolution(
        name="discovery",
        source="config",
        platform_key="config",
        path=binary,
        available=True,
        profile_value=f"config:{binary}",
        error=None,
    )


def test_unsupported_package_platform_returns_unavailable():
    resolution = resolve_rust_binary(
        binary_name="office_parser",
        configured_path=None,
        system_name="Darwin",
        machine="arm64",
    )

    assert resolution.source == "unavailable"
    assert resolution.available is False
    assert resolution.profile_value == "package:unsupported"
```

- [ ] **Step 2: Run resolver tests to verify RED**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_rust_binaries.py -q
```

Expected: fail with `ModuleNotFoundError` for `rust_binaries`.

- [ ] **Step 3: Implement resolver**

Create `src/ai_daily_report/core/rust_binaries.py`:

```python
"""Resolve bundled or configured Rust CLI binaries."""

from __future__ import annotations

import os
import platform
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from .resources import resource_path

BinaryName = Literal["discovery", "office_parser"]
BinarySource = Literal["config", "package", "unavailable"]


@dataclass(frozen=True, slots=True)
class BinaryResolution:
    name: str
    source: BinarySource
    platform_key: str
    path: Path | None
    available: bool
    profile_value: str
    error: str | None


def map_platform_key(
    *,
    system_name: str | None = None,
    machine: str | None = None,
) -> str | None:
    system = (system_name or platform.system()).strip().lower()
    arch = (machine or platform.machine()).strip().lower()
    if system == "linux" and arch in {"x86_64", "amd64"}:
        return "linux-x86_64"
    if system.startswith("win") and arch in {"amd64", "x86_64"}:
        return "win-amd64"
    return None


def binary_filename(binary_name: BinaryName, platform_key: str) -> str:
    suffix = ".exe" if platform_key == "win-amd64" else ""
    match binary_name:
        case "discovery":
            return f"ai-daily-discovery{suffix}"
        case "office_parser":
            return f"ai-daily-office-parser{suffix}"
    raise ValueError(f"Unsupported Rust binary name: {binary_name}")


def package_profile_value(binary_name: BinaryName, platform_key: str) -> str:
    return f"package:{platform_key}/{binary_filename(binary_name, platform_key)}"


def _is_executable(path: Path, platform_key: str) -> bool:
    if not path.exists() or not path.is_file():
        return False
    if platform_key.startswith("win"):
        return path.suffix.lower() == ".exe"
    return os.access(path, os.X_OK)


def _validate_path(
    *,
    binary_name: BinaryName,
    source: BinarySource,
    platform_key: str,
    path: Path,
    profile_value: str,
) -> BinaryResolution:
    if not path.exists():
        return BinaryResolution(
            name=binary_name,
            source=source,
            platform_key=platform_key,
            path=path,
            available=False,
            profile_value=profile_value,
            error=f"binary does not exist: {path}",
        )
    if not _is_executable(path, platform_key):
        return BinaryResolution(
            name=binary_name,
            source=source,
            platform_key=platform_key,
            path=path,
            available=False,
            profile_value=profile_value,
            error=f"binary is not executable: {path}",
        )
    return BinaryResolution(
        name=binary_name,
        source=source,
        platform_key=platform_key,
        path=path,
        available=True,
        profile_value=profile_value,
        error=None,
    )


def resolve_rust_binary(
    *,
    binary_name: BinaryName,
    configured_path: str | None,
    system_name: str | None = None,
    machine: str | None = None,
) -> BinaryResolution:
    if configured_path:
        path = Path(configured_path).expanduser()
        return _validate_path(
            binary_name=binary_name,
            source="config",
            platform_key="config",
            path=path,
            profile_value=f"config:{path}",
        )

    platform_key = map_platform_key(system_name=system_name, machine=machine)
    if platform_key is None:
        return BinaryResolution(
            name=binary_name,
            source="unavailable",
            platform_key="unsupported",
            path=None,
            available=False,
            profile_value="package:unsupported",
            error="unsupported platform for bundled Rust binary",
        )

    filename = binary_filename(binary_name, platform_key)
    path = resource_path(f"rust_bins/{platform_key}/{filename}")
    return _validate_path(
        binary_name=binary_name,
        source="package",
        platform_key=platform_key,
        path=path,
        profile_value=package_profile_value(binary_name, platform_key),
    )


def rust_binary_profile_value(
    *,
    binary_name: BinaryName,
    configured_path: str | None,
    system_name: str | None = None,
    machine: str | None = None,
) -> str:
    return resolve_rust_binary(
        binary_name=binary_name,
        configured_path=configured_path,
        system_name=system_name,
        machine=machine,
    ).profile_value


def describe_rust_binaries() -> dict[str, dict[str, str]]:
    resolutions = {
        "discovery": resolve_rust_binary(
            binary_name="discovery",
            configured_path=None,
        ),
        "office_parser": resolve_rust_binary(
            binary_name="office_parser",
            configured_path=None,
        ),
    }
    return {
        key: {
            "source": resolution.source,
            "platform": resolution.platform_key,
            "path": str(resolution.path or ""),
            "available": str(resolution.available).lower(),
            "error": resolution.error or "",
        }
        for key, resolution in resolutions.items()
    }
```

- [ ] **Step 4: Add empty package binary directories**

Run:

```bash
mkdir -p src/ai_daily_report/rust_bins/linux-x86_64 src/ai_daily_report/rust_bins/win-amd64
touch src/ai_daily_report/rust_bins/linux-x86_64/.gitkeep src/ai_daily_report/rust_bins/win-amd64/.gitkeep
```

- [ ] **Step 5: Run resolver tests to verify GREEN**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_rust_binaries.py -q
```

Expected: all resolver tests pass.

- [ ] **Step 6: Commit Task 5**

Run:

```bash
git add src/ai_daily_report/core/rust_binaries.py src/ai_daily_report/rust_bins tests/test_rust_binaries.py
git commit -m "Add Rust binary resolver"
```

---

### Task 6: Integrate Resolver With Discovery, Office Parser, Scan Planner, And Doctor

**Files:**
- Modify: `src/ai_daily_report/services/scan_discovery.py`
- Modify: `src/ai_daily_report/services/office_parser.py`
- Modify: `src/ai_daily_report/services/scan_planner.py`
- Modify: `src/ai_daily_report/core/healthcheck.py`
- Modify: `src/ai_daily_report/main.py`
- Modify: `tests/test_scan_discovery.py`
- Modify: `tests/test_office_parser.py`
- Modify: `tests/test_scan_planner.py`
- Modify: `tests/test_healthcheck.py`
- Modify: `tests/test_main.py`

- [ ] **Step 1: Write failing scan planner profile test**

Add to `tests/test_scan_planner.py`:

```python
def test_build_parser_profile_uses_stable_package_office_binary_marker(monkeypatch):
    monkeypatch.setattr(
        "ai_daily_report.core.rust_binaries.map_platform_key",
        lambda system_name=None, machine=None: "linux-x86_64",
    )
    planner = ScanPlanner(
        scanner_cfg={
            "excel_max_rows": 50,
            "pdf_max_pages": 5,
            "text_max_chars": 6000,
            "total_max_chars": 50000,
            "rust_office_parser_bin": None,
        }
    )

    profile = planner.build_parser_profile(summary_mode=False)

    assert profile["rust_office_parser_bin"] == (
        "package:linux-x86_64/ai-daily-office-parser"
    )
```

- [ ] **Step 2: Write failing doctor no-api parser test**

Add to `tests/test_main.py`:

```python
def test_build_parser_accepts_doctor_no_api():
    parser = main.build_parser()

    args = parser.parse_args(["doctor", "--no-api"])

    assert args.subcommand == "doctor"
    assert args.no_api is True
```

- [ ] **Step 3: Run new focused tests to verify RED**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_scan_planner.py::test_build_parser_profile_uses_stable_package_office_binary_marker tests/test_main.py::test_build_parser_accepts_doctor_no_api -q
```

Expected: both fail.

- [ ] **Step 4: Integrate stable profile value in ScanPlanner**

In `src/ai_daily_report/services/scan_planner.py`, import:

```python
from ai_daily_report.core.rust_binaries import rust_binary_profile_value
```

Replace profile assignment for `rust_office_parser_bin` with:

```python
profile["rust_office_parser_bin"] = rust_binary_profile_value(
    binary_name="office_parser",
    configured_path=self.scanner_cfg.get("rust_office_parser_bin"),
)
```

- [ ] **Step 5: Integrate resolver in Rust discovery runner**

In `src/ai_daily_report/services/scan_discovery.py`, import:

```python
from ai_daily_report.core.rust_binaries import resolve_rust_binary
```

Replace `_resolve_binary_path()` with:

```python
def _resolve_binary_path(self) -> Path:
    resolution = resolve_rust_binary(
        binary_name="discovery",
        configured_path=self.scanner_cfg.get("rust_discovery_bin"),
    )
    if not resolution.available or resolution.path is None:
        raise RustDiscoveryError(resolution.error or "Rust discovery binary unavailable")
    return resolution.path
```

This preserves the existing fallback path in `FileDiscoveryService`, because `RustDiscoveryError` already causes Python discovery fallback.

- [ ] **Step 6: Integrate resolver in Office parser**

In `src/ai_daily_report/services/office_parser.py`, import:

```python
from ai_daily_report.core.rust_binaries import BinaryResolution, resolve_rust_binary
```

Change `RustOfficeParserRunner.__init__`:

```python
def __init__(self, binary_path: str | Path | BinaryResolution):
    self.binary_path = binary_path
```

Change `_resolve_binary_path()`:

```python
def _resolve_binary_path(self) -> Path:
    if isinstance(self.binary_path, BinaryResolution):
        if self.binary_path.available and self.binary_path.path is not None:
            return self.binary_path.path
        raise OSError(self.binary_path.error or "Rust Office parser binary unavailable")
    configured = Path(self.binary_path)
    if configured.is_absolute():
        return configured
    project_root = Path(__file__).resolve().parents[3]
    return project_root / configured
```

Change default runner creation:

```python
runner = rust_runner or RustOfficeParserRunner(
    resolve_rust_binary(
        binary_name="office_parser",
        configured_path=scanner_cfg.get("rust_office_parser_bin"),
    )
)
```

- [ ] **Step 7: Add doctor no-api argument**

In `src/ai_daily_report/main.py`, add:

```python
doctor_parser.add_argument(
    "--no-api",
    action="store_true",
    help="跳过 API Key 检查，供安装包 smoke test 使用",
)
```

Change `run_doctor_cmd`:

```python
def run_doctor_cmd(*, check_api: bool = True) -> bool:
    """检查运行环境和配置。"""
    console.print("\n[bold green]===== 环境检查 =====[/bold green]\n")

    result = collect_healthcheck(check_api=check_api)
```

Change doctor dispatch:

```python
case "doctor":
    if not run_doctor_cmd(check_api=not getattr(args, "no_api", False)):
        raise SystemExit(1)
```

- [ ] **Step 8: Extend healthcheck for no-api and Rust binary status**

In `src/ai_daily_report/core/healthcheck.py`, import:

```python
from .rust_binaries import resolve_rust_binary
```

Change `_append_runtime_config_checks` signature:

```python
def _append_runtime_config_checks(
    result: HealthCheckResult,
    cfg: Any,
    *,
    check_api: bool,
) -> None:
```

Before API key checks:

```python
if not check_api:
    result.warnings.append("已跳过 API Key 检查")
    return
```

Add function:

```python
def _append_rust_binary_checks(result: HealthCheckResult, cfg: Any) -> None:
    scanner_cfg = getattr(cfg, "scanner_config")
    for label, binary_name, config_key in (
        ("Rust discovery", "discovery", "rust_discovery_bin"),
        ("Rust office parser", "office_parser", "rust_office_parser_bin"),
    ):
        resolution = resolve_rust_binary(
            binary_name=binary_name,
            configured_path=scanner_cfg.get(config_key),
        )
        result.info[f"{label} source"] = resolution.source
        result.info[f"{label} platform"] = resolution.platform_key
        result.info[f"{label} path"] = str(resolution.path or "")
        if not resolution.available:
            result.warnings.append(f"{label} 不可用: {resolution.error}")
```

Change `collect_healthcheck` signature:

```python
def collect_healthcheck(
    project_root: Path | None = None,
    config_obj: Any | None = None,
    *,
    check_api: bool = True,
) -> HealthCheckResult:
```

Inside it:

```python
_append_runtime_config_checks(result, cfg, check_api=check_api)
_append_rust_binary_checks(result, cfg)
```

- [ ] **Step 9: Run focused integration tests**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_scan_planner.py tests/test_scan_discovery.py tests/test_office_parser.py tests/test_healthcheck.py tests/test_main.py -q
```

Expected: all selected tests pass.

- [ ] **Step 10: Commit Task 6**

Run:

```bash
git add src/ai_daily_report/services/scan_discovery.py src/ai_daily_report/services/office_parser.py src/ai_daily_report/services/scan_planner.py src/ai_daily_report/core/healthcheck.py src/ai_daily_report/main.py tests/test_scan_discovery.py tests/test_office_parser.py tests/test_scan_planner.py tests/test_healthcheck.py tests/test_main.py
git commit -m "Use bundled Rust binary resolver"
```

---

### Task 7: Build Local Platform Wheel And Smoke Test

**Files:**
- Modify: `pyproject.toml`
- Modify: `setup.py`
- Use existing Rust target binaries.
- Test: installed wheel smoke checks.

- [ ] **Step 1: Ensure local Rust release binaries exist**

Run:

```bash
cd rust/discovery && cargo test && cargo build --release
cd ../office_parser && cargo test && cargo build --release
cd ../..
```

Expected: both Rust crates test and build successfully.

- [ ] **Step 2: Copy Linux binaries into package data**

Run on Linux:

```bash
mkdir -p src/ai_daily_report/rust_bins/linux-x86_64
cp rust/discovery/target/release/ai-daily-discovery src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-discovery
cp rust/office_parser/target/release/ai-daily-office-parser src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-office-parser
chmod 755 src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-discovery src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-office-parser
```

Expected:

```bash
test -x src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-discovery
test -x src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-office-parser
```

- [ ] **Step 3: Build local Linux platform wheel**

Run:

```bash
rm -rf build dist *.egg-info
/home/george/miniconda3/bin/conda run -n test python setup.py bdist_wheel --plat-name linux_x86_64
```

Expected: `dist/ai_daily_report-0.5.0-py3-none-linux_x86_64.whl` exists.

- [ ] **Step 4: Install wheel into a temporary venv**

Run:

```bash
python -m venv /tmp/ai-daily-report-wheel-smoke
/tmp/ai-daily-report-wheel-smoke/bin/python -m pip install --upgrade pip
/tmp/ai-daily-report-wheel-smoke/bin/python -m pip install dist/ai_daily_report-0.5.0-py3-none-linux_x86_64.whl
```

Expected: install succeeds without Rust.

- [ ] **Step 5: Run installed package smoke checks**

Run:

```bash
/tmp/ai-daily-report-wheel-smoke/bin/ai-daily-report --version
/tmp/ai-daily-report-wheel-smoke/bin/ai-daily-report --help
/tmp/ai-daily-report-wheel-smoke/bin/ai-daily-report doctor --no-api
/tmp/ai-daily-report-wheel-smoke/bin/python -c "from ai_daily_report.core.rust_binaries import describe_rust_binaries; print(describe_rust_binaries())"
```

Expected:

- version prints `ai-daily-report 0.5.0`
- help exits 0
- doctor no-api exits 0 or only fails for missing required work dir if configured example remains `/path/to/work`; if it fails for work dir, adjust doctor no-api to report work dir as warning in no-api mode and rerun.
- resolver output shows package binaries available on Linux.

- [ ] **Step 6: Remove local copied binary artifacts before commit**

Because Release wheels should get binaries from CI and repository should not commit compiled binaries, run:

```bash
rm -f src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-discovery
rm -f src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-office-parser
rm -rf build dist *.egg-info /tmp/ai-daily-report-wheel-smoke
```

Expected:

```bash
git status --short
```

does not show compiled Rust binaries under `src/ai_daily_report/rust_bins/`.

- [ ] **Step 7: Commit Task 7 if packaging config changed**

If Task 7 required changing packaging files, commit them:

```bash
git add pyproject.toml setup.py
git commit -m "Build platform wheel with bundled binary data"
```

If no packaging files changed, do not create an empty commit.

---

### Task 8: Add GitHub Actions Release Workflow

**Files:**
- Create: `.github/workflows/release-wheels.yml`

- [ ] **Step 1: Add workflow**

Create `.github/workflows/release-wheels.yml`:

```yaml
name: Build release wheels

on:
  workflow_dispatch:
  push:
    tags:
      - "v*.*.*"

permissions:
  contents: write

jobs:
  build-wheel:
    name: Build ${{ matrix.platform }} wheel
    runs-on: ${{ matrix.os }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - os: ubuntu-latest
            platform: linux-x86_64
            wheel_platform: linux_x86_64
            discovery_bin: ai-daily-discovery
            office_bin: ai-daily-office-parser
          - os: windows-latest
            platform: win-amd64
            wheel_platform: win_amd64
            discovery_bin: ai-daily-discovery.exe
            office_bin: ai-daily-office-parser.exe

    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Set up Python
        uses: actions/setup-python@v5
        with:
          python-version: "3.11"

      - name: Set up Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Install Python build tools
        run: |
          python -m pip install --upgrade pip
          python -m pip install wheel setuptools

      - name: Test and build Rust discovery
        shell: bash
        run: |
          cd rust/discovery
          cargo test
          cargo build --release

      - name: Test and build Rust office parser
        shell: bash
        run: |
          cd rust/office_parser
          cargo test
          cargo build --release

      - name: Copy Rust binaries into package
        shell: bash
        run: |
          mkdir -p "src/ai_daily_report/rust_bins/${{ matrix.platform }}"
          cp "rust/discovery/target/release/${{ matrix.discovery_bin }}" "src/ai_daily_report/rust_bins/${{ matrix.platform }}/${{ matrix.discovery_bin }}"
          cp "rust/office_parser/target/release/${{ matrix.office_bin }}" "src/ai_daily_report/rust_bins/${{ matrix.platform }}/${{ matrix.office_bin }}"
          if [ "${{ matrix.platform }}" = "linux-x86_64" ]; then
            chmod 755 "src/ai_daily_report/rust_bins/${{ matrix.platform }}/${{ matrix.discovery_bin }}"
            chmod 755 "src/ai_daily_report/rust_bins/${{ matrix.platform }}/${{ matrix.office_bin }}"
          fi

      - name: Build wheel
        run: python setup.py bdist_wheel --plat-name ${{ matrix.wheel_platform }}

      - name: Install wheel for smoke test
        shell: bash
        run: |
          python -m pip install dist/*.whl
          ai-daily-report --version
          ai-daily-report --help
          ai-daily-report doctor --no-api
          python -c "from ai_daily_report.core.rust_binaries import describe_rust_binaries; print(describe_rust_binaries())"

      - name: Upload wheel artifact
        uses: actions/upload-artifact@v4
        with:
          name: ai-daily-report-${{ matrix.platform }}-wheel
          path: dist/*.whl

      - name: Upload wheel to GitHub Release
        if: startsWith(github.ref, 'refs/tags/')
        uses: softprops/action-gh-release@v2
        with:
          name: ver 0.5
          files: dist/*.whl
```

- [ ] **Step 2: Validate workflow syntax locally where possible**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python - <<'PY'
from pathlib import Path
import yaml

path = Path(".github/workflows/release-wheels.yml")
payload = yaml.safe_load(path.read_text(encoding="utf-8"))
assert payload["name"] == "Build release wheels"
assert "build-wheel" in payload["jobs"]
PY
```

Expected: command exits 0.

- [ ] **Step 3: Commit Task 8**

Run:

```bash
git add .github/workflows/release-wheels.yml
git commit -m "Add release wheel workflow"
```

---

### Task 9: Update Version, Documentation, And Release Guidance

**Files:**
- Modify: `README.md`
- Modify: `AGENTS.md`
- Modify: `CLAUDE.md`
- Modify: `docs/scanner-backends.md`
- Modify: `config/settings.example.yaml`
- Modify: `src/ai_daily_report/config/settings.example.yaml`
- Test: version search

- [ ] **Step 1: Update visible version strings**

Replace current outward-facing `5.0` / `5.0.0` references with `0.5` / `0.5.0` in:

```text
README.md
AGENTS.md
CLAUDE.md
config/settings.example.yaml
src/ai_daily_report/config/settings.example.yaml
src/ai_daily_report/main.py
```

Use:

```text
审计日报生成器 v0.5
version: "0.5.0"
```

- [ ] **Step 2: Add Release install section to README**

Add this section to `README.md`:

```text
## Release Wheel 安装

普通使用者不需要安装 Rust，也不需要运行 GitHub Actions。维护者发布 `v0.5.0` 后，到 GitHub Release `ver 0.5` 下载对应平台 wheel：

- Linux x86_64: `ai_daily_report-0.5.0-py3-none-linux_x86_64.whl`
- Windows amd64: `ai_daily_report-0.5.0-py3-none-win_amd64.whl`

安装：

    pip install ai_daily_report-0.5.0-py3-none-linux_x86_64.whl
    ai-daily-report --help
    ai-daily-report doctor --no-api

Windows 用户安装对应 `win_amd64` wheel。安装后的 Rust discovery 和 Office parser 来自 wheel 内置二进制；只有开发者需要手工运行 `cargo build`。
```

- [ ] **Step 3: Run current-version search**

Run:

```bash
rg -n -P "(?<!0\\.)\\b5[.]0(?:[.]0)?\\b|\\bv5[.]0(?:[.]0)?\\b|审计日报生成器\\x20v5" README.md AGENTS.md CLAUDE.md config src docs/scanner-backends.md
```

Expected: no matches in outward-facing current docs and runtime files. Historical `docs/superpowers/*` files are intentionally excluded from this command.

- [ ] **Step 4: Run docs/package focused tests**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_package_entrypoints.py tests/test_config.py tests/test_healthcheck.py -q
git diff --check
```

Expected: tests pass and whitespace check passes.

- [ ] **Step 5: Commit Task 9**

Run:

```bash
git add README.md AGENTS.md CLAUDE.md docs/scanner-backends.md config/settings.example.yaml src/ai_daily_report/config/settings.example.yaml src/ai_daily_report/main.py
git commit -m "Document v0.5 wheel release process"
```

---

### Task 10: Final Verification And Release Dry Run

**Files:**
- No source changes expected unless verification exposes a defect.

- [ ] **Step 1: Run Rust verification**

Run:

```bash
cd rust/discovery && cargo test && cargo build --release
cd ../office_parser && cargo test && cargo build --release
cd ../..
```

Expected: both Rust crates test and build successfully.

- [ ] **Step 2: Run Python verification**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests -q
/home/george/miniconda3/bin/conda run -n test python -m compileall main.py src tests
git diff --check
```

Expected:

- pytest reports all tests pass.
- compileall exits 0.
- diff check exits 0.

- [ ] **Step 3: Build and smoke test Linux wheel**

Run:

```bash
mkdir -p src/ai_daily_report/rust_bins/linux-x86_64
cp rust/discovery/target/release/ai-daily-discovery src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-discovery
cp rust/office_parser/target/release/ai-daily-office-parser src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-office-parser
chmod 755 src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-discovery src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-office-parser
rm -rf build dist *.egg-info
/home/george/miniconda3/bin/conda run -n test python setup.py bdist_wheel --plat-name linux_x86_64
python -m venv /tmp/ai-daily-report-final-wheel
/tmp/ai-daily-report-final-wheel/bin/python -m pip install --upgrade pip
/tmp/ai-daily-report-final-wheel/bin/python -m pip install dist/ai_daily_report-0.5.0-py3-none-linux_x86_64.whl
/tmp/ai-daily-report-final-wheel/bin/ai-daily-report --version
/tmp/ai-daily-report-final-wheel/bin/ai-daily-report --help
/tmp/ai-daily-report-final-wheel/bin/ai-daily-report doctor --no-api
/tmp/ai-daily-report-final-wheel/bin/python -c "from ai_daily_report.core.rust_binaries import describe_rust_binaries; print(describe_rust_binaries())"
```

Expected:

- wheel file exists with version `0.5.0`.
- installed command prints version `0.5.0`.
- resolver reports bundled Linux binaries available.

- [ ] **Step 4: Clean local wheel artifacts**

Run:

```bash
rm -f src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-discovery
rm -f src/ai_daily_report/rust_bins/linux-x86_64/ai-daily-office-parser
rm -rf build dist *.egg-info /tmp/ai-daily-report-final-wheel
```

Expected:

```bash
git status --short
```

does not show compiled binary artifacts.

- [ ] **Step 5: Push branch and create release tag after user approval**

Only after user approval for release:

```bash
git status --short
git tag v0.5.0
git push origin main
git push origin v0.5.0
```

Expected: GitHub Actions creates Release `ver 0.5` with Linux and Windows wheel assets.

- [ ] **Step 6: Post-release verification**

After GitHub Actions finishes, download both assets from Release `ver 0.5` and verify:

```bash
ls -lh ai_daily_report-0.5.0-py3-none-linux_x86_64.whl
```

On Linux:

```bash
python -m venv /tmp/ai-daily-report-release-check
/tmp/ai-daily-report-release-check/bin/python -m pip install ai_daily_report-0.5.0-py3-none-linux_x86_64.whl
/tmp/ai-daily-report-release-check/bin/ai-daily-report --version
/tmp/ai-daily-report-release-check/bin/ai-daily-report doctor --no-api
```

Expected: installed Release wheel runs without Rust installed.

---

## Final Acceptance Checklist

- [ ] Spec `docs/superpowers/specs/2026-05-26-python-wheel-rust-binaries-design.md` is implemented.
- [ ] Package version is `0.5.0`.
- [ ] Git tag plan is `v0.5.0`.
- [ ] GitHub Release title is `ver 0.5`.
- [ ] `ai-daily-report --version` prints `0.5.0`.
- [ ] Linux wheel contains Linux Rust binaries.
- [ ] Windows wheel contains Windows Rust `.exe` binaries.
- [ ] Users do not need Rust to install and run smoke commands.
- [ ] Discovery fallback to Python remains intact.
- [ ] Office parser fallback remains intact.
- [ ] `.xlsx` still reports `rust_xlsx_bounded_v1`.
- [ ] `doctor --no-api` works for CI smoke tests.
- [ ] Root `main.py` still delegates to package CLI.
- [ ] Templates load from package resources.
- [ ] Current outward-facing version docs use `0.5` / `0.5.0`.
- [ ] Full pytest suite passes.
- [ ] Rust crate tests pass.
- [ ] Wheel smoke test passes.
- [ ] GitHub Actions uploads Release assets on tag.

## Self-Review

- Spec coverage: tasks cover package structure, resource loading, runtime config, Rust binary resolution, runner integration, stable parser profile, doctor no-api, platform wheel build, GitHub Release upload, version `0.5.0`, and final verification.
- Placeholder scan: this plan contains no placeholder markers, no empty deferred-work steps, and every code-changing step includes concrete code or exact commands.
- Type consistency: resolver names are consistent across tasks: `BinaryResolution`, `map_platform_key`, `binary_filename`, `resolve_rust_binary`, `rust_binary_profile_value`, and `describe_rust_binaries`.
