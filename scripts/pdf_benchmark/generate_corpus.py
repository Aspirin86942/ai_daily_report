"""生成含中英文 ground truth 的合成 PDF 基准语料。

幂等：同一 case 的 PDF 与文本都已存在时跳过，不覆盖既有证据。
"""

from __future__ import annotations

from pathlib import Path

from reportlab.lib.pagesizes import A4
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.cidfonts import UnicodeCIDFont
from reportlab.pdfgen import canvas


OUTPUT = (
    Path(__file__).resolve().parents[2]
    / "tests"
    / "fixtures"
    / "pdf_benchmark"
)
CJK_FONT_NAME = "STSong-Light"
ASCII_FONT_NAME = "Helvetica"
FONT_SIZE = 12
LEFT_MARGIN = 50
TOP = 790
LINE_HEIGHT = 22
MAX_WIDTH = A4[0] - 2 * LEFT_MARGIN

CASES = (
    "审计月度工作汇报：本月完成对供应链模块的数据抽取与核对，共处理 47 个批次的入库单据，异常率 0.8%。",
    "Quarterly financial review: revenue reached $128,400, expenses $96,300, net margin 25.1%. Vendor onboarding completed.",
    "项目周会纪要：接口联调进度 80%，遗留 3 个阻塞项；下周交付验收报告 v2。",
    "Meeting notes: integration status 80%, three blockers remain; acceptance report v2 due next week.",
    "混合文本样例：PDF 解析质量需同时保留 ASCII 标识符如 AUTH-2026-001、数值 3.14159 与中文断句。",
    "Mixed sample: keep identifiers like AUTH-2026-001, numbers 3.14159, and Chinese punctuation intact.",
)


def _font_for(character: str) -> str:
    return ASCII_FONT_NAME if ord(character) < 128 else CJK_FONT_NAME


def _text_width(text: str) -> float:
    """按实际字体计算混排文本宽度。"""
    return sum(
        pdfmetrics.stringWidth(character, _font_for(character), FONT_SIZE)
        for character in text
    )


def _wrap_text(text: str) -> list[str]:
    """按字体真实宽度换行，确保长英文和中英文混排不越过页面。"""
    lines: list[str] = []
    current = ""
    for character in text:
        candidate = current + character
        if current and _text_width(candidate) > MAX_WIDTH:
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
    """按 CJK/ASCII run 切换字体，避免 CID 字体把英文显示成全角字距。"""
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


def generate() -> tuple[int, int]:
    """生成缺失的语料，返回 ``(生成数, 跳过数)``。"""
    OUTPUT.mkdir(parents=True, exist_ok=True)
    pdfmetrics.registerFont(UnicodeCIDFont(CJK_FONT_NAME))
    generated = 0
    skipped = 0
    for index, text in enumerate(CASES, start=1):
        pdf_path = OUTPUT / f"case_{index:02d}.pdf"
        text_path = OUTPUT / f"case_{index:02d}.txt"
        if pdf_path.exists() and text_path.exists():
            skipped += 1
            continue

        text_path.write_text(text + "\n", encoding="utf-8")
        document = canvas.Canvas(
            str(pdf_path),
            pagesize=A4,
            pageCompression=1,
            invariant=1,
        )
        document.setTitle(f"PDF parser gate case {index:02d}")
        document.setAuthor("ai-daily-report synthetic benchmark")
        y = TOP
        for line in _wrap_text(text):
            _draw_line(document, line, y)
            y -= LINE_HEIGHT
        document.showPage()
        document.save()
        generated += 1
    return generated, skipped


if __name__ == "__main__":
    created, existing = generate()
    print(
        f"generated {created} pdf cases, skipped {existing}, "
        f"total {len(CASES)} in {OUTPUT}"
    )
