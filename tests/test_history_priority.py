from src.services.coverage_repair import FilePin, RepairDate, history_priority_paths


def test_history_priority_requires_the_same_original_file_hash():
    identical = FilePin(relative_path="unchanged.xlsx", size_bytes=1, mtime_ns=1,
        prior_content_sha256="1" * 64, content_sha256="1" * 64)
    changed = identical.model_copy(update={"relative_path": "changed.xlsx", "content_sha256": "2" * 64})
    unknown = identical.model_copy(update={"relative_path": "unknown.xlsx", "prior_content_sha256": None})
    item = RepairDate(date="2026-09-22", gap_codes=["EXCERPT_BOUNDED"],
        old_run_id="1" * 32, old_sha256="1" * 64, old_quality="partial",
        files=[unknown, changed, identical])
    assert history_priority_paths(item) == ["unchanged.xlsx"]
