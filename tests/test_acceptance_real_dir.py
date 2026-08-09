"""Real-directory acceptance evidence gates stay reproducible and portable."""
from __future__ import annotations

import copy
import hashlib
import json
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))

import acceptance_real_dir as acceptance  # noqa: E402


def _profile_evidence(*, candidates: int = 384) -> tuple[str, str]:
    profile = {
        "schema_version": "scanner_profile_v2",
        "admission": {"max_candidate_files": candidates},
        "parse": {"pdf": {"max_pages": 5}},
    }
    canonical = json.dumps(
        profile,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=True,
    )
    return canonical, hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def _build_identity(
    *,
    engine_build: str = "sha256-source-v1:" + "e" * 64,
) -> dict:
    return {
        "engine_build": engine_build,
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


def _clean_sample(
    *,
    candidates: int = 384,
    engine_build: str = "sha256-source-v1:" + "e" * 64,
) -> dict:
    canonical, profile_hash = _profile_evidence(candidates=candidates)
    metadata_kind = (
        "windows_file_id_change_time_v1"
        if sys.platform == "win32"
        else "unix_inode_ctime_v1"
    )
    return {
        "anti_cheat": {
            "validated": True,
            "stage_deadline_exhausted_count": 0,
            "runtime_not_parsed_count": 0,
            "unknown_count": 0,
            "error_count": 0,
            "timeout_count": 0,
            "text_pdf_coverage": 1.0,
            "no_text_pdfplumber_invocations": 0,
            "no_text_action_label_anomaly_count": 0,
            "source_guard_unavailable_count": 0,
            "session_capability_present": True,
            "session_fallback_count": 0,
            "source_guard": {
                "policy": "source_guard_v2",
                "metrics_complete": True,
                "discovery_observed_file_count": 3,
                "kind_counts": {
                    metadata_kind: 2,
                    "content_sha256_v1": 1,
                    "unavailable": 0,
                },
                "content_hash_bytes_read": 128,
            },
            "normalized_profile_json": canonical,
            "normalized_profile_hash_algorithm": "sha256(sorted-key-json-utf8)",
            "normalized_profile_sha256": profile_hash,
            "build_identity": _build_identity(engine_build=engine_build),
            "build_cross_checks": {
                "engine_build_matches_attempt": True,
                "office_worker_build_matches_attempt": True,
                "python_worker_build_matches_attempt": True,
            },
        }
    }


def _manifests(value: str = "f" * 64) -> list[dict]:
    return [
        {"anonymous_manifest_sha256": value, "source_count": 3}
        for _ in range(3)
    ]


def test_assert_sample_rejects_no_text_action_label_anomaly() -> None:
    sample = _clean_sample()
    sample["anti_cheat"]["no_text_action_label_anomaly_count"] = 1

    with pytest.raises(AssertionError, match="NO_TEXT_ACTION_LABEL_ANOMALY"):
        acceptance._assert_sample(sample)


def test_assert_sample_rejects_incomplete_source_guard_metrics() -> None:
    sample = _clean_sample()
    sample["anti_cheat"]["source_guard"]["metrics_complete"] = False

    with pytest.raises(AssertionError, match="SOURCE_GUARD_METRICS_INCOMPLETE"):
        acceptance._assert_sample(sample)


@pytest.mark.parametrize(
    ("mutate", "failed_gate"),
    [
        (
            lambda samples, manifests: manifests[2].__setitem__(
                "source_count",
                None,
            ),
            "cold_corpus_manifests_complete",
        ),
        (
            lambda samples, manifests: manifests.__setitem__(
                2,
                {"anonymous_manifest_sha256": "a" * 64, "source_count": 3},
            ),
            "cold_corpus_manifests_identical",
        ),
        (
            lambda samples, manifests: samples[2]["anti_cheat"].__setitem__(
                "build_identity",
                _build_identity(engine_build="x" * 64),
            ),
            "cold_build_identities_identical",
        ),
        (
            lambda samples, manifests: samples[2]["anti_cheat"].__setitem__(
                "normalized_profile_sha256",
                _profile_evidence(candidates=385)[1],
            ),
            "cold_normalized_profiles_identical",
        ),
    ],
)
def test_cold_reproducibility_gates_fail_on_sample_drift(
    mutate,
    failed_gate: str,
) -> None:
    samples = [_clean_sample() for _ in range(3)]
    manifests = _manifests()
    mutate(samples, manifests)

    gates = acceptance._cold_reproducibility_gates(
        samples,
        manifests,
        frozen_manifest_sha256="f" * 64,
    )

    assert gates[failed_gate] is False
    assert gates["passes"] is False


def test_cold_reproducibility_gates_fail_when_frozen_corpus_changed() -> None:
    gates = acceptance._cold_reproducibility_gates(
        [_clean_sample() for _ in range(3)],
        _manifests("a" * 64),
        frozen_manifest_sha256="f" * 64,
    )

    assert gates["corpus_matches_frozen_manifest"] is False
    assert gates["passes"] is False


def test_cold_reproducibility_gates_pass_for_three_exact_samples() -> None:
    samples = [_clean_sample() for _ in range(3)]
    manifests = _manifests()

    gates = acceptance._cold_reproducibility_gates(
        samples,
        manifests,
        frozen_manifest_sha256="f" * 64,
    )

    assert gates["passes"] is True


def test_portable_evidence_rejects_windows_absolute_path() -> None:
    with pytest.raises(ValueError, match="absolute path"):
        acceptance._assert_portable_evidence(
            {"error": r"failed to read D:\private\customer-name.pdf"}
        )


def test_failed_sample_evidence_has_fixed_path_free_shape() -> None:
    evidence = acceptance._failed_sample_evidence(
        session_capability_present=True,
        source_guard_policy="source_guard_v2",
        collection_error_code="EVIDENCE_COLLECTION_FAILED",
    )

    assert evidence["collection_error_code"] == "EVIDENCE_COLLECTION_FAILED"
    assert evidence["source_guard"]["policy"] == "source_guard_v2"
    assert evidence["normalized_profile_json"] is None
    acceptance._assert_portable_evidence(copy.deepcopy(evidence))
