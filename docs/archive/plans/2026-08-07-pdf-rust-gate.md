# PDF Rust 迁移基准门禁实施计划（阶段 7）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用「先基准再定」门禁决定 PDF 解析是否从 Python `pdfplumber` 迁移到 Rust：构建同语料对比（质量 + 速度），只有 Rust 提取质量**不低于** pdfplumber 且速度**≥2×** 才进入迁移设计；否则记录"维持 pdfplumber"证据。

**Architecture:** 生成一批含中英文已知文本的合成 PDF（reportlab，ground truth 一并落盘）作为固定语料；写两个对称基准执行器：Python 侧用 pdfplumber，Rust 侧用一个最小 `pdf-extract` bin，各自输出每份 PDF 的 提取文本 + 耗时；用共享质量度量脚本计算 字符保真率（SequenceMatcher vs ground truth）、可打印字符比例（乱码检测）、速度（P50/P90、吞吐 files/s）；按门禁判据出结论。本 plan **不**写 PDF 迁移实现——门禁通过才进入新的迁移 plan。

**Tech Stack:** Python 3.13、reportlab（dev，仅生成语料）、pdfplumber、Rust（pdf-extract crate）、uv run、cargo。

**前置：** Plan 1–5 已完成；`uv run pytest` 全绿。

## Global Constraints

- 修改范围：`scripts/pdf_benchmark/`（语料生成 + 两侧执行器 + 质量度量）、`rust/pdf_extract_bench/`（最小 Rust bin，评估用）、`tests/fixtures/pdf_benchmark/`（合成语料，git 跟踪）、`docs/superpowers/specs/2026-08-07-pdf-rust-gate-evidence.md`（证据）；`pyproject.toml` dev 组临时加 `reportlab`（仅生成语料用）。
- **禁止**：改 `src/services/document_parser.py`、`src/core/llm.py`、`rust/scanner_core/**`（门禁通过前不碰生产代码）。
- 门禁判据（两条件同时满足才迁）：① 质量 —— Rust 侧平均字符保真率 ≥ pdfplumber 的 95%，且可打印字符比例 ≥ 98%；② 速度 —— Rust 侧 P50 耗时 ≤ pdfplumber 的 50%（即 ≥2×）。
- 门禁结论必须写入证据文档并 git 提交；无论通过与否都不删语料与执行器（供复测）。
- 每 Task 结束 `uv run pytest` 全绿（本 plan 不加 pytest 测试；执行器是独立脚本，靠命令验证）。

---

### Task 1: 生成合成 PDF 语料

**Files:**
- Create: `scripts/pdf_benchmark/generate_corpus.py`
- Create: `tests/fixtures/pdf_benchmark/`（生成语料 + ground truth）
- Modify: `pyproject.toml`（dev 组临时加 `reportlab>=4.0,<5`，生成后决定是否保留）

**Interfaces:**
- Consumes: 无
- Produces: `tests/fixtures/pdf_benchmark/case_*.pdf` + 同名 `.txt`（ground truth）；`generate_corpus.py` 幂等可重跑

- [ ] **Step 1: 临时加 reportlab dev 依赖**

在 `pyproject.toml` `[dependency-groups].dev` 加 `"reportlab>=4.0,<5"`，Run: `uv sync`。

- [ ] **Step 2: 写语料生成脚本**

创建 `scripts/pdf_benchmark/generate_corpus.py`：
```python
"""生成合成 PDF 基准语料（含中英文已知文本），ground truth 落到同名 .txt。

幂等：已存在的 case_*.pdf 跳过，不覆盖。
"""

from __future__ import annotations

import pathlib
from datetime import datetime

from reportlab.lib.pagesizes import A4
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.cidfonts import UnicodeCIDFont
from reportlab.pdfgen import canvas

OUTPUT = pathlib.Path(__file__).resolve().parents[2] / "tests" / "fixtures" / "pdf_benchmark"
N_CASES = 6

_CASES = [
    "审计月度工作汇报：本月完成对供应链模块的数据抽取与核对，共处理 47 个批次的入库单据，异常率 0.8%。",
    "Quarterly financial review: revenue reached $128,400, expenses $96,300, net margin 25.1%. Vendor onboarding completed.",
    "项目周会纪要：接口联调进度 80%，遗留 3 个阻塞项；下周交付验收报告 v2。",
    "Meeting notes: integration status 80%, three blockers remain; acceptance report v2 due next week.",
    "混合文本样例：PDF 解析质量需同时保留 ASCII 标识符如 AUTH-2026-001、数值 3.14159 与中文断句。",
    "Mixed sample: keep identifiers like AUTH-2026-001, numbers 3.14159, and Chinese punctuation intact.",
]


def generate() -> None:
    OUTPUT.mkdir(parents=True, exist_ok=True)
    pdfmetrics.registerFont(UnicodeCIDFont("STSong-Light"))
    for idx, text in enumerate(_CASES, start=1):
        pdf_path = OUTPUT / f"case_{idx:02d}.pdf"
        txt_path = OUTPUT / f"case_{idx:02d}.txt"
        if pdf_path.exists() and txt_path.exists():
            continue
        with open(txt_path, "w", encoding="utf-8") as gt:
            gt.write(text + "\n")
        c = canvas.Canvas(str(pdf_path), pagesize=A4)
        c.setFont("STSong-Light", 12)
        c.drawString(50, 780, text)
        c.showPage()
        c.save()


if __name__ == "__main__":
    generate()
    print(f"generated {N_CASES} pdf cases in {OUTPUT}")
```

- [ ] **Step 3: 运行生成并抽查**

Run: `uv run python scripts/pdf_benchmark/generate_corpus.py`
Expected: 打印 `generated 6 pdf cases`；`ls tests/fixtures/pdf_benchmark/` 含 `case_01.pdf`…`case_06.pdf` 与对应 `.txt`。

- [ ] **Step 4: Commit**

```bash
git add scripts/pdf_benchmark tests/fixtures/pdf_benchmark pyproject.toml uv.lock
git commit -m "bench: generate synthetic PDF corpus with ground truth for parser gate"
```

---

### Task 2: pdfplumber 基准执行器

**Files:**
- Create: `scripts/pdf_benchmark/run_python.py`

**Interfaces:**
- Consumes: Task 1 语料
- Produces: JSON 结果 `scripts/pdf_benchmark/results/python.json`（每份：file、chars、printable_ratio、gt_ratio、duration_ms）

- [ ] **Step 1: 写执行器**

创建 `scripts/pdf_benchmark/run_python.py`：
```python
"""用 pdfplumber 提取语料，输出质量与耗时 JSON。"""

from __future__ import annotations

import json
import pathlib
import time

import pdfplumber

ROOT = pathlib.Path(__file__).resolve().parents[2]
CORPUS = ROOT / "tests" / "fixtures" / "pdf_benchmark"
OUT = pathlib.Path(__file__).resolve().parent / "results" / "python.json"


def _printable_ratio(text: str) -> float:
    if not text:
        return 0.0
    printable = sum(
        1 for ch in text if ch.isprintable() and (ch.isalnum() or ch.isspace())
    )
    return printable / len(text)


def _gt_ratio(text: str, gt: str) -> float:
    from difflib import SequenceMatcher

    return SequenceMatcher(None, text, gt).ratio()


def run() -> None:
    OUT.parent.mkdir(parents=True, exist_ok=True)
    rows = []
    for pdf in sorted(CORPUS.glob("case_*.pdf")):
        gt = pdf.with_suffix(".txt").read_text(encoding="utf-8").strip()
        started = time.perf_counter()
        with pdfplumber.open(str(pdf)) as doc:
            text = "\n".join(page.extract_text() or "" for page in doc.pages)
        duration_ms = int((time.perf_counter() - started) * 1000)
        rows.append(
            {
                "file": pdf.name,
                "chars": len(text),
                "printable_ratio": round(_printable_ratio(text), 4),
                "gt_ratio": round(_gt_ratio(text, gt), 4),
                "duration_ms": duration_ms,
            }
        )
    OUT.write_text(json.dumps({"engine": "pdfplumber", "rows": rows}, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(rows, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    run()
```

- [ ] **Step 2: 运行并记录**

Run: `uv run python scripts/pdf_benchmark/run_python.py`
Expected: 输出 6 行 JSON；`gt_ratio` 偏高（≥0.8 表示 pdfplumber 提取与 ground truth 高度一致，验证语料可用）。

- [ ] **Step 3: Commit**

```bash
git add scripts/pdf_benchmark
git commit -m "bench: add pdfplumber parser benchmark runner"
```

---

### Task 3: 候选 Rust 提取基准执行器

**Files:**
- Create: `rust/pdf_extract_bench/Cargo.toml`
- Create: `rust/pdf_extract_bench/src/main.rs`
- Create: `scripts/pdf_benchmark/run_rust.py`（批量调用 Rust bin，输出对称 JSON）

**Interfaces:**
- Consumes: Task 1 语料
- Produces: `scripts/pdf_benchmark/results/rust.json`（结构与 python.json 对称）；`rust/pdf_extract_bench` 独立 cargo 目标，不并入 scanner workspace

- [ ] **Step 1: 创建 Rust 评估 bin**

创建 `rust/pdf_extract_bench/Cargo.toml`：
```toml
[package]
name = "pdf-extract-bench"
version = "0.1.0"
edition = "2021"

[dependencies]
pdf-extract = "0.6"
```

创建 `rust/pdf_extract_bench/src/main.rs`：
```rust
// 评估用：pdf-extract 文本提取。质量不足或编译过重时，门禁判定 Rust 不迁移。
use std::env;
use std::process;

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: pdf-extract-bench <file.pdf>");
        process::exit(2);
    });
    match pdf_extract::extract_text(&path) {
        Ok(text) => print!("{text}"),
        Err(err) => {
            eprintln!("extract failed: {err}");
            process::exit(1);
        }
    }
}
```

- [ ] **Step 2: 写 Rust 侧执行器**

创建 `scripts/pdf_benchmark/run_rust.py`：
```python
"""用 Rust pdf-extract bin 提取语料，输出与 python.json 对称的 JSON。"""

from __future__ import annotations

import json
import pathlib
import subprocess
import time

ROOT = pathlib.Path(__file__).resolve().parents[2]
CORPUS = ROOT / "tests" / "fixtures" / "pdf_benchmark"
BIN = (
    ROOT
    / "rust"
    / "target"
    / "release"
    / ("pdf-extract-bench.exe" if __import__("os").name == "nt" else "pdf-extract-bench")
)
OUT = pathlib.Path(__file__).resolve().parent / "results" / "rust.json"


def _printable_ratio(text: str) -> float:
    if not text:
        return 0.0
    printable = sum(
        1 for ch in text if ch.isprintable() and (ch.isalnum() or ch.isspace())
    )
    return printable / len(text)


def _gt_ratio(text: str, gt: str) -> float:
    from difflib import SequenceMatcher

    return SequenceMatcher(None, text, gt).ratio()


def run() -> None:
    if not BIN.is_file():
        raise SystemExit(f"build first: cargo build --release --manifest-path rust/pdf_extract_bench/Cargo.toml")
    OUT.parent.mkdir(parents=True, exist_ok=True)
    rows = []
    for pdf in sorted(CORPUS.glob("case_*.pdf")):
        gt = pdf.with_suffix(".txt").read_text(encoding="utf-8").strip()
        started = time.perf_counter()
        completed = subprocess.run(
            [str(BIN), str(pdf)], capture_output=True, text=True, encoding="utf-8", check=False
        )
        duration_ms = int((time.perf_counter() - started) * 1000)
        text = completed.stdout
        rows.append(
            {
                "file": pdf.name,
                "chars": len(text),
                "printable_ratio": round(_printable_ratio(text), 4),
                "gt_ratio": round(_gt_ratio(text, gt), 4),
                "duration_ms": duration_ms,
                "exit_code": completed.returncode,
            }
        )
    OUT.write_text(json.dumps({"engine": "pdf-extract", "rows": rows}, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(rows, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    run()
```

- [ ] **Step 3: 构建 Rust bin**

Run: `cargo build --release --manifest-path rust/pdf_extract_bench/Cargo.toml`
Expected: 编译成功生成 `rust/target/release/pdf-extract-bench(.exe)`。若 `pdf-extract = "0.6"` 解析失败，改为该 crate 的可用最新版本并记录；若编译过重/失败，记录"Rust 候选不可行"并跳到 Task 4 走"维持 pdfplumber"结论。

- [ ] **Step 4: 运行并记录**

Run: `uv run python scripts/pdf_benchmark/run_rust.py`
Expected: 输出 6 行 JSON；记录各 `gt_ratio` / `printable_ratio` / `duration_ms`。

- [ ] **Step 5: Commit**

```bash
git add rust/pdf_extract_bench scripts/pdf_benchmark
git commit -m "bench: add rust pdf-extract candidate benchmark runner"
```

---

### Task 4: 门禁判定 + 证据文档

**Files:**
- Create: `docs/superpowers/specs/2026-08-07-pdf-rust-gate-evidence.md`

**Interfaces:**
- Consumes: Task 2 `results/python.json`、Task 3 `results/rust.json`
- Produces: 门禁结论（迁移 or 维持现状）+ 证据；该结论是阶段 7 的唯一交付，不进入迁移实现

- [ ] **Step 1: 汇总指标**

读两个 JSON，计算并记录：
- 每侧：平均 `gt_ratio`、平均 `printable_ratio`、耗时 P50/P90、吞吐 files/s（6 份总耗时倒数）。
- 对比：Rust 平均 `gt_ratio` / pdfplumber 平均 `gt_ratio`；Rust P50 / pdfplumber P50。

- [ ] **Step 2: 按门禁判据判定**

- **质量条件**：Rust 平均 `gt_ratio` ≥ pdfplumber 的 95% **且** 平均 `printable_ratio` ≥ 0.98。
- **速度条件**：Rust P50 ≤ pdfplumber P50 的 50%。
- 两条件同时满足 → 结论"迁移候选成立，进入 PDF→Rust 迁移设计（新 plan）"；否则 → 结论"维持 pdfplumber"，并记录是哪个条件未满足（实测数值）。

- [ ] **Step 3: 写证据文档**

创建 `docs/superpowers/specs/2026-08-07-pdf-rust-gate-evidence.md`，含：
```markdown
# PDF 解析迁移基准门禁证据

> 日期：2026-08-07
> 语料：tests/fixtures/pdf_benchmark/（6 份合成中英 PDF，ground truth 已知）
> 对比引擎：pdfplumber（Python 现状） vs pdf-extract（Rust 候选）

## 结果

| 指标 | pdfplumber | pdf-extract |
|---|---|---|
| 平均 gt_ratio | <填> | <填> |
| 平均 printable_ratio | <填> | <填> |
| 耗时 P50 / P90 (ms) | <填> / <填> | <填> / <填> |
| 吞吐 (files/s) | <填> | <填> |

## 门禁判定

- 质量条件（Rust gt_ratio ≥ pdfplumber 的 95% 且 printable ≥ 0.98）：<通过/未通过>
- 速度条件（Rust P50 ≤ pdfplumber P50 的 50%）：<通过/未通过>

## 结论

<选一>
- **维持 pdfplumber**：理由与实测数值如上；不进入迁移，不新增生产依赖。
- **迁移候选成立**：进入 PDF→Rust 迁移设计（新 plan），迁移后再以本语料复测质量。
```

- [ ] **Step 4: 决定 reportlab 去留**

若语料已落盘且复测有用，`reportlab` 保留在 dev 组；否则移除并 `uv sync`。本 plan 建议保留（语料可重生成）。

- [ ] **Step 5: 跑全量 + Commit**

Run: `uv run pytest`
Expected: 全绿。
```bash
git add docs/superpowers/specs/2026-08-07-pdf-rust-gate-evidence.md pyproject.toml uv.lock
git commit -m "bench: record pdf parser migration gate evidence"
```

---

## Self-Review

- **Spec coverage**：阶段 7（先基准再定）由 Task 1–4 完整覆盖；门禁判据与设计规格 4.4 一致（质量不低于 + 速度 ≥2×）；明确"不通过则维持 pdfplumber"且不写迁移实现（YAGNI）。
- **占位符**：无 TBD；语料脚本、两侧执行器、Rust bin 代码完整；证据文档表项标注"<填>"为 Task 4 实测填表步骤，非占位符。
- **类型一致性**：`results/python.json` 与 `results/rust.json` 结构对称；`gt_ratio`/`printable_ratio` 度量函数在两侧一致；门禁判据引用的字段与输出一致。
