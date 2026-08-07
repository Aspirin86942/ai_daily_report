"""用 pdfplumber 提取固定语料并输出质量与耗时 JSON。"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from time import perf_counter_ns

import pdfplumber

from metrics import ground_truth_ratio, printable_ratio


ROOT = Path(__file__).resolve().parents[2]
CORPUS = ROOT / "tests" / "fixtures" / "pdf_benchmark"
OUT = Path(__file__).resolve().parent / "results" / "python.json"


def extract_text(pdf_path: Path) -> str:
    """通过当前生产候选 pdfplumber 提取全部页面文本。"""
    with pdfplumber.open(str(pdf_path)) as document:
        return "\n".join(page.extract_text() or "" for page in document.pages)


def run() -> dict[str, object]:
    """运行基准并写出可审计的逐文件结果。"""
    pdf_paths = sorted(CORPUS.glob("case_*.pdf"))
    if not pdf_paths:
        raise RuntimeError(f"no PDF benchmark corpus found in {CORPUS}")

    rows: list[dict[str, object]] = []
    for pdf_path in pdf_paths:
        ground_truth = pdf_path.with_suffix(".txt").read_text(encoding="utf-8")
        started_ns = perf_counter_ns()
        text = extract_text(pdf_path)
        duration_ms = (perf_counter_ns() - started_ns) / 1_000_000
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
            }
        )

    result: dict[str, object] = {
        "schema_version": "pdf_parser_gate_v1",
        "engine": "pdfplumber",
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
