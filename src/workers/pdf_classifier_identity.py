"""Stdlib-only PDF classifier identity for live capability preflight."""

from __future__ import annotations

import json
import os
import sys
import unicodedata

try:
    from _sha2 import sha256 as _sha256
except ImportError:  # Keep hashing available without third-party imports.
    try:
        from _sha256 import sha256 as _sha256
    except ImportError:
        from _hashlib import openssl_sha256 as _sha256


POLICY_VERSION = "pdf_text_presence_v1"
CLASSIFIER_CONTRACT_VERSION = "ai_daily_pdf_classifier_v1"
CLASSIFIER_PROTOCOL_VERSION = 1
_CLASSIFIER_DOMAIN = b"classifier-build-v1\0"

CLASSIFIER_BUILD_INPUTS = (
    "requirements.lock",
    "src/models/scanner_contract.py",
    "src/workers/document_parser_worker.py",
    "src/workers/pdf_classifier.py",
    "src/workers/pdf_classifier_identity.py",
)


def _installed_version_json(package: str) -> dict[str, object]:
    """Read wheel metadata without importing the package or its native DLL."""
    roots = [entry for entry in sys.path if entry]
    venv_root = os.path.dirname(os.path.dirname(sys.executable))
    roots.extend(
        [
            os.path.join(venv_root, "Lib", "site-packages"),
            os.path.join(
                venv_root,
                "lib",
                f"python{sys.version_info.major}.{sys.version_info.minor}",
                "site-packages",
            ),
        ]
    )
    for root in roots:
        candidate = os.path.join(root, package, "version.json")
        if os.path.isfile(candidate):
            with open(candidate, encoding="utf-8") as source:
                payload = json.load(source)
            break
    else:
        raise RuntimeError(f"{package} version metadata is missing")
    if not isinstance(payload, dict):
        raise RuntimeError(f"{package} version metadata is invalid")
    return payload


def _pdfium_native_version() -> str:
    payload = _installed_version_json("pypdfium2_raw")
    tag = ".".join(
        str(payload[field]) for field in ("major", "minor", "build", "patch")
    )
    suffixes: list[str] = []
    if int(payload.get("n_commits", 0)) > 0:
        suffixes.extend(
            [str(payload["n_commits"]), str(payload.get("hash"))]
        )
    description = f"+{'.'.join(suffixes)}" if suffixes else ""
    flags = payload.get("flags") or []
    if flags:
        description += f":{','.join(str(flag) for flag in flags)}"
    origin = str(payload.get("origin", ""))
    if origin != "pdfium-binaries":
        description += f"@{origin}"
    return tag + description


def _pypdfium2_version() -> str:
    payload = _installed_version_json("pypdfium2")
    tag = ".".join(str(payload[field]) for field in ("major", "minor", "patch"))
    if payload.get("beta") is not None:
        tag += f"b{payload['beta']}"
    suffixes: list[str] = []
    if int(payload.get("n_commits", 0)) > 0:
        suffixes.extend(
            [str(payload["n_commits"]), str(payload.get("hash"))]
        )
    if payload.get("dirty"):
        suffixes.append("dirty")
    description = f"+{'.'.join(suffixes)}" if suffixes else ""
    data_source = str(payload.get("data_source", ""))
    if data_source != "git":
        description += f":{data_source}"
    if payload.get("is_editable"):
        description += "@editable"
    return tag + description


def _python_version() -> str:
    return ".".join(str(part) for part in sys.version_info[:3])


def _target_triple() -> str:
    if sys.platform == "win32":
        arch = os.environ.get("PROCESSOR_ARCHITECTURE", "").lower()
        if not arch:
            raise RuntimeError("Windows processor architecture is unavailable")
        return f"{arch}-pc-windows-msvc"
    arch = os.uname().machine.lower()
    if sys.platform == "darwin":
        return f"{arch}-apple-darwin"
    return f"{arch}-unknown-linux-gnu"


def _compute_classifier_build() -> str:
    repository_root = __file__.replace("\\", "/").rsplit("/", 3)[0]
    digest = _sha256()
    digest.update(_CLASSIFIER_DOMAIN)
    for relative_path in CLASSIFIER_BUILD_INPUTS:
        path_bytes = relative_path.encode("utf-8", errors="strict")
        with open(f"{repository_root}/{relative_path}", "rb") as source:
            file_bytes = source.read()
        digest.update(len(path_bytes).to_bytes(8, "little"))
        digest.update(path_bytes)
        digest.update(len(file_bytes).to_bytes(8, "little"))
        digest.update(file_bytes)
    metadata = {
        "policy_version": POLICY_VERSION,
        "python_implementation": sys.implementation.name,
        "python_version": _python_version(),
        "unicode_data_version": unicodedata.unidata_version,
        "pypdfium2_version": _pypdfium2_version(),
        "pdfium_version": _pdfium_native_version(),
        "target_triple": _target_triple(),
    }
    canonical = json.dumps(
        metadata,
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    ).encode("utf-8", errors="strict")
    digest.update(len(canonical).to_bytes(8, "little"))
    digest.update(canonical)
    return digest.hexdigest()


CLASSIFIER_BUILD = _compute_classifier_build()


def classifier_version_payload() -> dict[str, object]:
    return {
        "contract": "ai_daily_pdf_classifier",
        "protocol_version": CLASSIFIER_PROTOCOL_VERSION,
        "classifier_contract_version": CLASSIFIER_CONTRACT_VERSION,
        "classifier_build": CLASSIFIER_BUILD,
        "policy_version": POLICY_VERSION,
        "python_implementation": sys.implementation.name,
        "python_version": _python_version(),
        "unicode_data_version": unicodedata.unidata_version,
        "pypdfium2_version": _pypdfium2_version(),
        "pdfium_version": _pdfium_native_version(),
        "target_triple": _target_triple(),
    }


_CLASSIFIER_VERSION_JSON = json.dumps(
    classifier_version_payload(),
    ensure_ascii=False,
    separators=(",", ":"),
).encode("utf-8", errors="strict")


def classifier_version_json() -> bytes:
    return _CLASSIFIER_VERSION_JSON + b"\n"
