"""Stage only named, committed synthetic fixtures and deterministic benchmark pages."""

import hashlib
import json
import subprocess
from pathlib import Path

from PIL import Image, ImageDraw
from reportlab.lib.utils import ImageReader
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.ttfonts import TTFont
from reportlab.pdfgen import canvas


REPO = Path(__file__).resolve().parents[3]
RELEASE_COMMIT = "818433f445a20b09cc072b993673b8c5eb6ecbf3"
EVIDENCE_ROOT = (REPO / ".artifacts/kernel-once-20261121/pdf-compare").resolve()
STAGE = Path.cwd().resolve()


def check_stage():
    if STAGE != EVIDENCE_ROOT and STAGE.parent != EVIDENCE_ROOT:
        raise RuntimeError(f"run in {EVIDENCE_ROOT} or a dedicated child directory")


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def fixture(name):
    return subprocess.check_output(
        ["git", "-C", str(REPO), "show", f"{RELEASE_COMMIT}:tests/fixtures/{name}"]
    )


def new_pdf(folder, name):
    return canvas.Canvas(
        str(folder / f"{name}.pdf"), pagesize=(600, 800), invariant=1, pageCompression=1
    )


def write_at(doc, x, y, text, font="Helvetica"):
    doc.setFont(font, 13)
    doc.drawString(x, y, text)


def synthetic_raster():
    image = Image.new("RGB", (400, 75), "white")
    drawing = ImageDraw.Draw(image)
    drawing.rectangle((0, 0, 399, 74), outline="black", width=3)
    drawing.text((12, 20), "PIXELS ONLY: USD 99,999.99", fill="black", font_size=22)
    return ImageReader(image)


def stage_corpus(folder):
    folder.mkdir()
    cases = {}

    def register(name, category, pages, status="text", **extra):
        cases[name] = {
            "category": category, "status": status, "pages": pages,
            "sha256": sha(folder / f"{name}.pdf"), **extra,
        }

    for index in range(1, 7):
        name = f"case_{index:02}"
        source = f"pdf_benchmark/{name}"
        (folder / f"{name}.pdf").write_bytes(fixture(f"{source}.pdf"))
        truth = fixture(f"{source}.txt")
        register(name, "committed_cid_or_latin", [truth.decode("utf-8").strip()],
                 source=source, truth_sha256=hashlib.sha256(truth).hexdigest())

    for name, status, pages in [
        ("no_text_blank_01", "no_text", [""]),
        ("error_encrypted_01", "protected", []),
        ("error_corrupt_01", "parse_error", []),
    ]:
        source = f"pdf_classifier/{name}.pdf"
        (folder / f"{name}.pdf").write_bytes(fixture(source))
        register(name, name.rsplit("_", 1)[0], pages, status, source=source)

    doc = new_pdf(folder, "amount_table")
    # Paint each column separately, reverse the numeric objects, and leave one empty value.
    for y, label in [(710, "Item"), (672, "Transport"), (635, "Refund"),
                     (598, "Pending"), (560, "Spares")]:
        write_at(doc, 55, y, label)
    for y, value in [(560, "18 kg"), (635, "USD -37.50"),
                     (672, "USD 12,345.60"), (710, "Amount")]:
        write_at(doc, 355, y, value)
    for y in [700, 550]:
        doc.line(52, y, 500, y)
    doc.showPage()
    doc.save()
    rows = ["Item Amount", "Transport USD 12,345.60", "Refund USD -37.50", "Pending", "Spares 18 kg"]
    register("amount_table", "table_amount_quantity_empty_cell", ["\n".join(rows)],
             table_rows={"1": rows})

    doc = new_pdf(folder, "two_columns")
    heading = "SETTLEMENT CONDITIONS AND SUPPLIER RETENTION"
    write_at(doc, 115, 745, heading)
    for y, left, right in [
        (685, "Dispatch", "Retention"),
        (660, "Release after delivery.", "Hold until review."),
        (635, "Pay USD 430.00.", "Deduct USD 17.25."),
    ]:
        write_at(doc, 48, y, left)
        write_at(doc, 338, y, right)
    doc.showPage()
    doc.save()
    blocks = [heading, "Dispatch", "Release after delivery.", "Pay USD 430.00.",
              "Retention", "Hold until review.", "Deduct USD 17.25."]
    register("two_columns", "full_width_heading_column_major", ["\n".join(blocks)],
             reading_order=blocks)

    raster = synthetic_raster()
    doc = new_pdf(folder, "raster_only")
    doc.drawImage(raster, 48, 630, width=400, height=75)
    doc.showPage()
    doc.save()
    register("raster_only", "raster_no_text_layer", [""], "no_text")

    cjk = Path("C:/Windows/Fonts/simhei.ttf")
    pdfmetrics.registerFont(TTFont("BenchmarkCJK", str(cjk)))
    doc = new_pdf(folder, "mixed_pages")
    write_at(doc, 48, 720, "合成样本：订单 SYN-731，预付人民币 8,901.20 元。", "BenchmarkCJK")
    doc.showPage()
    doc.drawImage(raster, 48, 630, width=400, height=75)
    doc.showPage()
    doc.drawImage(raster, 48, 550, width=400, height=75)
    write_at(doc, 48, 710, "Review 3: 复核后支付 24.50 元。", "BenchmarkCJK")
    doc.showPage()
    doc.showPage()  # A physical blank page must keep its page number.
    write_at(doc, 48, 720, "Final balance: USD -42.75 only after approval.")
    doc.showPage()
    doc.save()
    register("mixed_pages", "embedded_cjk_raster_mixed_blank_last_page", [
        "合成样本：订单 SYN-731，预付人民币 8,901.20 元。", "",
        "Review 3: 复核后支付 24.50 元。", "", "Final balance: USD -42.75 only after approval.",
    ], embedded_font_sha256=sha(cjk))

    doc = new_pdf(folder, "chinese_columns")
    for y, left, right in [
        (716, "甲项：货款", "乙项：质保金"),
        (684, "未复核不得付款。", "验收后才可退还。"),
        (652, "金额 4,817.25 元。", "保留 780.00 元。"),
    ]:
        write_at(doc, 48, y, left, "BenchmarkCJK")
        write_at(doc, 350, y, right, "BenchmarkCJK")
    doc.showPage()
    doc.save()
    blocks = ["甲项：货款", "未复核不得付款。", "金额 4,817.25 元。",
              "乙项：质保金", "验收后才可退还。", "保留 780.00 元。"]
    register("chinese_columns", "cjk_columns_negation_conditions_amounts", ["\n".join(blocks)],
             reading_order=blocks)

    doc = new_pdf(folder, "continued_table")
    # Each physical page reuses the labels but attaches different amounts to them.
    for refund, charge in [("-37.50 元", "240.00 元"), ("-73.05 元", "402.00 元")]:
        for y, label in [(720, "项目"), (680, "退款"), (640, "运费")]:
            write_at(doc, 60, y, label, "BenchmarkCJK")
        for y, value in [(640, charge), (680, refund), (720, "金额")]:
            write_at(doc, 350, y, value, "BenchmarkCJK")
        doc.showPage()
    doc.save()
    first = ["项目 金额", "退款 -37.50 元", "运费 240.00 元"]
    second = ["项目 金额", "退款 -73.05 元", "运费 402.00 元"]
    register("continued_table", "cjk_table_repeated_labels_across_pages",
             ["\n".join(first), "\n".join(second)], table_rows={"1": first, "2": second})
    return cases


if __name__ == "__main__":
    check_stage()
    cases = stage_corpus(STAGE / "corpus")
    with (STAGE / "truth.json").open("x", encoding="utf-8") as handle:
        json.dump(cases, handle, indent=2, ensure_ascii=False)
    print(f"{len(cases)} synthetic files, {sum(len(c['pages']) for c in cases.values())} readable pages")
