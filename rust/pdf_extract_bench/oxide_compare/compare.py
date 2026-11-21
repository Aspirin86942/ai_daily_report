"""Windows process benchmark. Run from the prepared pdf-compare evidence directory."""

import asyncio
import json
import os
import platform
import random
import shutil
import statistics
import subprocess
import time
import uuid
from contextlib import asynccontextmanager
from datetime import datetime, timezone
from importlib.metadata import version
from pathlib import Path

from corpus import RELEASE_COMMIT, REPO, STAGE, check_stage, sha
from score import check_oracle, score, tokens


MODES = ("pdfium", "extract", "layout", "extract-no-tables")
REPEATS = 7
TIMEOUT = 10
STATIC_RELEASE = "x86_64-pc-windows-msvc/release"
RELEASE = REPO / ".releases/rust-20261121-detailed-daily-01"
EXPECTED_CASES = {f"case_{i:02}" for i in range(1, 7)} | {
    "no_text_blank_01", "error_encrypted_01", "error_corrupt_01", "amount_table",
    "two_columns", "mixed_pages", "raster_only", "chinese_columns", "continued_table",
}


def save(path, value):
    with path.open("x", encoding="utf-8") as handle:
        json.dump(value, handle, ensure_ascii=False, indent=2)


def stage_programs(run):
    manifest = json.loads((RELEASE / "parser-manifest.json").read_text(encoding="utf-8"))
    release = json.loads((RELEASE / "manifest.json").read_text(encoding="utf-8"))
    assert release["git_commit"] == RELEASE_COMMIT
    pinned = json.loads((REPO / "rust/build-inputs.json").read_text(encoding="utf-8"))["pdfium"]
    assert manifest["files"]["pdfium.dll"] == pinned["dll_sha256"]
    target = REPO / ".artifacts/rust-migration/cargo-release" / STATIC_RELEASE
    bins = {}
    for name, source_dir, subdir in [
        ("ai-daily-pdf-parser.exe", RELEASE, "baseline"),
        ("pdfium.dll", RELEASE, "baseline"),
        ("oxide-pages.exe", target, "candidate"),
        ("pdfium-startup-probe.exe", target, "probe"),
    ]:
        original = (source_dir / name).resolve(strict=True)
        dest = run / subdir / name
        dest.parent.mkdir(exist_ok=True)
        shutil.copyfile(original, dest)
        expected = manifest["files"][name] if source_dir == RELEASE else sha(original)
        assert sha(dest) == expected, name
        bins[name] = {"source": str(original), "staged": str(dest), "sha256": expected}
    return bins


def frame(case):
    path = (STAGE / "corpus" / f"{case}.pdf").resolve(strict=True)
    if path.parent != (STAGE / "corpus").resolve():
        raise ValueError("fixture outside staged corpus")
    stat = path.stat()
    request = {
        "contract": "ai_daily_worker", "protocol_version": 4,
        "request_id": str(uuid.uuid5(uuid.NAMESPACE_DNS, f"synthetic.pdf.{case}")),
        "operation": "pdf_parse",
        "payload": {
            "file_path": str(path), "file_type": ".pdf", "backend": "rust_pdfium_v3",
            "remaining_timeout_ms": TIMEOUT * 1000, "max_file_size_bytes": 8_388_608,
            "expected_source_version": f"mtime_ns={stat.st_mtime_ns}:size={stat.st_size}",
            "parser_limits": {"kind": "pdf", "max_pages": 50, "excerpt_max_chars": 60_000,
                              "selected_pages": None},
        },
    }
    return request["request_id"], (json.dumps(request) + "\n").encode(), (json.dumps(str(path)) + "\n").encode()


def baseline_output(reply, request_id):
    if (reply["contract"], reply["protocol_version"], reply["request_id"], reply["operation"]) != (
        "ai_daily_worker", 4, request_id, "pdf_parse"
    ):
        raise ValueError("invalid PDFium response envelope")
    if reply["status"] == "error":
        code = reply["error"]["error_code"]
        return {"status": {"PDF_PROTECTED": "protected", "PARSER_FAILED": "parse_error"}.get(code, code),
                "diagnostic": code, "pages": []}
    result = reply["result"]
    audit = result["evidence_audit"]
    lines = {}
    for span in audit["read_ranges"]:
        assert span["unit"] == "pdf_page"
        for n in range(span["start"], span["end"]):
            assert n not in lines
            lines[n] = []
    consumed = []
    for unit in audit["content_units"]:
        span, = unit["ranges"]
        assert span["unit"] == "pdf_page" and span["end"] == span["start"] + 1
        line = result["content"][unit["text_start"]:unit["text_end"]]
        lines[span["start"]].append(line)
        consumed.append(line)
    assert tokens("\n".join(consumed)) == tokens(result["content"]), "content missing from source ranges"
    return {"status": "text" if result["content"].strip() else "no_text", "truncated": result["truncated"],
            "pages": [{"page": page, "text": "\n".join(text)} for page, text in lines.items()]}


async def read_line(process):
    line = await asyncio.wait_for(process.stdout.readline(), TIMEOUT)
    if not line:
        code = await asyncio.wait_for(process.wait(), TIMEOUT)
        raise ChildProcessError(f"worker exited with {code}")
    return line


@asynccontextmanager
async def worker(mode, bins):
    baseline = mode == "pdfium"
    binary = bins["ai-daily-pdf-parser.exe" if baseline else "oxide-pages.exe"]["staged"]
    start = time.perf_counter_ns()
    proc = await asyncio.create_subprocess_exec(
        binary, "session" if baseline else mode, cwd=STAGE, creationflags=subprocess.CREATE_NO_WINDOW,
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
        limit=4 * 1024 * 1024,
    )
    try:
        hello = json.loads(await read_line(proc))
        ready = time.perf_counter_ns()
        if baseline:
            assert hello["worker_kind"] == "pdf" and hello["protocol_version"] == 4
        else:
            assert hello == {"ready": True}
        yield proc, start, ready
        proc.stdin.close()
        if await asyncio.wait_for(proc.wait(), TIMEOUT) != 0:
            raise ChildProcessError("worker failed on shutdown")
    finally:
        if proc.returncode is None:
            proc.kill()
            await proc.wait()


async def request(proc, mode, wire):
    request_id, baseline, candidate = wire
    t0 = time.perf_counter_ns()
    proc.stdin.write(baseline if mode == "pdfium" else candidate)
    await asyncio.wait_for(proc.stdin.drain(), TIMEOUT)
    line = await read_line(proc)
    end = time.perf_counter_ns()
    reply = json.loads(line)
    output = baseline_output(reply, request_id) if mode == "pdfium" else reply
    return output, (end - t0) / 1e6, end


def distribution(values):
    return {"n": len(values), "p50_ms": round(statistics.median(values), 4),
            "p95_ms": round(statistics.quantiles(values, n=100, method="inclusive")[94], 4)}


def observe_modules(pid):
    # Query only the child started by this benchmark, outside timed samples.
    command = (f"$ErrorActionPreference='Stop'; (Get-Process -Id {int(pid)}).Modules | "
               "Where-Object ModuleName -eq 'pdfium.dll' | ForEach-Object FileName")
    out = subprocess.check_output(
        ["pwsh", "-NoProfile", "-NonInteractive", "-Command", command], text=True,
        cwd=STAGE, creationflags=subprocess.CREATE_NO_WINDOW, timeout=TIMEOUT,
    )
    return [str(Path(line).resolve(strict=True)) for line in out.splitlines() if line.strip()]


def probe_startup(bins):
    rows = []
    for i in range(25):
        order = ("noop", "dll") if i % 2 == 0 else ("dll", "noop")
        for mode in order:
            arg = "--noop" if mode == "noop" else bins["pdfium.dll"]["staged"]
            start = time.perf_counter_ns()
            raw = subprocess.check_output([bins["pdfium-startup-probe.exe"]["staged"], arg],
                                          cwd=STAGE, timeout=TIMEOUT, creationflags=subprocess.CREATE_NO_WINDOW)
            elapsed = (time.perf_counter_ns() - start) / 1e6
            rows.append({"mode": mode, "round": i, "wall_ms": elapsed, **json.loads(raw)})
    dll = [row for row in rows if row["mode"] == "dll"]
    summary = {key.removesuffix("_us"): distribution([row[key] / 1000 for row in dll])
               for key in ("read_us", "sha256_us", "load_and_bind_us", "init_us", "total_us")}
    for mode in ("noop", "dll"):
        summary[mode + "_process_wall"] = distribution([r["wall_ms"] for r in rows if r["mode"] == mode])
    return rows, summary


async def main():
    check_stage()
    if os.name != "nt":
        raise RuntimeError("Windows native process measurements require Windows")
    check_oracle()
    truth = json.loads((STAGE / "truth.json").read_text(encoding="utf-8"))
    if truth.keys() != EXPECTED_CASES:
        raise ValueError("staged manifest does not contain the full benchmark corpus")
    wires = {name: frame(name) for name in truth}
    for name, case in truth.items():
        assert sha(STAGE / "corpus" / f"{name}.pdf") == case["sha256"]
    run = STAGE / ("run-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ"))
    run.mkdir()
    bins = stage_programs(run)
    sources = Path(__file__).resolve().parent
    save(run / "identities.json", {"programs": bins, "release_commit": RELEASE_COMMIT,
         "platform": {"windows": platform.version(), "arch": platform.machine(), "logical_cpus": os.cpu_count(),
                      "python": platform.python_version(), "reportlab": version("reportlab"), "pillow": version("pillow")},
         "production_lock_sha256": sha(REPO / "rust/Cargo.lock"), "truth_sha256": sha(STAGE / "truth.json"),
         "tool_sources": {f.relative_to(sources).as_posix(): sha(f) for f in [
             sources / name for name in ("Cargo.toml", "Cargo.lock", "compare.py", "corpus.py", "score.py", "run.ps1",
                                         "src/bin/oxide-pages.rs", "src/bin/pdfium-startup-probe.rs")]}})
    results = {mode: {name: [] for name in truth} for mode in MODES}
    samples = []

    def record(mode, name, output):
        variants = results[mode][name]
        for index, item in enumerate(variants):
            if item["output"] == output:
                item["observations"] += 1
                return index
        variants.append({"output": output, "observations": 1, "score": score(truth[name], output)})
        return len(variants) - 1

    modules = {}
    for mode in MODES:
        async with worker(mode, bins) as (proc, _, _):
            sequence = ["case_02", "error_encrypted_01", "error_corrupt_01", "case_02"]
            statuses = []
            for case in sequence:
                out, _, _ = await request(proc, mode, wires[case])
                record(mode, case, out)
                statuses.append(out["status"])
            paths = observe_modules(proc.pid)
            expected = [bins["pdfium.dll"]["staged"]] if mode == "pdfium" else []
            assert paths == expected, (mode, paths)
            modules[mode] = {"pid": proc.pid, "dll_paths": paths, "sequence": sequence, "statuses": statuses}
    save(run / "isolation.json", modules)

    rng = random.Random(20261121)
    with (run / "latencies.jsonl").open("x", encoding="utf-8") as log:
        def sample(row):
            samples.append(row)
            log.write(json.dumps(row) + "\n")
            log.flush()

        for iteration in range(REPEATS):
            names = list(truth)
            rng.shuffle(names)
            for index, name in enumerate(names):
                offset = (index + iteration) % len(MODES)
                for mode in MODES[offset:] + MODES[:offset]:
                    async with worker(mode, bins) as (proc, start, ready):
                        out, elapsed, end = await request(proc, mode, wires[name])
                        sample({"round": iteration, "mode": mode, "case": name, "regime": "cold",
                                "pid": proc.pid, "ready_ms": (ready - start) / 1e6, "request_ms": elapsed,
                                "wall_ms": (end - start) / 1e6, "variant": record(mode, name, out)})
            offset = iteration % len(MODES)
            for mode in MODES[offset:] + MODES[:offset]:
                async with worker(mode, bins) as (proc, _, _):
                    # One complete unmeasured pass. Every measured request reopens a PDF;
                    # consecutive requests use different paths, including across the boundary.
                    for name in names:
                        out, _, _ = await request(proc, mode, wires[name])
                        record(mode, name, out)
                    for name in names:
                        out, elapsed, _ = await request(proc, mode, wires[name])
                        sample({"round": iteration, "mode": mode, "case": name, "regime": "warm",
                                "pid": proc.pid, "wall_ms": elapsed, "variant": record(mode, name, out)})
    save(run / "quality.json", results)
    probe_rows, calibration = probe_startup(bins)
    save(run / "dll-probe.json", probe_rows)

    scores, timings = {}, {}
    for mode, cases in results.items():
        passed = [name for name, variants in cases.items()
                  if len(variants) == 1 and variants[0]["score"]["pass"]]
        scores[mode] = {"passed": len(passed), "total": len(cases),
                        "failed": sorted(cases.keys() - set(passed)),
                        "changed_between_calls": [n for n, v in cases.items() if len(v) != 1]}
        cold = [s for s in samples if (s["mode"], s["regime"]) == (mode, "cold")]
        warm = [s for s in samples if (s["mode"], s["regime"]) == (mode, "warm")]
        assert len(cold) == len(warm) == REPEATS * len(truth)
        timings[mode] = {
            "cold_to_first_reply": distribution([s["wall_ms"] for s in cold]),
            "cold_to_ready": distribution([s["ready_ms"] for s in cold]),
            "cold_request_after_ready": distribution([s["request_ms"] for s in cold]),
            "warm_request": distribution([s["wall_ms"] for s in warm]),
        }
    quality_pass = [mode for mode in MODES if mode != "pdfium" and not scores[mode]["failed"]]
    shared = [name for name in truth if all(name not in scores[m]["failed"] for m in MODES)]
    common_timings = {}
    if len(shared) * REPEATS >= 2:
        for mode in MODES:
            common_timings[mode] = {
                regime: distribution([r["wall_ms"] for r in samples if r["mode"] == mode
                                      and r["regime"] == regime and r["case"] in shared])
                for regime in ("cold", "warm")
            }
    report = {
        "verdict": "candidate_passed_semantics" if quality_pass else "retain_pinned_pdfium",
        "quality_pass_modes": quality_pass, "quality": scores, "timings_ms": timings,
        "common_pass_cases": shared, "common_pass_timings_ms": common_timings,
        "startup_probe_ms": calibration, "repeats": REPEATS, "timeout_seconds": TIMEOUT,
        "cold_definition": "new Windows child, spawn to first full response; OS cache not flushed",
        "warm_definition": "one full untimed corpus pass, then different files in the same Windows child",
        "timing_scope": "released v4 PDFium worker includes source and evidence audits; candidate returns pages",
        "gate": "Every expected physical page and complete ordered token stream, exact table rows, "
                "expected error/no-text class, no truncation, deterministic repeats.",
    }
    save(run / "summary.json", report)
    print(str(run))
    print(json.dumps({"verdict": report["verdict"], "quality": scores, "timed_requests": len(samples)}))


if __name__ == "__main__":
    asyncio.run(main())
