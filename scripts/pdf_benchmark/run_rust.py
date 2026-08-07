"""用 Rust pdf-extract 候选提取固定语料并输出对称 JSON。"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
from pathlib import Path
from time import perf_counter_ns

from metrics import ground_truth_ratio, printable_ratio


ROOT = Path(__file__).resolve().parents[2]
CORPUS = ROOT / "tests" / "fixtures" / "pdf_benchmark"
BIN = (
    ROOT
    / "rust"
    / "pdf_extract_bench"
    / "target"
    / "release"
    / ("pdf-extract-bench.exe" if os.name == "nt" else "pdf-extract-bench")
)
OUT = Path(__file__).resolve().parent / "results" / "rust.json"


def stderr_summary(raw_stderr: bytes) -> str:
    """保留候选错误事实，但移除本机 Cargo registry 路径和进程号。"""
    text = raw_stderr.decode("utf-8", errors="replace").strip()
    if not text:
        return ""
    for line in text.splitlines():
        if "unsupported encoding" in line:
            return line[line.index("unsupported encoding") :].strip()
        if line.startswith("extract failed:"):
            return line.strip()
    return "pdf-extract process failed without a portable error summary"


def run() -> dict[str, object]:
    """运行候选基准并写出可审计的逐文件结果。"""
    if not BIN.is_file():
        raise RuntimeError(
            "build first: cargo build --release --manifest-path "
            "rust/pdf_extract_bench/Cargo.toml"
        )
    pdf_paths = sorted(CORPUS.glob("case_*.pdf"))
    if not pdf_paths:
        raise RuntimeError(f"no PDF benchmark corpus found in {CORPUS}")

    rows: list[dict[str, object]] = []
    for pdf_path in pdf_paths:
        ground_truth = pdf_path.with_suffix(".txt").read_text(encoding="utf-8")
        started_ns = perf_counter_ns()
        completed = subprocess.run(
            [str(BIN), str(pdf_path)],
            capture_output=True,
            check=False,
        )
        duration_ms = (perf_counter_ns() - started_ns) / 1_000_000
        text = completed.stdout.decode("utf-8", errors="replace")
        stderr = stderr_summary(completed.stderr)
        rows.append(
            {
                "file": pdf_path.name,
                "text": text,
                "text_sha256": hashlib.sha256(text.encode("utf-8")).hexdigest(),
                "chars": len(text),
                "printable_ratio": round(printable_ratio(text), 6),
                "gt_ratio": round(
                    ground_truth_ratio(text, ground_truth),
                    6,
                ),
                "duration_ms": round(duration_ms, 3),
                "exit_code": completed.returncode,
                "stderr": stderr,
            }
        )

    result: dict[str, object] = {
        "schema_version": "pdf_parser_gate_v1",
        "engine": "pdf-extract",
        "rows": rows,
    }
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(
        json.dumps(result, ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
    return result


if __name__ == "__main__":
    print(json.dumps(run(), ensure_ascii=False, indent=2))
