import subprocess
from pathlib import Path


UV_VERSION = "0.12.0"
UV_EXPORT_ARGS = (
    "export",
    "--frozen",
    "--no-dev",
    "--no-emit-project",
    "--no-header",
    "--format",
    "requirements.txt",
)
DEV_DISTRIBUTIONS = {
    "pytest",
    "pytest-timeout",
    "pytest-xdist",
    "reportlab",
}
WORKFLOW_GATES = {
    ".github/workflows/ci.yml": 2,
    ".github/workflows/windows-release.yml": 1,
}
WINDOWS_PRODUCTION_WORKFLOWS = (
    ".github/workflows/ci.yml",
    ".github/workflows/windows-release.yml",
)


def _export_lock(root: Path, output_path: Path) -> bytes:
    subprocess.run(
        ["uv", *UV_EXPORT_ARGS, "--output-file", str(output_path)],
        cwd=root,
        capture_output=True,
        check=True,
    )
    return output_path.read_bytes()


def _requirement_blocks(lock_text: str) -> list[list[str]]:
    blocks: list[list[str]] = []
    current: list[str] = []
    for line in lock_text.splitlines():
        if line and not line[0].isspace() and not line.startswith("#"):
            if current:
                blocks.append(current)
            current = [line]
        elif current:
            current.append(line)
    if current:
        blocks.append(current)
    return blocks


def _distribution_name(requirement_line: str) -> str:
    return requirement_line.split("==", 1)[0].strip().lower().replace("_", "-")


def test_uv_export_toolchain_is_frozen() -> None:
    completed = subprocess.run(
        ["uv", "--version"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=True,
    )
    assert completed.stdout.split()[1] == UV_VERSION


def test_lock_regenerates_byte_identical(tmp_path: Path) -> None:
    root = Path(__file__).resolve().parents[1]
    expected = (root / "requirements.lock").read_bytes()
    generated = _export_lock(root, tmp_path / "requirements.generated.lock")
    assert generated == expected, "requirements.lock 与冻结 uv export 不一致"


def test_lock_has_hashes_and_excludes_dev_editable_and_project() -> None:
    root = Path(__file__).resolve().parents[1]
    lock_text = (root / "requirements.lock").read_text(encoding="utf-8")
    blocks = _requirement_blocks(lock_text)
    names = {_distribution_name(block[0]) for block in blocks}

    assert blocks
    assert all(any("--hash=sha256:" in line for line in block) for block in blocks)
    assert DEV_DISTRIBUTIONS.isdisjoint(names)
    assert "ai-daily-report" not in names
    assert "pypdfium2" in names
    assert not any(
        line.lstrip().startswith(("-e ", "--editable "))
        or " @ file:" in line.lower()
        for line in lock_text.splitlines()
    )


def test_ci_pins_uv_syncs_frozen_and_runs_the_projection_gate() -> None:
    root = Path(__file__).resolve().parents[1]
    for relative_path, expected_gate_count in WORKFLOW_GATES.items():
        workflow = (root / relative_path).read_text(encoding="utf-8")
        assert workflow.count("uses: astral-sh/setup-uv@v6") == expected_gate_count
        assert workflow.count(f'version: "{UV_VERSION}"') == expected_gate_count
        assert workflow.count("uv sync --frozen") == expected_gate_count
        assert "requirements-dev.txt" not in workflow
        assert (
            workflow.count("uv run pytest tests/test_requirements_lock.py -v")
            == expected_gate_count
        )


def test_windows_workflows_run_the_explicit_clean_production_chain() -> None:
    root = Path(__file__).resolve().parents[1]
    ordered_tokens = (
        "python -m venv $prodVenv",
        "& $prodPython -m pip install --requirement requirements.lock",
        "-m src.workers.document_parser_worker version",
        "-m src.workers.document_parser_worker session-version",
        "& $env:AI_DAILY_PROD_PYTHON main.py doctor --strict",
        "& $env:AI_DAILY_PROD_PYTHON scripts/corpus_gate.py",
    )
    for relative_path in WINDOWS_PRODUCTION_WORKFLOWS:
        workflow = (root / relative_path).read_text(encoding="utf-8")
        positions = []
        for token in ordered_tokens:
            assert workflow.count(token) == 1, f"{relative_path}: {token}"
            positions.append(workflow.index(token))
        assert positions == sorted(positions), (
            f"{relative_path}: production gate order"
        )
        assert "worker_build -ne $worker.worker_build" in workflow
        assert "--work-dir (Join-Path $gateRoot 'corpus')" in workflow
        assert "--out-root (Join-Path $gateRoot 'runs')" in workflow
        assert "--evidence (Join-Path $gateRoot 'evidence.json')" in workflow
