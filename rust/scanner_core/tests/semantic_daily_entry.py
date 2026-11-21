"""Run the real Windows daily CLI twice against inspectable, synthetic UTF-8 files."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
from datetime import datetime, timedelta, timezone


REPO = Path(__file__).resolve().parents[3]
ARTIFACTS = REPO / ".artifacts"
DAY = "2026-11-20"
OPENING = "本记录仅用于复核。"
MIDDLE = "## 第三节 复核条件"
CONDITION = "登记已完成。但未经复核，不得付款。"
ENDING = "结论：𠀀批次尚未批准。"
HEADER = "页眉：合成验收"
FOOTER = "页脚：仅供复核"
BODY = "相同正文仍要逐页保留。"
COLUMNS = "| 项目 | 金额 |\n| --- | --- |"
SECOND_ROW = "| 乙 | 7 |"
TSV_LINES = ["\t金额\t说明", "\t75\t合成行", "项目乙\t\t核对", "尾项\t7\t", "\t\t"]


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2), encoding="utf-8")


def parse_attempts(root):
    path = (root / "db" / "scan_index_rust_v3.sqlite3").resolve(strict=True)
    with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as conn:
        return conn.execute(
            "SELECT coalesce(sum(parse_attempt_count), 0) FROM scan_file_execution"
        ).fetchone()[0]


def run_cli(root, exe, stage):
    # Only the synthetic config and Windows process essentials enter this child.
    env = {k: os.environ[k] for k in ("SystemRoot", "WINDIR", "PATH", "SYSTEMDRIVE", "COMSPEC")
           if k in os.environ}
    env.update(TEMP=str(root / "tmp"), TMP=str(root / "tmp"))
    cmd = [str(exe), "--config-dir", str(root / "config"), "daily", "--dry-run", "--target-date", DAY]
    result = subprocess.run(cmd, cwd=root, env=env, capture_output=True, timeout=180)
    if result.returncode:
        raise RuntimeError(result.stderr.decode("utf-8", errors="replace"))
    out = json.loads(result.stdout)
    assert out["action"] == "dry_run" and out["model_calls"] == 0
    (root / f"{stage}.json").write_bytes(result.stdout)
    return json.loads(out["markdown"]), parse_attempts(root)


def check_source(root, selection, name, expected):
    raw = (root / "work" / name).read_bytes()
    original = raw.decode("utf-8")
    blocks = [m for m in selection["materials"] if m["source"]["relative_path"] == name]
    assert blocks
    excerpt = "".join(block["text"] for block in blocks)
    reads, omissions, ranges = [], [], []
    for index, block in enumerate(blocks):
        audit = block["source"]["coverage_limits"]["evidence_audit"]
        if index:
            assert "read_ranges" not in audit and "omitted_ranges" not in audit
        reads.extend(audit.get("read_ranges") or [])
        omissions.extend(audit.get("omitted_ranges") or [])
        for evidence in block["evidence"]:
            for r in evidence["ranges"]:
                unit, start, end = r["unit"], r["start"], r["end"]
                assert unit in ("byte", "text_char")
                assert 0 <= start < end <= (len(raw) if unit == "byte" else len(original))
                quote = raw[start:end].decode("utf-8") if unit == "byte" else original[start:end]
                assert quote and quote in block["text"]
                ranges.append({"unit": unit, "start": start, "end": end, "quote": quote})
    for fact, count in expected.items():
        positions = {(r["start"], r["end"]) for r in ranges if r["quote"] == fact}
        assert len(positions) == count, (name, fact, len(positions), count)
    assert all(any(read["unit"] == r["unit"] and read["start"] <= r["start"]
                   and read["end"] >= r["end"] for read in reads) for r in ranges)
    byte_windows = [r for r in reads if r["unit"] == "byte"]
    for block in blocks:
        positions = [r for ev in block["evidence"] for r in ev["ranges"] if r["unit"] == "byte"]
        if positions:
            assert any(all(w["start"] <= r["start"] and r["end"] <= w["end"] for r in positions)
                       for w in byte_windows), "material crosses an unread byte interval"
    return {"source_bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest(),
            "excerpt_chars": len(excerpt), "material_blocks": len(blocks), "read_ranges": reads,
            "omitted_ranges": omissions, "verified_excerpt_ranges": ranges}, excerpt


def check_table_units(selection, name, header, source_text):
    blocks = [m for m in selection["materials"] if m["source"]["relative_path"] == name]
    block, = [m for m in blocks if SECOND_ROW in m["text"]]
    assert f"{header}\n{COLUMNS}\n{SECOND_ROW}" in block["text"]
    assert "| 甲 | 11 |" not in block["text"]
    positions = [r for ev in block["evidence"] for r in ev["ranges"]]
    page_start = source_text.index("\f") + 1
    assert all(r["unit"] == "text_char" and r["start"] >= page_start for r in positions)
    assert any(r["start"] == page_start and source_text[r["start"]:r["end"]] == header for r in positions)
    assert not block["source"]["parse_truncated"]
    return {"table_row": SECOND_ROW, "unit_header": header, "header_start": page_start}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=ARTIFACTS / "rust-migration/cargo-release/debug")
    parser.add_argument("--output-parent", type=Path, default=ARTIFACTS)
    args = parser.parse_args()
    workers = args.bin_dir.resolve(strict=True)
    exe = (workers / "ai-daily-report.exe").resolve(strict=True)
    parent = args.output_parent.resolve(strict=True)
    if not parent.is_relative_to(ARTIFACTS.resolve(strict=True)):
        raise ValueError("output-parent must be an isolated directory inside .artifacts")
    root = Path(tempfile.mkdtemp(prefix="daily-semantic-entry-", dir=parent))
    print(root, flush=True)
    for folder in ("work", "config", "data", "reports", "db", "logs", "notes", "tmp"):
        (root / folder).mkdir()
    filler = "常规背景材料，仅作记录。\n" * 400
    long_note = f"# 说明\n{OPENING}\n{filler}{MIDDLE}\n{CONDITION}\n{filler}# 最终判断\n{ENDING}"
    page = f"{HEADER}\n\n\n{BODY}\n{HEADER}\n逐页确认。\n{FOOTER}"
    changed_unit = "页眉：乙项目（单位：元）"
    same_unit = "页眉：同项目（单位：万元）"
    files = {"long.md": long_note, "pages.txt": "\n\f\n".join([page] * 3),
             "units-changed.md": f"页眉：甲项目\n单位：万元\n{COLUMNS}\n| 甲 | 11 |\n\f{changed_unit}\n{COLUMNS}\n{SECOND_ROW}",
             "units-same.md": f"{same_unit}\n{COLUMNS}\n| 甲 | 11 |\n\f{same_unit}\n{COLUMNS}\n{SECOND_ROW}",
             "empty-columns.txt": "\r\n".join(TSV_LINES)}
    when = datetime.fromisoformat(DAY + "T12:00:00").replace(tzinfo=timezone(timedelta(hours=8))).timestamp()
    for name, text in files.items():
        path = root / "work" / name
        path.write_bytes(text.encode("utf-8"))
        os.utime(path, (when, when))
    config = {"paths": {k: str(root / v) for k, v in (
        ("work_dir", "work"), ("data_dir", "data"), ("reports_dir", "reports"),
        ("db_dir", "db"), ("log_dir", "logs"))},
        "report": {"profile": "personal", "notes_dir": str(root / "notes")},
        "archive": {"allow_work_dir_for_model": True},
        "scanner": {"allowed_extensions": [".md", ".txt"], "direct_text_read_bytes": 1200,
                    "text_max_chars": 300, "text_excerpt_max_chars": 300, "total_max_chars": 1600,
                    "max_workers": 1, "office_worker_path": str((workers / "ai-daily-office-parser.exe").resolve(strict=True)),
                    "pdf_worker_path": str((workers / "ai-daily-pdf-parser.exe").resolve(strict=True))}}
    write_json(root / "config/settings.yaml", config)
    cold, first = run_cli(root, exe, "cold")
    warm, second = run_cli(root, exe, "warm")
    assert (first, second - first) == (len(files), 0)
    assert cold["summary"]["selected_files"] == warm["summary"]["selected_files"] == len(files)
    checks = {}
    for name, expected in [("long.md", {s: 1 for s in (OPENING, MIDDLE, CONDITION, ENDING)}),
                           ("pages.txt", {HEADER: 6, FOOTER: 3, BODY: 3}),
                           ("units-changed.md", {changed_unit: 1, SECOND_ROW: 1}),
                           ("units-same.md", {same_unit: 2, SECOND_ROW: 1}),
                           ("empty-columns.txt", {line: 1 for line in TSV_LINES})]:
        check, excerpt = check_source(root, cold, name, expected)
        assert (check, excerpt) == check_source(root, warm, name, expected)
        (root / (name + ".excerpt.txt")).write_bytes(excerpt.encode("utf-8"))
        if name == "pages.txt":
            assert (excerpt.count(HEADER), excerpt.count(FOOTER), excerpt.count(BODY)) == (4, 1, 3)
        if name.startswith("units-"):
            header = changed_unit if name == "units-changed.md" else same_unit
            check["table_unit_check"] = check_table_units(cold, name, header, files[name])
            assert check["table_unit_check"] == check_table_units(warm, name, header, files[name])
        if name == "empty-columns.txt":
            assert excerpt.endswith("\n".join(TSV_LINES))
            check["field_counts"] = [len(line.split("\t")) for line in TSV_LINES]
            assert check["field_counts"] == [3] * len(TSV_LINES)
        checks[name] = check
    windows = checks["long.md"]["read_ranges"]
    assert checks["long.md"]["material_blocks"] >= 3
    assert checks["pages.txt"]["material_blocks"] == 1
    original_positions = len(checks["long.md"]["verified_excerpt_ranges"]) + len(checks["pages.txt"]["verified_excerpt_ranges"])
    assert original_positions == 39
    assert [r["label"] for r in windows] == ["head", "middle", "tail"]
    assert sum(r["end"] - r["start"] for r in windows) == 1200
    unread = [(a["end"], b["start"]) for a, b in zip(windows, windows[1:])]
    assert all(start < end for start, end in unread)
    assert any(g["code"] == "PARSE_BOUNDED" for g in cold["gaps"])
    reported = {(r["start"], r["end"]) for r in checks["long.md"]["omitted_ranges"] if r["label"] == "unread"}
    write_json(root / "verified-ranges.json", {"command": ["daily", "--dry-run", "--target-date", DAY],
        "cold_parser_calls": first, "warm_parser_calls": second - first, "model_calls": 0,
        "unread_from_read_windows": unread, "explicit_unread_ranges_match": reported == set(unread),
        "original_verified_ranges": original_positions,
        "sources": checks})
    assert reported == set(unread), "normal source metadata must carry the exact unread byte ranges"


if __name__ == "__main__":
    main()
