"""Timer-baseline evidence keeps corpus/profile/build provenance auditable."""
from __future__ import annotations

import copy
import json
import sqlite3
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))

import benchmark_timer_baseline as baseline  # noqa: E402
from benchmark_snapshot_warm import capture_run_reproducibility  # noqa: E402


def _snapshot_payload() -> dict:
    return {
        "snapshot_key_version": "context_snapshot_key_v1",
        "logical_request": {"work_dir": r"D:\private", "request_id": "removed"},
        "discovery": [{"relative_path": "private-name.txt"}],
        "profile": {
            "schema_version": "scanner_profile_v2",
            "parse": {"pdf": {"max_pages": 5}},
        },
        "engine_build": "sha256-source-v1:" + "e" * 64,
        "workers": {
            "office_contract": "ai_daily_worker_v1",
            "office_version": "0.1.0",
            "office_build": "sha256-source-v1:" + "a" * 64,
            "python_contract": "ai_daily_worker_v1",
            "python_version": "0.1.0",
            "python_build": "b" * 64,
        },
        "session": {
            "capability": "session",
            "contract": "ai_daily_python_session_v1",
            "version": "0.1.0",
            "build": "b" * 64,
        },
        "classifier": {
            "contract": "ai_daily_pdf_classifier_v1",
            "build": "c" * 64,
            "profile_hash": "d" * 64,
        },
    }


def _provenance_db() -> sqlite3.Connection:
    conn = sqlite3.connect(":memory:")
    conn.executescript(
        """
        CREATE TABLE context_runs (
            scan_run_id INTEGER PRIMARY KEY,
            context_profile_hash TEXT NOT NULL,
            artifact_id INTEGER NOT NULL
        );
        CREATE TABLE context_artifacts (
            artifact_id INTEGER PRIMARY KEY,
            snapshot_key_sha256 TEXT NOT NULL,
            snapshot_key_json TEXT NOT NULL
        );
        CREATE TABLE scan_run_attempts (
            scan_run_id INTEGER NOT NULL,
            attempt_number INTEGER NOT NULL,
            engine_fingerprint TEXT NOT NULL,
            office_worker_contract TEXT,
            office_worker_version TEXT,
            office_worker_build TEXT,
            python_worker_contract TEXT,
            python_worker_version TEXT,
            python_worker_build TEXT
        );
        """
    )
    payload = _snapshot_payload()
    engine_fingerprint = {
        "contract": "ai_daily_scanner",
        "protocol_version": 1,
        "binary_name": "ai-daily-scanner",
        "engine_version": "0.1.0",
        "engine_build": "sha256-source-v1:" + "e" * 64,
        "target_triple": "x86_64-pc-windows-msvc",
    }
    conn.execute(
        "INSERT INTO context_artifacts VALUES (1, ?, ?)",
        ("a" * 64, json.dumps(payload, separators=(",", ":"))),
    )
    conn.execute("INSERT INTO context_runs VALUES (7, ?, 1)", ("e" * 64,))
    conn.execute(
        "INSERT INTO scan_run_attempts VALUES (7, 1, ?, ?, ?, ?, ?, ?, ?)",
        (
            json.dumps(engine_fingerprint, separators=(",", ":")),
            "ai_daily_worker_v1",
            "0.1.0",
            "sha256-source-v1:" + "a" * 64,
            "ai_daily_worker_v1",
            "0.1.0",
            "b" * 64,
        ),
    )
    return conn


def test_capture_run_reproducibility_projects_only_portable_fields() -> None:
    conn = _provenance_db()
    try:
        evidence = capture_run_reproducibility(conn, 7)
    finally:
        conn.close()

    rendered = json.dumps(evidence, sort_keys=True)
    assert "private-name.txt" not in rendered
    assert r"D:\private" not in rendered
    assert "logical_request" not in rendered
    assert evidence["normalized_profile_sha256"]
    assert all(evidence["build_cross_checks"].values())
    assert set(evidence) == set(baseline._missing_provenance())


def _sample() -> dict:
    conn = _provenance_db()
    try:
        provenance = capture_run_reproducibility(conn, 7)
    finally:
        conn.close()
    return {
        "corpus_manifest": {
            "anonymous_manifest_sha256": "f" * 64,
            "source_count": 3,
        },
        "provenance": provenance,
    }


def test_timer_reproducibility_gates_detect_each_identity_drift() -> None:
    samples = [_sample() for _ in range(3)]
    assert baseline._reproducibility_gates(samples)["passes"] is True

    corpus_drift = copy.deepcopy(samples)
    corpus_drift[2]["corpus_manifest"]["source_count"] = 4
    assert baseline._reproducibility_gates(corpus_drift)[
        "cold_corpus_manifests_identical"
    ] is False

    build_drift = copy.deepcopy(samples)
    build_drift[2]["provenance"]["build_identity"]["engine_build"] = "x" * 64
    assert baseline._reproducibility_gates(build_drift)[
        "sample_build_identities_identical"
    ] is False

    profile_drift = copy.deepcopy(samples)
    profile_drift[2]["provenance"]["normalized_profile_sha256"] = "x" * 64
    assert baseline._reproducibility_gates(profile_drift)[
        "sample_normalized_profiles_identical"
    ] is False


def test_external_corpus_label_never_contains_path(tmp_path: Path) -> None:
    label = baseline._anonymous_corpus_label(tmp_path.resolve())

    assert label == "external-corpus"
    assert str(tmp_path.resolve()) not in label
