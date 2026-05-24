# Scanner Performance Evidence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add benchmark and production metrics so scanner performance bottlenecks can be proven before choosing Rust, Go, NTFS Journal, or worker-pool changes.

**Architecture:** Add a small metrics collector, extend SQLite scan metrics with backward-compatible migrations, instrument `FileScanner.scan_files()`, and add a benchmark script that calls the real scanner path. Keep `ScanResult` unchanged and preserve `latest_scan_run()` compatibility.

**Tech Stack:** Python 3.10+, pytest, SQLite, argparse, pathlib, JSON, Markdown

---

## File Structure

- Create: `src/services/scan_metrics.py`
  - Dataclasses and timing helpers for scan run and per-extension metrics.
- Modify: `src/services/scan_index_store.py`
  - Add scan run detail columns, extension metrics table, migrations, and read APIs.
- Modify: `src/services/file_scanner.py`
  - Add stage timing and per-extension parse timing without changing `ScanResult`.
- Create: `scripts/benchmark_scanner.py`
  - CLI entry that runs the real scanner and emits JSON / Markdown evidence.
- Create: `tests/test_scan_metrics.py`
  - Unit tests for timer-free metrics aggregation.
- Modify: `tests/test_scan_index_store.py`
  - Tests for migration, detail readback, and extension metrics.
- Modify: `tests/test_file_scanner.py`
  - Tests for production metrics written by scan paths, including empty scan.
- Create: `tests/test_benchmark_scanner.py`
  - Tests benchmark report formatting and output file writing.

## Tasks

### Task 1: Add Metrics Model

- [ ] Write failing tests in `tests/test_scan_metrics.py` for `ScanMetricsCollector`, stage duration setting, result counts, extension aggregation, timeout counting, and summary line.
- [ ] Run `conda run -n test python -m pytest tests/test_scan_metrics.py -q` and confirm it fails because `src.services.scan_metrics` is missing.
- [ ] Create `src/services/scan_metrics.py` with `ExtensionMetrics`, `ScanRunMetrics`, `ScanMetricsCollector`, `is_timeout_error()`, and summary helpers.
- [ ] Run `conda run -n test python -m pytest tests/test_scan_metrics.py -q` and confirm it passes.

### Task 2: Persist Full Metrics

- [ ] Add failing tests to `tests/test_scan_index_store.py` for new `scan_runs` columns, `save_scan_run_metrics(run_metrics=..., extension_metrics=...)`, `latest_scan_run_detail()`, and `list_extension_metrics(run_id)`.
- [ ] Run `conda run -n test python -m pytest tests/test_scan_index_store.py -q` and confirm the new tests fail on missing APIs or columns.
- [ ] Update `src/services/scan_index_store.py` with backward-compatible migrations and new persistence/read APIs while preserving `latest_scan_run()`.
- [ ] Run `conda run -n test python -m pytest tests/test_scan_index_store.py -q` and confirm it passes.

### Task 3: Instrument Production Scanner

- [ ] Add failing tests to `tests/test_file_scanner.py` that verify empty scans and mixed cached/uncached scans write full run detail and per-extension metrics.
- [ ] Run focused file scanner tests and confirm the new assertions fail before implementation.
- [ ] Update `src/services/file_scanner.py` to create a collector, measure `discovery`, `inventory_cache`, `parse`, and `aggregation`, record per-extension parse results, write complete metrics once per scan, and log a summary line.
- [ ] Run `conda run -n test python -m pytest tests/test_file_scanner.py -q` and confirm it passes.

### Task 4: Add Benchmark Script

- [ ] Add failing tests in `tests/test_benchmark_scanner.py` for JSON payload construction, Markdown rendering, `--json-out`, and `--markdown-out`.
- [ ] Run `conda run -n test python -m pytest tests/test_benchmark_scanner.py -q` and confirm it fails because `scripts/benchmark_scanner.py` is missing.
- [ ] Create `scripts/benchmark_scanner.py` with argument parsing for `--start-date`, `--end-date`, `--summary-mode`, `--json-out`, and `--markdown-out`.
- [ ] Run `conda run -n test python -m pytest tests/test_benchmark_scanner.py -q` and confirm it passes.

### Task 5: Full Verification

- [ ] Run `conda run -n test python -m pytest tests -q`.
- [ ] Run `conda run -n test python -m compileall main.py src tests scripts`.
- [ ] Run `git status --short` and inspect that only planned files changed.

## Self-Review

- Spec coverage: benchmark output, production metrics, SQLite persistence, per-extension stats, empty scans, and compatibility are covered.
- Placeholder scan: no placeholders remain; every task has a command and expected failure/pass condition.
- Type consistency: metrics names match the design doc and planned storage APIs.
