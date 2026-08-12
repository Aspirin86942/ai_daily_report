from __future__ import annotations

import ast
import hashlib
import json
from pathlib import Path

from src.workers.python_worker_identity import (
    PYTHON_WORKER_BUILD,
    PYTHON_WORKER_BUILD_INPUTS,
)


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_PATH = REPOSITORY_ROOT / "tests/fixtures/python_worker_build_inputs.json"


def _independent_fingerprint(inputs: list[str]) -> str:
    digest = hashlib.sha256()
    for relative_path in inputs:
        path_bytes = relative_path.encode("utf-8", errors="strict")
        file_bytes = (REPOSITORY_ROOT / relative_path).read_bytes()
        digest.update(len(path_bytes).to_bytes(8, "little"))
        digest.update(path_bytes)
        digest.update(len(file_bytes).to_bytes(8, "little"))
        digest.update(file_bytes)
    return digest.hexdigest()


def _local_import_path(importing_file: str, node: ast.ImportFrom) -> str | None:
    if node.level == 0:
        module = node.module or ""
    else:
        package_parts = Path(importing_file).with_suffix("").parts[:-1]
        keep = len(package_parts) - node.level + 1
        module_parts = list(package_parts[:keep])
        if node.module:
            module_parts.extend(node.module.split("."))
        module = ".".join(module_parts)
    if not module.startswith("src."):
        return None
    candidate = module.replace(".", "/") + ".py"
    return candidate if (REPOSITORY_ROOT / candidate).is_file() else None


def test_python_worker_build_allowlist_is_frozen_and_reproducible() -> None:
    fixture = json.loads(FIXTURE_PATH.read_text(encoding="utf-8"))
    inputs = fixture["inputs"]

    assert fixture["algorithm"] == "sha256-python-worker-build-v1"
    assert inputs == sorted(inputs)
    assert tuple(inputs) == PYTHON_WORKER_BUILD_INPUTS
    assert PYTHON_WORKER_BUILD == _independent_fingerprint(inputs)
    assert len(PYTHON_WORKER_BUILD) == 64


def test_every_direct_local_worker_parser_import_is_fingerprinted() -> None:
    fingerprinted = set(PYTHON_WORKER_BUILD_INPUTS)
    parser_entrypoints = {
        "src/workers/contracts.py",
        "src/workers/document_parser_worker.py",
        "src/services/document_parser.py",
    }
    imported_sources: set[str] = set()
    for relative_path in parser_entrypoints:
        tree = ast.parse(
            (REPOSITORY_ROOT / relative_path).read_text(encoding="utf-8"),
            filename=relative_path,
        )
        for node in ast.walk(tree):
            if isinstance(node, ast.ImportFrom):
                imported = _local_import_path(relative_path, node)
                if imported:
                    imported_sources.add(imported)

    assert imported_sources <= fingerprinted
