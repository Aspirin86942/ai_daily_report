"""Bind an installed release to HEAD, the exact dirty tree and offline gate logs."""
from __future__ import annotations

import argparse
from hashlib import sha256
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from windows_release import sha256_file, verify_release_bundle


def git(*args) -> bytes:
    return subprocess.check_output(["git", "-c", "core.autocrlf=false", *args], cwd=ROOT, stderr=subprocess.DEVNULL)


def source_state(bundle: Path, output: Path, logs: list[Path]) -> dict:
    manifest = verify_release_bundle(bundle)
    head = git("rev-parse", "HEAD").decode("ascii").strip()
    if head != manifest["git_commit"]:
        raise ValueError("release manifest and current HEAD differ")
    tracked = git("ls-files", "-z").decode("utf-8").split("\0")
    untracked = git("ls-files", "--others", "--exclude-standard", "-z").decode("utf-8").split("\0")
    generated = {".artifacts", ".venv", "data", "logs", "target", "__pycache__", ".pytest_cache"}
    source_names = {name for name in tracked + untracked
                    if name and not generated.intersection(Path(name).parts)}
    hashes = {name: sha256_file(ROOT / name) for name in sorted(source_names)
              if (ROOT / name).is_file()}
    for record in manifest["files"]:
        if record["path"].startswith("app/"):
            name = record["path"][4:]
            if hashes.get(name) != record["sha256"]:
                raise ValueError("release application differs from current source: " + name)
    status = git("status", "--porcelain=v1", "-z")
    patch = git("diff", "--binary", "HEAD", "--")
    output.parent.mkdir(parents=True, exist_ok=True)
    patch_path = output.parent / "source.patch"
    patch_path.write_bytes(patch)
    gates = []
    for path in logs:
        data = path.read_bytes()
        content = data.decode("utf-8", errors="replace")
        if re.search(r"\b[1-9][0-9]* failed\b|error: could not compile|FAILED ", content):
            raise ValueError("failed gate log: " + path.name)
        gates.append({"path": str(path.absolute()), "sha256": sha256(data).hexdigest(),
            "pytest_passed": re.findall(r"([0-9]+) passed in", content),
            "rust_passed_total": sum(int(number) for number in re.findall(r"test result: ok\. ([0-9]+) passed", content))})
    value = {"version": 1, "head": head, "dirty": bool(status), "status_sha256": sha256(status).hexdigest(),
        "patch_sha256": sha256(patch).hexdigest(), "source_files": hashes,
        "untracked_files": {name: hashes[name] for name in untracked if name in hashes},
        "release_version": manifest["release_version"], "release_manifest_sha256": sha256_file(bundle / "manifest.json"),
        "native_build_identity": manifest["native"]["build_identity"], "gate_logs": gates,
        "model_calls": 0}
    output.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8", newline="\n")
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--gate-log", action="append", type=Path, default=[])
    args = parser.parse_args()
    state = source_state(args.bundle, args.output, args.gate_log)
    print(json.dumps({"output": str(args.output.absolute()), "head": state["head"], "dirty": state["dirty"],
        "source_files": len(state["source_files"]), "model_calls": 0}, ensure_ascii=False))


if __name__ == "__main__":
    main()
