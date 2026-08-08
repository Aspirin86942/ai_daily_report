"""生成 PDF 分类器数值门禁（spec Part 3.3）的固定 corpus 与 manifest。

幂等：fixture 与 manifest 都已存在时跳过，不覆盖既有证据。crypto 加密依赖
``pypdf``（仅生成期使用；提交后的 fixture 与测试不需要它）。所有 PDF 均
确定生成（reportlab invariant=1），text/no-text/error 与 category 记录在
``manifest.json``，供 ``tests/test_pdf_classifier.py`` 的数值门禁读取。
"""

from __future__ import annotations

import json
from pathlib import Path

from reportlab.lib.pagesizes import A4
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.cidfonts import UnicodeCIDFont
from reportlab.pdfgen import canvas

OUTPUT = (
    Path(__file__).resolve().parents[2]
    / "tests"
    / "fixtures"
    / "pdf_classifier"
)
MANIFEST = OUTPUT / "manifest.json"

CJK_FONT_NAME = "STSong-Light"
ASCII_FONT_NAME = "Helvetica"
FONT_SIZE = 12
LEFT_MARGIN = 50
TOP = 790
MAX_PAGES = 5
PAGE_COUNT_MAX = 6  # beyond-max_pages fixture 在 max_pages 之后才出现文字

CJK_SAMPLES = (
    "审计月度工作汇报：本月完成对供应链模块的数据抽取与核对。",
    "项目周会纪要：接口联调进度 80%，遗留 3 个阻塞项；下周交付验收报告 v2。",
    "财务对账单：期初余额 12,300.50 元，本期发生 47 笔，期末余额 8,901.20 元。",
)
ASCII_SAMPLES = (
    "Quarterly financial review: revenue reached $128,400 and expenses were $96,300.",
    "Vendor onboarding completed; acceptance report v2 due next week before the cutoff.",
    "Monthly metrics: 47 batches processed, 0.8% anomaly rate, 99.2% line-item match.",
    "Engineering standup: three blockers remain on the integration branch, owner Alex.",
    "Regression suite: 1,204 tests green, coverage 91.7%, worst case 412 ms per fixture.",
    "Release notes: version 5.0 ships the budget-aware scanner and PDF classifier.",
    "Purchase order PO-2026-118 covers 12 line items totalling 48,910.75 across three sites.",
    "Support handoff: ticket 4512 resolved as duplicate of 4489; SLA met at 96% for Q3.",
    "Inventory audit: 214 SKUs checked, 3 discrepancies found, all corrected same day.",
    "Payroll summary: 86 staff paid on time, 2 corrections, total disbursement 1,204,550.00.",
    "Marketing funnel: 12,840 impressions, 3.2% CTR, 411 leads, 38 opportunities closed.",
    "Compliance review: 9 policies updated, 142 employees re-trained, 0 findings outstanding.",
)
MIXED_SAMPLES = (
    "混合文本样例：保留 ASCII 标识符 AUTH-2026-001、数值 3.14159 与中文断句。",
    "Mixed sample: 审计编号 AUD-118、金额 ￥8,901.20 与中文 punctuation intact。",
    "季度报告：revenue $128,400、支出 ￥96,300，净利率 25.1%，完成率 99.2%。",
)
SPARSE_CHARS = ("a", "中", "1")


def _font_for(character: str) -> str:
    return ASCII_FONT_NAME if ord(character) < 128 else CJK_FONT_NAME


def _text_width(text: str, font_size: int = FONT_SIZE) -> float:
    return sum(
        pdfmetrics.stringWidth(character, _font_for(character), font_size)
        for character in text
    )


def _wrap_text(text: str, max_width: float, font_size: int = FONT_SIZE) -> list[str]:
    lines: list[str] = []
    current = ""
    for character in text:
        candidate = current + character
        if current and _text_width(candidate, font_size) > max_width:
            last_space = current.rfind(" ")
            if last_space > 0:
                lines.append(current[:last_space].rstrip())
                current = (current[last_space + 1 :] + character).lstrip()
            else:
                lines.append(current.rstrip())
                current = character.lstrip()
        else:
            current = candidate
    if current:
        lines.append(current.rstrip())
    return lines


def _draw_line(document: canvas.Canvas, line: str, y: float) -> None:
    x = LEFT_MARGIN
    run = ""
    run_font = ""
    for character in line:
        font_name = _font_for(character)
        if run and font_name != run_font:
            document.setFont(run_font, FONT_SIZE)
            document.drawString(x, y, run)
            x += pdfmetrics.stringWidth(run, run_font, FONT_SIZE)
            run = ""
        run_font = font_name
        run += character
    if run:
        document.setFont(run_font, FONT_SIZE)
        document.drawString(x, y, run)


def _draw_text_block(
    document: canvas.Canvas,
    text: str,
    *,
    rotate: bool = False,
    font_size: int = FONT_SIZE,
) -> None:
    document.saveState()
    if rotate:
        document.translate(A4[0] / 2, A4[1] / 2)
        document.rotate(90)
        document.translate(-A4[0] / 2, -A4[1] / 2)
    y = TOP
    for line in _wrap_text(text, A4[0] - 2 * LEFT_MARGIN, font_size):
        _draw_line(document, line, y)
        y -= 22
    document.restoreState()


def _draw_shapes(document: canvas.Canvas, seed: int) -> None:
    """用纯图形（无文本 operator）填充页面，模拟 image-only PDF。"""

    for index in range(seed % 5 + 2):
        x = 40 + (index * 97) % 420
        y = 60 + (index * 137) % 620
        style = (seed + index) % 4
        if style == 0:
            document.rect(x, y, 90, 60, fill=1, stroke=1)
        elif style == 1:
            document.circle(x + 45, y + 30, 40, fill=1, stroke=1)
        elif style == 2:
            document.line(x, y, x + 100, y + 80)
            document.line(x, y + 80, x + 100, y)
        else:
            points = [
                (x, y),
                (x + 90, y + 10),
                (x + 60, y + 70),
                (x + 10, y + 60),
            ]
            document.setLineWidth(2)
            for first, second in zip(points, points[1:] + points[:1]):
                document.line(*first, *second)
            document.setLineWidth(1)
    if seed % 3 == 0:
        document.roundRect(120, 420, 200, 140, 12, fill=1, stroke=1)
    if seed % 5 == 0:
        document.ellipse(300, 120, 420, 220, fill=1, stroke=1)
    if seed % 7 == 0:
        for step in range(6):
            document.arc(
                200,
                200,
                300 + step * 8,
                280 + step * 8,
                startAng=30,
                extent=120,
            )
    if seed % 11 == 0:
        document.bezier(
            60, 60, 120, 200, 260, 200, 320, 60,
        )
    document.setFillColorRGB(0.5, 0.6, 0.7)


def _make_text_pdf(path: Path, text: str, *, rotate: bool = False) -> None:
    document = canvas.Canvas(str(path), pagesize=A4, pageCompression=1, invariant=1)
    document.setTitle(f"pdf classifier text {path.stem}")
    document.setAuthor("ai-daily-report synthetic classifier corpus")
    _draw_text_block(document, text, rotate=rotate)
    document.showPage()
    document.save()


def _make_sparse_pdf(path: Path, character: str) -> None:
    document = canvas.Canvas(str(path), pagesize=A4, pageCompression=1, invariant=1)
    document.setTitle(f"pdf classifier sparse {path.stem}")
    document.setAuthor("ai-daily-report synthetic classifier corpus")
    document.setFont(_font_for(character), 12)
    document.drawString(300, 400, character)
    document.showPage()
    document.save()


def _make_mixed_pdf(path: Path, text: str) -> None:
    document = canvas.Canvas(str(path), pagesize=A4, pageCompression=1, invariant=1)
    document.setTitle(f"pdf classifier mixed {path.stem}")
    document.setAuthor("ai-daily-report synthetic classifier corpus")
    document.setFillColorRGB(0.85, 0.85, 0.85)
    document.rect(40, 40, 180, 120, fill=1, stroke=1)
    document.circle(500, 700, 50, fill=1, stroke=1)
    _draw_text_block(document, text)
    document.showPage()
    document.save()


def _make_hidden_ocr_pdf(path: Path, text: str) -> None:
    document = canvas.Canvas(str(path), pagesize=A4, pageCompression=1, invariant=1)
    document.setTitle(f"pdf classifier ocr {path.stem}")
    document.setAuthor("ai-daily-report synthetic classifier corpus")
    document.setFillColorRGB(1, 1, 1)  # 白字白底：隐藏文字层
    _draw_text_block(document, text)
    document.showPage()
    document.save()


def _make_blank_pdf(path: Path) -> None:
    document = canvas.Canvas(str(path), pagesize=A4, pageCompression=1, invariant=1)
    document.setTitle(f"pdf classifier blank {path.stem}")
    document.setAuthor("ai-daily-report synthetic classifier corpus")
    document.showPage()
    document.save()


def _make_beyond_max_pages_pdf(path: Path, text: str) -> None:
    document = canvas.Canvas(str(path), pagesize=A4, pageCompression=1, invariant=1)
    document.setTitle(f"pdf classifier beyond {path.stem}")
    document.setAuthor("ai-daily-report synthetic classifier corpus")
    for _ in range(PAGE_COUNT_MAX - 1):
        document.showPage()
    _draw_text_block(document, text)
    document.showPage()
    document.save()


def _make_image_only_pdf(path: Path, seed: int) -> None:
    document = canvas.Canvas(str(path), pagesize=A4, pageCompression=1, invariant=1)
    document.setTitle(f"pdf classifier image {path.stem}")
    document.setAuthor("ai-daily-report synthetic classifier corpus")
    _draw_shapes(document, seed)
    document.showPage()
    document.save()


def _make_encrypted_pdf(path: Path, plain_pdf: Path, password: str) -> None:
    from pypdf import PdfReader, PdfWriter

    reader = PdfReader(str(plain_pdf))
    writer = PdfWriter()
    writer.append_pages_from_reader(reader)
    writer.encrypt(password)
    with path.open("wb") as handle:
        writer.write(handle)


def _make_corrupt_pdf(path: Path, seed: int) -> None:
    header = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n"
    garbage = bytes((seed * 13 + index) % 256 for index in range(64))
    path.write_bytes(header + garbage)


def _entry_index(name: str, prefix: str) -> int:
    """从 ``prefixNN.pdf`` 文件名解析序号（prefix 后固定两位数字）。"""
    return int(name[len(prefix) : len(prefix) + 2])


def _build_entries() -> list[dict[str, object]]:
    entries: list[dict[str, object]] = []
    add = entries.append

    # ---- 窗口内 text：30 份 ----
    for index, text in enumerate(CJK_SAMPLES, start=1):
        add({"file": f"text_cjk_{index:02d}.pdf", "ground_truth": "text_in_parse_window", "max_pages": MAX_PAGES, "categories": ["cjk"]})
    for index, text in enumerate(ASCII_SAMPLES, start=1):
        add({"file": f"text_plain_{index:02d}.pdf", "ground_truth": "text_in_parse_window", "max_pages": MAX_PAGES, "categories": ["ascii"]})
    for index, text in enumerate(MIXED_SAMPLES, start=1):
        add({"file": f"text_mixed_{index:02d}.pdf", "ground_truth": "text_in_parse_window", "max_pages": MAX_PAGES, "categories": ["cjk", "mixed"]})
    for index, character in enumerate(SPARSE_CHARS, start=1):
        add({"file": f"text_sparse_{index:02d}.pdf", "ground_truth": "text_in_parse_window", "max_pages": MAX_PAGES, "categories": ["sparse"]})
    for index in range(1, 4):
        add({"file": f"text_rotated_{index:02d}.pdf", "ground_truth": "text_in_parse_window", "max_pages": MAX_PAGES, "categories": ["rotated"]})
    for index, text in enumerate(ASCII_SAMPLES[:3], start=1):
        add({"file": f"text_image_mixed_{index:02d}.pdf", "ground_truth": "text_in_parse_window", "max_pages": MAX_PAGES, "categories": ["mixed"]})
    for index, text in enumerate(CJK_SAMPLES[:3], start=1):
        add({"file": f"text_ocr_hidden_{index:02d}.pdf", "ground_truth": "text_in_parse_window", "max_pages": MAX_PAGES, "categories": ["ocr_hidden"]})

    # ---- 窗口内 no-text：100 份 ----
    for index in range(1, 4):
        add({"file": f"no_text_blank_{index:02d}.pdf", "ground_truth": "no_text_in_parse_window", "max_pages": MAX_PAGES, "categories": ["blank"]})
    for index, text in enumerate(ASCII_SAMPLES[:3], start=1):
        add({"file": f"no_text_beyond_max_pages_{index:02d}.pdf", "ground_truth": "no_text_in_parse_window", "max_pages": MAX_PAGES, "categories": ["beyond_max_pages"]})
    for index in range(1, 95):
        add({"file": f"no_text_image_{index:02d}.pdf", "ground_truth": "no_text_in_parse_window", "max_pages": MAX_PAGES, "categories": ["image_only"]})

    # ---- 确定性 error：6 份 ----
    for index in range(1, 4):
        add({"file": f"error_encrypted_{index:02d}.pdf", "ground_truth": "error", "max_pages": MAX_PAGES, "categories": ["encrypted"]})
    for index in range(1, 4):
        add({"file": f"error_corrupt_{index:02d}.pdf", "ground_truth": "error", "max_pages": MAX_PAGES, "categories": ["corrupt"]})

    return entries


def generate() -> tuple[int, int, int]:
    """生成缺失语料，返回 ``(生成数, 跳过数, 总数)``。"""
    if OUTPUT.exists():
        existing_pdfs = list(OUTPUT.glob("*.pdf"))
        if existing_pdfs and MANIFEST.exists():
            return 0, len(existing_pdfs), len(existing_pdfs)
    OUTPUT.mkdir(parents=True, exist_ok=True)
    pdfmetrics.registerFont(UnicodeCIDFont(CJK_FONT_NAME))

    entries = _build_entries()
    generated = 0
    for entry in entries:
        name = entry["file"]
        path = OUTPUT / name
        generated += 1
        if name.startswith("text_cjk_"):
            _make_text_pdf(path, CJK_SAMPLES[_entry_index(name, "text_cjk_") - 1])
        elif name.startswith("text_plain_"):
            _make_text_pdf(path, ASCII_SAMPLES[_entry_index(name, "text_plain_") - 1])
        elif name.startswith("text_mixed_"):
            _make_text_pdf(path, MIXED_SAMPLES[_entry_index(name, "text_mixed_") - 1])
        elif name.startswith("text_sparse_"):
            _make_sparse_pdf(path, SPARSE_CHARS[_entry_index(name, "text_sparse_") - 1])
        elif name.startswith("text_rotated_"):
            text = ASCII_SAMPLES[_entry_index(name, "text_rotated_") - 1]
            _make_text_pdf(path, text, rotate=True)
        elif name.startswith("text_image_mixed_"):
            text = ASCII_SAMPLES[_entry_index(name, "text_image_mixed_") - 1]
            _make_mixed_pdf(path, text)
        elif name.startswith("text_ocr_hidden_"):
            text = CJK_SAMPLES[_entry_index(name, "text_ocr_hidden_") - 1]
            _make_hidden_ocr_pdf(path, text)
        elif name.startswith("no_text_blank_"):
            _make_blank_pdf(path)
        elif name.startswith("no_text_beyond_max_pages_"):
            text = ASCII_SAMPLES[_entry_index(name, "no_text_beyond_max_pages_") - 1]
            _make_beyond_max_pages_pdf(path, text)
        elif name.startswith("no_text_image_"):
            _make_image_only_pdf(path, _entry_index(name, "no_text_image_"))
        elif name.startswith("error_encrypted_"):
            plain = OUTPUT / f"text_plain_{_entry_index(name, 'error_encrypted_'):02d}.pdf"
            _make_encrypted_pdf(path, plain, f"secret-{_entry_index(name, 'error_encrypted_')}")
        elif name.startswith("error_corrupt_"):
            _make_corrupt_pdf(path, _entry_index(name, "error_corrupt_"))

    manifest = {
        "algorithm": "pdf_classifier_corpus_v1",
        "policy_version": "pdf_text_presence_v1",
        "default_max_pages": MAX_PAGES,
        "entries": entries,
    }
    MANIFEST.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=False) + "\n",
        encoding="utf-8",
    )
    return generated, 0, len(entries)


if __name__ == "__main__":
    created, skipped, total = generate()
    print(f"generated {created} pdf fixtures, skipped {skipped}, total {total} in {OUTPUT}")
