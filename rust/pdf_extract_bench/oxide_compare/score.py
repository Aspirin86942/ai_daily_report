"""Judge complete pages against independent text, including cell and word boundaries."""

import re
import unicodedata
from itertools import zip_longest


TOKEN = re.compile(r"[A-Za-z]+(?:[-_][A-Za-z0-9]+)+|[+-]?\d+(?:,\d{3})*(?:\.\d+)?%?|[A-Za-z]+|[^\s]")


def tokens(text):
    return TOKEN.findall(unicodedata.normalize("NFC", text))


def has_order(text, blocks):
    stream = tokens(text)
    cursor = 0
    for block in blocks:
        part = tokens(block)
        while cursor <= len(stream) - len(part) and stream[cursor:cursor + len(part)] != part:
            cursor += 1
        if cursor > len(stream) - len(part):
            return False
        cursor += len(part)
    return True


def score(truth, output):
    actual = output["pages"]
    numbers = [p["page"] for p in actual]
    expected = truth["pages"]
    pages_ok = numbers == list(range(1, len(expected) + 1))
    by_number = {p["page"]: p["text"] for p in actual}
    mismatches = []
    for page, gold in enumerate(expected, start=1):
        if page not in by_number:
            mismatches.append({"page": page, "missing_page": True})
            continue
        wanted, found = tokens(gold), tokens(by_number[page])
        if wanted != found:
            index, pair = next((i, pair) for i, pair in enumerate(zip_longest(wanted, found))
                               if pair[0] != pair[1])
            mismatches.append({"page": page, "token_index": index, "expected": pair[0],
                               "actual": pair[1], "expected_tokens": len(wanted),
                               "actual_tokens": len(found)})
    first = by_number.get(1, "")
    table = truth.get("table_rows", {})
    if isinstance(table, list):  # Earlier single-page benchmark manifests.
        table = {"1": table}
    bad_tables = []
    for number, rows in table.items():
        lines = by_number.get(int(number), "").splitlines()
        if [tokens(s) for s in lines if s.strip()] != [tokens(s) for s in rows]:
            bad_tables.append(int(number))
    rows_ok = not bad_tables
    order_ok = has_order(first, truth.get("reading_order", []))
    status_ok = truth["status"] == output["status"]
    return {
        "pass": status_ok and pages_ok and not mismatches and rows_ok and order_ok
                and not output.get("truncated", False),
        "status_ok": status_ok, "page_numbers_ok": pages_ok, "mismatched_pages": mismatches,
        "table_rows_ok": rows_ok if table else None, "mismatched_table_pages": bad_tables,
        "column_order_ok": order_ok if "reading_order" in truth else None,
    }


def check_oracle():
    assert tokens("USD\t-12,345.67") == tokens("USD -12,345.67")
    for wrong in ["USD 12,345.67", "USD -12,345 .67", "USD -12,345.76"]:
        assert tokens(wrong) != tokens("USD -12,345.67")
    assert tokens("not approved") != tokens("notapproved")
    assert tokens("SYN-731") != tokens("SYN -731")
    assert not has_order("Left Right Pay Hold", ["Left", "Pay", "Right", "Hold"])
    truth = {"status": "text", "pages": ["A 10\nB 20", ""]}
    out = {"status": "text", "pages": [{"page": 1, "text": "A 10 B 20"}, {"page": 2, "text": ""}]}
    assert score(truth, out)["pass"]
    assert not score({**truth, "table_rows": ["A 10", "B 20"]}, out)["pass"]
    assert not score(truth, {**out, "pages": out["pages"][:1]})["pass"]
    assert not score(truth, {**out, "status": "parse_error"})["pass"]
    assert not score(truth, {**out, "truncated": True})["pass"]
