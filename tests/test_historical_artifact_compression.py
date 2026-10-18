"""Synthetic NTFS maintenance tests; never query or change production tasks/data."""

import contextlib
import ctypes
from ctypes import wintypes
from datetime import datetime, timedelta, timezone
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/compress_historical_artifacts.py"
ALLOWED = (
    ".artifacts/rust-migration/archive-benchmark-final-20261017-04/incremental/data/archive/extractions",
    ".artifacts/rust-migration/archive-benchmark-final-20261017/incremental/data/archive/extractions",
)
EVIDENCE = ".artifacts/rust-migration/historical-artifact-compression"
NAMES = ("body.md", "units.jsonl", "units.idx")
class BatchClock(datetime):
    @classmethod
    def now(cls, tz=None):
        return cls(2035, 1, 2, 23, 59, 59, tzinfo=timezone.utc)


NOW = datetime(2026, 10, 18, 7, 0, tzinfo=timezone.utc)
TASK_NAMES = ("AI Daily Report", "AI Daily Report Collect", "AI Daily Report Health")


def load_tool():
    if not SCRIPT.is_file():
        return None
    spec = importlib.util.spec_from_file_location("historical_artifact_compression", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


tool = load_tool()


class Fixture:
    def __init__(self):
        self.temp = tempfile.TemporaryDirectory(prefix="historical-compression-task1-")
        self.root = Path(self.temp.name) / "source"
        self.root.mkdir()
        self.expected_paths = set()
        for index, allowed in enumerate(ALLOWED):
            extraction = self.root / allowed / "synthetic"
            extraction.mkdir(parents=True)
            for name in NAMES:
                path = extraction / name
                path.write_bytes((f"synthetic-{index}-{name}\n".encode()) * 17)
                self.expected_paths.add(path.relative_to(self.root).as_posix())
            (extraction / "other.json").write_bytes(b"not a target")
        self.outside = self.root / ".artifacts/rust-migration/unrelated/body.md"
        self.outside.parent.mkdir(parents=True)
        self.outside.write_bytes(b"outside allowlist")
        (self.root / "body.md").write_bytes(b"root is outside allowlist")
        self.evidence = self.root / EVIDENCE / "fresh-plan"
        self.window_patch = mock.patch.object(
            tool, "read_task_window", side_effect=lambda now: (False, now + timedelta(hours=1))
        )
        self.window_patch.start()
        self.junctions = []

    def close(self):
        self.window_patch.stop()
        for junction in reversed(self.junctions):
            if junction.exists():
                os.rmdir(junction)
        self.temp.cleanup()

    def source_fingerprints(self):
        result = {}
        for path in self.root.rglob("*"):
            if path.is_file() and not path.is_relative_to(self.root / EVIDENCE):
                stat = path.stat()
                result[path.relative_to(self.root).as_posix()] = (
                    path.read_bytes(), stat.st_size, stat.st_mtime_ns, stat.st_file_attributes
                )
        return result

    def junction(self, path, target):
        path.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            result = subprocess.run(
                [os.environ.get("COMSPEC", r"C:\Windows\System32\cmd.exe"),
                 "/d", "/c", "mklink", "/J", str(path), str(target)],
                stdout=stdout, stderr=stderr, check=False,
            )
            stdout.seek(0)
            stderr.seek(0)
            if result.returncode != 0:
                raise AssertionError(f"junction coverage unavailable: {stdout.read()!r} {stderr.read()!r}")
        self.junctions.append(path)
        return path

    def rewrite_frozen_manifest(self, path, manifest):
        raw = (json.dumps(manifest, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
        path.write_bytes(raw)
        path.with_suffix(".sha256").write_bytes((hashlib.sha256(raw).hexdigest() + "\n").encode("ascii"))


def compress_fixture_file(path):
    """Only the test fixture owns this mutation; production plan stays read-only."""
    import msvcrt
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.DeviceIoControl.argtypes = [
        wintypes.HANDLE, wintypes.DWORD, wintypes.LPVOID, wintypes.DWORD,
        wintypes.LPVOID, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID,
    ]
    kernel.DeviceIoControl.restype = wintypes.BOOL
    with path.open("r+b") as stream:
        compression = wintypes.USHORT(2)
        returned = wintypes.DWORD()
        if not kernel.DeviceIoControl(
            msvcrt.get_osfhandle(stream.fileno()), 0x9C040,
            ctypes.byref(compression), ctypes.sizeof(compression), None, 0,
            ctypes.byref(returned), None,
        ):
            raise ctypes.WinError(ctypes.get_last_error())


class SyntheticTests(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(tool, "plan maintenance script is not implemented")
        self.f = Fixture()
        self.addCleanup(self.f.close)

    def assert_code(self, expected, operation):
        with self.assertRaises(tool.ToolError) as raised:
            operation()
        self.assertEqual(raised.exception.code, expected, str(raised.exception))

    def require_task2(self):
        for name in ("apply_manifest", "verify_manifest"):
            self.assertTrue(callable(getattr(tool, name, None)), f"{name} is missing")

    def freeze(self):
        manifest = tool.scan_manifest(self.f.root)
        return tool.write_manifest(manifest, self.f.evidence), manifest

    def fingerprint(self, path):
        info = path.stat()
        return (hashlib.sha256(path.read_bytes()).hexdigest(), info.st_size, info.st_mtime_ns,
                info.st_file_attributes, info.st_nlink, info.st_ino)


class ScopeTests(SyntheticTests):
    # Break caught: enumerating anything outside the two exact roots/names or writing sources.
    def test_plan_has_exact_scope_and_never_mutates_sources(self):
        before = self.f.source_fingerprints()
        manifest = tool.scan_manifest(self.f.root)
        self.assertEqual({e["relative_path"] for e in manifest["eligible"]}, self.f.expected_paths)
        self.assertTrue(all(e["nlink"] == 1 and not e["was_compressed"] for e in manifest["eligible"]))
        self.assertEqual(manifest["allowed_roots"], list(ALLOWED))
        self.assertEqual(manifest["totals"]["eligible_files"], 6)
        for entry in manifest["eligible"]:
            data = (self.f.root / entry["relative_path"]).read_bytes()
            self.assertEqual(entry["sha256"], hashlib.sha256(data).hexdigest())
            self.assertEqual(entry["logical_bytes"], len(data))
            self.assertGreaterEqual(entry["allocated_before_bytes"], entry["logical_bytes"])
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: using zero-valued DirEntry identities or reusing the same ID for all files.
    def test_distinct_files_have_nonzero_distinct_ids(self):
        ids = [entry["file_id"] for entry in tool.scan_manifest(self.f.root)["eligible"]]
        self.assertEqual(len({(item["volume_serial"], item["file_index"]) for item in ids}), 6)
        self.assertTrue(all(item["file_index"] != 0 for item in ids))

    # Break caught: accepting shared links or protected/compressed files as future write candidates.
    def test_hardlink_encrypted_and_precompressed_files_are_skipped(self):
        first = self.f.root / ALLOWED[0] / "synthetic"
        os.link(first / "body.md", self.f.root / "shared-link.md")
        compress_fixture_file(first / "units.idx")
        original = tool._file_metadata

        def encrypted_metadata(stream):
            result = original(stream)
            if Path(stream.name) == first / "units.jsonl":
                result["attributes"] |= 0x4000
            return result

        before = self.f.source_fingerprints()
        with mock.patch.object(tool, "_file_metadata", side_effect=encrypted_metadata):
            manifest = tool.scan_manifest(self.f.root)
        self.assertEqual({entry["reason"] for entry in manifest["skipped"]},
                         {"hardlink", "encrypted", "already_compressed"})
        self.assertEqual(len(manifest["eligible"]), 3)
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: following internal junctions and including the external body's bytes.
    def test_internal_reparse_is_recorded_without_entering(self):
        linked = self.f.root / ALLOWED[0] / "linked"
        self.f.junction(linked, self.f.outside.parent)
        manifest = tool.scan_manifest(self.f.root)
        self.assertEqual({e["relative_path"] for e in manifest["eligible"]}, self.f.expected_paths)
        self.assertIn({"relative_path": linked.relative_to(self.f.root).as_posix(), "reason": "reparse"},
                      manifest["skipped"])

    # Break caught: resolving a reparse source/allowed-root ancestor before validating it.
    def test_reparse_parent_and_path_escape_are_rejected(self):
        alias = Path(self.f.temp.name) / "source-alias"
        self.f.junction(alias, self.f.root)
        self.assert_code("SCOPE_REJECTED", lambda: tool.scan_manifest(alias))
        manifest_path = tool.write_manifest(tool.scan_manifest(self.f.root), self.f.evidence)
        for relative in ("../body.md", "D:/body.md", ALLOWED[0] + "/../body.md",
                         ALLOWED[0] + "\\body.md", ALLOWED[0] + "/body.md:stream",
                         ALLOWED[0] + "//body.md", ".artifacts/rust-migration/unrelated/body.md"):
            with self.subTest(relative=relative):
                manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
                manifest["eligible"][0]["relative_path"] = relative
                self.f.rewrite_frozen_manifest(manifest_path, manifest)
                self.assert_code("SCOPE_REJECTED", lambda: tool.load_manifest(manifest_path))

    # Break caught: a permitted root's parent is exchanged for a junction before scanning.
    def test_allowed_directory_ancestor_reparse_is_rejected(self):
        allowed = self.f.root / ALLOWED[0]
        actual = allowed.parent / "real-extractions"
        allowed.rename(actual)
        self.f.junction(allowed, actual)
        self.assert_code("SCOPE_REJECTED", lambda: tool.scan_manifest(self.f.root))

    # Break caught: publishing a file that changes between its metadata and hash reads.
    def test_file_changed_while_hashing_is_skipped(self):
        target = self.f.root / sorted(self.f.expected_paths)[0]
        original_stat = target.stat()
        changed_mtime = original_stat.st_mtime_ns + 1_000_000_000
        original = tool._file_metadata
        changed = False

        def changing_metadata(stream):
            nonlocal changed
            result = original(stream)
            if Path(stream.name) == target and not changed:
                os.utime(target, ns=(original_stat.st_atime_ns, changed_mtime))
                changed = True
            return result

        with mock.patch.object(tool, "_file_metadata", side_effect=changing_metadata):
            manifest = tool.scan_manifest(self.f.root)
        self.assertEqual(target.stat().st_mtime_ns, changed_mtime)
        self.assertIn({"relative_path": target.relative_to(self.f.root).as_posix(), "reason": "unstable"},
                      manifest["skipped"])
        self.assertNotIn(target.relative_to(self.f.root).as_posix(),
                         {e["relative_path"] for e in manifest["eligible"]})

    # Break caught: accepting a hash sidecar that no longer matches the exact published bytes.
    def test_modified_manifest_is_rejected(self):
        path = tool.write_manifest(tool.scan_manifest(self.f.root), self.f.evidence)
        path.write_bytes(path.read_bytes() + b" ")
        self.assert_code("MANIFEST_CHANGED", lambda: tool.load_manifest(path))

    # Break caught: silently interpreting unknown fields, unsupported versions, or malformed hashes.
    def test_unknown_manifest_fields_versions_hashes_and_duplicates_are_rejected(self):
        path = tool.write_manifest(tool.scan_manifest(self.f.root), self.f.evidence)
        original = json.loads(path.read_text(encoding="utf-8"))
        for change in (lambda m: m.update(extra=True), lambda m: m.update(schema_version=2),
                       lambda m: m["eligible"][0].update(extra=True),
                       lambda m: m["eligible"][0].update(sha256="z" * 64),
                       lambda m: m["eligible"].append(dict(m["eligible"][0])),
                       lambda m: m["totals"].update(eligible_files=99),
                       lambda m: m["eligible"][0].update(nlink=2),
                       lambda m: m["eligible"][0].update(was_compressed=True)):
            manifest = json.loads(json.dumps(original))
            change(manifest)
            self.f.rewrite_frozen_manifest(path, manifest)
            self.assert_code("MANIFEST_CHANGED", lambda: tool.load_manifest(path))

    # Break caught: overwriting existing evidence, writing outside evidence root, or following a junction.
    def test_evidence_directory_is_new_and_strictly_scoped(self):
        manifest = tool.scan_manifest(self.f.root)
        before = self.f.source_fingerprints()
        for invalid in (self.f.root / "evidence", self.f.root / ALLOWED[0] / "evidence",
                        Path(self.f.temp.name) / "outside-evidence"):
            self.assert_code("SCOPE_REJECTED", lambda: tool.write_manifest(manifest, invalid))
            self.assertFalse(invalid.exists())
        path = tool.write_manifest(manifest, self.f.evidence)
        frozen = path.read_bytes()
        self.assert_code("EVIDENCE_EXISTS", lambda: tool.write_manifest(manifest, self.f.evidence))
        self.assertEqual(path.read_bytes(), frozen)
        self.assertEqual(self.f.source_fingerprints(), before)
        linked = self.f.root / EVIDENCE / "linked"
        self.f.junction(linked, self.f.outside.parent)
        self.assert_code("SCOPE_REJECTED", lambda: tool.write_manifest(manifest, linked / "new"))
        self.assertFalse((self.f.outside.parent / "new").exists())

    # Break caught: loading a copied frozen manifest from a location apply must never trust.
    def test_load_requires_original_evidence_location_and_no_reparse(self):
        path = tool.write_manifest(tool.scan_manifest(self.f.root), self.f.evidence)
        copied = self.f.root / "manifest.json"
        copied.write_bytes(path.read_bytes())
        copied.with_suffix(".sha256").write_bytes(path.with_suffix(".sha256").read_bytes())
        self.assert_code("SCOPE_REJECTED", lambda: tool.load_manifest(copied))
        entry = json.loads(path.read_text(encoding="utf-8"))["eligible"][0]
        parent = (self.f.root / entry["relative_path"]).parent
        destination = parent.with_name("replaced-directory")
        parent.rename(destination)
        self.f.junction(parent, destination)
        self.assert_code("SCOPE_REJECTED", lambda: tool.load_manifest(path))

    # Break caught: serializing/re-reading changes the frozen document or omits the byte hash.
    def test_frozen_manifest_round_trip_preserves_complete_schema(self):
        manifest = tool.scan_manifest(self.f.root)
        path = tool.write_manifest(manifest, self.f.evidence)
        self.assertEqual(path.name, "manifest.json")
        self.assertEqual(tool.load_manifest(path), manifest)
        self.assertEqual(path.with_suffix(".sha256").read_text(encoding="ascii").strip(),
                         hashlib.sha256(path.read_bytes()).hexdigest())
        self.assertEqual(set(manifest), {"schema_version", "plan_id", "created_at_utc", "project_root",
                                         "allowed_roots", "eligible", "skipped", "totals"})
        self.assertEqual(set(manifest["eligible"][0]), {"relative_path", "file_id", "nlink", "logical_bytes",
                         "mtime_ns", "attributes", "was_compressed", "sha256", "allocated_before_bytes"})


class EvidenceRaceTests(SyntheticTests):
    def assert_swap_is_blocked(self, node, gate):
        manifest = tool.scan_manifest(self.f.root)
        self.f.evidence.parent.mkdir(parents=True, exist_ok=True)
        outside = Path(self.f.temp.name) / "outside-evidence"
        outside.mkdir()
        (outside / "sentinel.txt").write_bytes(b"outside evidence must be untouched")
        if gate == 2:
            (outside / self.f.evidence.relative_to(node)).mkdir(parents=True, exist_ok=True)
        old = self.f.evidence.with_name("old-plan") / "manifest.json"
        old.parent.mkdir()
        old.write_bytes(b"preserve old evidence")

        def outside_snapshot():
            return {path.relative_to(outside).as_posix(): path.read_bytes() if path.is_file() else None
                    for path in outside.rglob("*")}

        before = outside_snapshot()
        displaced = node.with_name(node.name + "-displaced")
        calls = 0
        blocked = None
        swapped = False
        result = None

        def window(now):
            nonlocal calls, blocked, swapped
            calls += 1
            if calls == gate:
                try:
                    node.rename(displaced)
                except OSError as exc:
                    blocked = exc.winerror
                    if blocked not in (5, 32):
                        raise
                else:
                    swapped = True
                    self.f.junction(node, outside)
            return False, now + timedelta(hours=1)

        with mock.patch.object(tool, "read_task_window", side_effect=window):
            try:
                result = tool.write_manifest(manifest, self.f.evidence)
            except (tool.ToolError, OSError):
                # A rejection after an escaped mkdir is still a failure below.
                pass
        self.assertEqual(outside_snapshot(), before, "publication created bytes/directories outside evidence scope")
        self.assertIn(blocked, (5, 32), "the directory rename must be denied by a live stable handle")
        self.assertFalse(swapped)
        self.assertEqual(calls, 2)
        self.assertEqual(result, self.f.evidence / "manifest.json")
        self.assertEqual(tool.load_manifest(result), manifest)
        self.assertEqual(old.read_bytes(), b"preserve old evidence")
        frozen = result.read_bytes()
        self.assert_code("EVIDENCE_EXISTS", lambda: tool.write_manifest(manifest, self.f.evidence))
        self.assertEqual(result.read_bytes(), frozen)

    # Break caught: a parent swap during the first query creates a directory outside the evidence root.
    def test_evidence_parent_swap_before_mkdir_cannot_create_outside(self):
        self.assert_swap_is_blocked(self.f.evidence.parent, 1)

    # Break caught: a leaf swap after writing the sidecar publishes manifest.json into a junction target.
    def test_evidence_directory_swap_before_publication_cannot_write_outside(self):
        self.assert_swap_is_blocked(self.f.evidence, 2)

    # Break caught: locking only the leaf leaves a SourceRoot ancestor replaceable during publication.
    def test_source_ancestor_swap_before_publication_cannot_write_outside(self):
        self.assert_swap_is_blocked(self.f.root, 2)


class WindowTests(SyntheticTests):
    def check_pause(self, window, code):
        with mock.patch.object(tool, "read_task_window", side_effect=window if isinstance(window, Exception)
                               else lambda now: window(now)):
            self.assert_code(code, lambda: tool.scan_manifest(self.f.root))
        self.assertFalse(self.f.evidence.exists())

    # Break caught: treating exactly 300 seconds as safe or silently carrying on after query errors.
    def test_running_query_failure_and_300_second_boundary_pause(self):
        self.check_pause(lambda now: (True, now + timedelta(hours=1)), "TASK_RUNNING")
        self.check_pause(RuntimeError("query unavailable"), "TASK_QUERY_FAILED")
        self.check_pause(lambda now: (False, now + timedelta(seconds=300)), "TASK_IMMINENT")
        self.check_pause(lambda now: (False, now - timedelta(seconds=1)), "TASK_IMMINENT")
        with mock.patch.object(tool, "read_task_window", side_effect=lambda now: (False, now + timedelta(seconds=301))):
            self.assertEqual(len(tool.scan_manifest(self.f.root)["eligible"]), 6)

    # Break caught: finishing a plan even though the task window closed after enumeration.
    def test_window_rechecked_before_publication(self):
        manifest = tool.scan_manifest(self.f.root)
        with mock.patch.object(tool, "read_task_window", side_effect=lambda now: (True, now + timedelta(hours=1))):
            self.assert_code("TASK_RUNNING", lambda: tool.write_manifest(manifest, self.f.evidence))
        self.assertFalse(self.f.evidence.exists())

    # Break caught: scanning more than 100 files without returning to the scheduler gate.
    def test_scan_pauses_at_100_file_boundary(self):
        for index in range(105):
            path = self.f.root / ALLOWED[0] / f"batch-{index:03}" / "body.md"
            path.parent.mkdir()
            path.write_bytes(b"synthetic")
        original = tool._file_metadata
        observed = set()

        def metadata(stream):
            observed.add(str(stream.name))
            return original(stream)

        windows = iter((False, True))
        with mock.patch.object(tool, "datetime", BatchClock), \
             mock.patch.object(tool, "_file_metadata", side_effect=metadata), \
             mock.patch.object(tool, "read_task_window", side_effect=lambda now: (next(windows), now + timedelta(hours=1))):
            self.assert_code("TASK_RUNNING", lambda: tool.scan_manifest(self.f.root))
        self.assertEqual(len(observed), 100)

    # Break caught: ignoring the byte threshold when fewer than 100 files are present.
    def test_scan_pauses_at_256_mib_without_truncating_large_file(self):
        first = self.f.root / ALLOWED[0] / "000-large" / "body.md"
        first.parent.mkdir()
        with first.open("wb") as stream:
            stream.truncate(268_435_456 + 123)
        original = tool._file_metadata
        observed = set()

        def metadata(stream):
            observed.add(str(stream.name))
            return original(stream)

        windows = iter((False, True))
        with mock.patch.object(tool, "datetime", BatchClock), \
             mock.patch.object(tool, "_file_metadata", side_effect=metadata), \
             mock.patch.object(tool, "read_task_window", side_effect=lambda now: (next(windows), now + timedelta(hours=1))):
            self.assert_code("TASK_RUNNING", lambda: tool.scan_manifest(self.f.root))
        self.assertEqual(observed, {str(first)})
        self.assertEqual(first.stat().st_size, 268_435_579)

    # Break caught: a byte checkpoint moves the independent every-100-files checkpoint.
    def test_byte_checkpoint_does_not_reset_100_file_boundary(self):
        first = self.f.root / ALLOWED[0] / "000-large" / "body.md"
        first.parent.mkdir()
        with first.open("wb") as stream:
            stream.truncate(268_435_456 + 123)
        for index in range(105):
            path = self.f.root / ALLOWED[0] / f"batch-{index:03}" / "body.md"
            path.parent.mkdir()
            path.write_bytes(b"synthetic")
        original = tool._file_metadata
        observed = set()

        def metadata(stream):
            observed.add(str(stream.name))
            return original(stream)

        windows = iter((False, False, True))
        with mock.patch.object(tool, "datetime", BatchClock), \
             mock.patch.object(tool, "_file_metadata", side_effect=metadata), \
             mock.patch.object(tool, "read_task_window", side_effect=lambda now: (next(windows), now + timedelta(hours=1))):
            self.assert_code("TASK_RUNNING", lambda: tool.scan_manifest(self.f.root))
        self.assertEqual(len(observed), 100)

    # Break caught: the 100-file gate discards bytes already counted toward the 256 MiB gate.
    def test_file_checkpoint_does_not_reset_byte_boundary(self):
        for name in ("000-large", "batch-999-large"):
            path = self.f.root / ALLOWED[0] / name / "body.md"
            path.parent.mkdir()
            with path.open("wb") as stream:
                stream.truncate(134_217_728)
        for index in range(99):
            path = self.f.root / ALLOWED[0] / f"batch-{index:03}" / "body.md"
            path.parent.mkdir()
            path.write_bytes(b"synthetic")
        original = tool._file_metadata
        observed = set()

        def metadata(stream):
            observed.add(str(stream.name))
            return original(stream)

        windows = iter((False, False, True))
        with mock.patch.object(tool, "datetime", BatchClock), \
             mock.patch.object(tool, "_file_metadata", side_effect=metadata), \
             mock.patch.object(tool, "read_task_window", side_effect=lambda now: (next(windows), now + timedelta(hours=1))):
            self.assert_code("TASK_RUNNING", lambda: tool.scan_manifest(self.f.root))
        self.assertEqual(len(observed), 101)

    # Break caught: a slow scheduler query consumes the last safe second, but scanning still begins.
    def test_query_elapsed_time_is_included_in_300_second_margin(self):
        start = datetime(2026, 10, 18, 7, 0, tzinfo=timezone.utc)
        instants = iter((start, start + timedelta(seconds=2)))

        class QueryClock(datetime):
            @classmethod
            def now(cls, tz=None):
                value = next(instants)
                return cls.fromisoformat(value.isoformat())

        upcoming = QueryClock.fromisoformat((start + timedelta(seconds=301)).isoformat())
        with mock.patch.object(tool, "datetime", QueryClock), \
             mock.patch.object(tool, "read_task_window", return_value=(False, upcoming)):
            self.assert_code("TASK_IMMINENT", tool._check_task_window)

    # Break caught: apply writes at exactly 300s, or on running/missing/failed actual query records.
    def test_cutoff_running_missing_and_failed_probe_pause(self):
        self.require_task2()
        path, manifest = self.freeze()
        before = self.f.source_fingerprints()
        self.f.window_patch.stop()
        start = BatchClock.now(timezone.utc)
        rows = [{"task_name": name, "state": 3,
                 "next_run_time_utc": (start + timedelta(hours=1)).isoformat()} for name in TASK_NAMES]
        imminent = json.loads(json.dumps(rows))
        imminent[0]["next_run_time_utc"] = (start + timedelta(seconds=300)).isoformat()
        running = json.loads(json.dumps(rows))
        running[1]["state"] = 4
        queued = json.loads(json.dumps(rows))
        queued[2]["state"] = 2
        for data, exitcode, code in ((imminent, 0, "TASK_IMMINENT"), (running, 0, "TASK_RUNNING"),
                                     (queued, 0, "TASK_RUNNING"), (rows[:2], 0, "TASK_QUERY_FAILED"),
                                     (rows, 1, "TASK_QUERY_FAILED")):
            with self.subTest(code=code), mock.patch.object(tool, "datetime", BatchClock), \
                 mock.patch.object(tool.subprocess, "run", side_effect=self.scheduler_process(data, exitcode)):
                result = tool.apply_manifest(path)
            self.assertEqual((result["status"], result["error_code"]), ("paused", code), result)
            self.assertEqual(result["files_changed"], 0)
            self.assertEqual(self.f.source_fingerprints(), before)
            run_dir = path.parent / "runs" / result["run_id"]
            self.assertTrue((run_dir / "summary.json").is_file(), "paused run summary is missing")
            self.assertEqual(json.loads((run_dir / "summary.json").read_text(encoding="utf-8")), result)
        rows[0]["next_run_time_utc"] = (start + timedelta(seconds=301)).isoformat()
        with mock.patch.object(tool, "datetime", BatchClock), \
             mock.patch.object(tool.subprocess, "run", side_effect=self.scheduler_process(rows)):
            result = tool.apply_manifest(path)
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual(result["files_changed"], 6)

    # Break caught: a successful first file consumes the safe second and the next file still starts.
    def test_each_file_checks_known_cutoff_without_another_task_query(self):
        self.require_task2()
        path, manifest = self.freeze()
        paths = [self.f.root / entry["relative_path"] for entry in manifest["eligible"]]
        before = {item: self.fingerprint(item) for item in paths[1:]}
        start = BatchClock.now(timezone.utc)
        current = [start]
        queries = []
        native = tool.set_compression

        class MovingClock(datetime):
            @classmethod
            def now(cls, tz=None):
                return cls.fromisoformat(current[0].isoformat())

        def window(now):
            queries.append(now)
            return False, MovingClock.fromisoformat((start + timedelta(seconds=301)).isoformat())

        def compress(stream, enabled):
            native(stream, enabled)
            current[0] += timedelta(seconds=1)

        with mock.patch.object(tool, "datetime", MovingClock), \
             mock.patch.object(tool, "read_task_window", side_effect=window), \
             mock.patch.object(tool, "set_compression", side_effect=compress):
            result = tool.apply_manifest(path)
        self.assertEqual((result["status"], result["pause_reason"]), ("paused", "TASK_IMMINENT"))
        self.assertEqual((result["files_verified"], result["files_changed"]), (1, 1))
        self.assertEqual(len(queries), 1)
        self.assertTrue(paths[0].stat().st_file_attributes & 0x800)
        self.assertEqual({item: self.fingerprint(item) for item in paths[1:]}, before)

    # Break caught: a scheduler gate is skipped after the 100th file or skipped files are counted as changed.
    def test_apply_requeries_tasks_at_100_files(self):
        self.require_task2()
        for index in range(101):
            item = self.f.root / ALLOWED[0] / f"batch-{index:03}/body.md"
            item.parent.mkdir()
            item.write_bytes(b"real batch content\n" * 300)
        path, manifest = self.freeze()
        windows = iter((False, True))
        with mock.patch.object(tool, "datetime", BatchClock), \
             mock.patch.object(tool, "read_task_window", side_effect=lambda now: (next(windows), now + timedelta(hours=1))):
            result = tool.apply_manifest(path)
        self.assertEqual((result["status"], result["pause_reason"]), ("paused", "TASK_RUNNING"), result)
        self.assertEqual((result["files_verified"], result["files_changed"]), (100, 100))
        states = [bool((self.f.root / entry["relative_path"]).stat().st_file_attributes & 0x800)
                  for entry in manifest["eligible"]]
        self.assertEqual(states, [True] * 100 + [False] * 7)

    # Break caught: a large file is truncated/sampled, or the first 256MiB trial proceeds without a fresh gate.
    def test_apply_trial_reads_full_large_file_then_pauses_before_next(self):
        self.require_task2()
        first = self.f.root / ALLOWED[0] / "000-large/body.md"
        first.parent.mkdir()
        size = 268_435_456 + 123
        with first.open("wb") as stream:
            stream.truncate(size)
            stream.seek(1_048_577)
            stream.write(b"middle-content-outside-the-three-samples")
            stream.seek(size - 120)
            stream.write(b"final-content")
        with first.open("rb") as stream:
            expected_hash = hashlib.file_digest(stream, "sha256").hexdigest()
        path, manifest = self.freeze()
        later = {self.f.root / entry["relative_path"]: self.fingerprint(self.f.root / entry["relative_path"])
                 for entry in manifest["eligible"][1:]}
        windows = iter((False, True))
        with mock.patch.object(tool, "datetime", BatchClock), \
             mock.patch.object(tool, "read_task_window", side_effect=lambda now: (next(windows), now + timedelta(hours=1))):
            result = tool.apply_manifest(path)
        self.assertEqual((result["status"], result["pause_reason"]), ("paused", "TASK_RUNNING"), result)
        self.assertEqual((result["files_verified"], result["files_changed"], result["logical_bytes"]), (1, 1, size))
        with first.open("rb") as stream:
            self.assertEqual(hashlib.file_digest(stream, "sha256").hexdigest(), expected_hash)
        self.assertEqual(first.stat().st_size, size)
        self.assertTrue(first.stat().st_file_attributes & 0x800)
        self.assertEqual({item: self.fingerprint(item) for item in later}, later)
        events = [json.loads(line) for line in (path.parent / "runs" / result["run_id"] / "events.jsonl").read_text(encoding="utf-8").splitlines()]
        trials = [event for event in events if event["event"] == "trial_completed"]
        self.assertEqual(len(trials), 1)
        self.assertEqual((trials[0]["files_verified"], trials[0]["logical_bytes"]), (1, size))

    def task_rows(self):
        return [
            {"task_name": TASK_NAMES[0], "state": 3, "next_run_time_utc": "2026-10-18T16:00:00+08:00"},
            {"task_name": TASK_NAMES[1], "state": 3, "next_run_time_utc": "2026-10-18T07:30:00Z"},
            {"task_name": TASK_NAMES[2], "state": 3, "next_run_time_utc": "2026-10-18T15:00:00Z"},
        ]

    def scheduler_process(self, rows, returncode=0):
        def run(args, *, stdout, stderr, **kwargs):
            self.assertIsInstance(args, list)
            self.assertEqual(Path(args[0]).name.lower(), "powershell.exe")
            self.assertNotEqual(stdout, subprocess.PIPE)
            self.assertNotEqual(stderr, subprocess.PIPE)
            stdout.write(json.dumps(rows).encode("utf-8"))
            stdout.flush()
            return subprocess.CompletedProcess(args, returncode)
        return run

    # Break caught: selecting a later trigger or interpreting Shanghai local time as UTC.
    def test_task_query_uses_all_three_tasks_and_utc_earliest_trigger(self):
        self.f.window_patch.stop()
        rows = self.task_rows()
        with mock.patch.object(tool.subprocess, "run", side_effect=self.scheduler_process(rows)):
            running, upcoming = tool.read_task_window(NOW)
        self.assertFalse(running)
        self.assertEqual(upcoming, datetime(2026, 10, 18, 7, 30, tzinfo=timezone.utc))
        rows[2]["state"] = 4
        with mock.patch.object(tool.subprocess, "run", side_effect=self.scheduler_process(rows)):
            self.assertTrue(tool.read_task_window(NOW)[0])

    # Break caught: assuming idle when any task is absent or task output cannot be verified.
    def test_missing_duplicate_invalid_tasks_or_process_failure_are_unverifiable(self):
        self.f.window_patch.stop()
        rows = self.task_rows()
        invalid_time = self.task_rows()
        invalid_time[0]["next_run_time_utc"] = "2026-10-18T08:00:00"
        invalid_state = self.task_rows()
        invalid_state[0]["state"] = 0
        for data, exitcode in ((rows[:2], 0), (rows + rows[:1], 0), (invalid_time, 0),
                               (invalid_state, 0), (rows, 1), (None, 0)):
            with self.subTest(data=data, exitcode=exitcode), \
                 mock.patch.object(tool.subprocess, "run", side_effect=self.scheduler_process(data, exitcode)):
                self.assert_code("TASK_QUERY_FAILED", lambda: tool.read_task_window(NOW))


class CliTests(SyntheticTests):
    # Break caught: plan fails to publish usable evidence or reports changes it never made.
    def test_plan_command_writes_frozen_evidence_and_completed_summary(self):
        before = self.f.source_fingerprints()
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            exitcode = tool.main(["plan", "--project-root", str(self.f.root), "--evidence-dir", str(self.f.evidence)])
        result = json.loads(output.getvalue())
        self.assertEqual(exitcode, 0)
        self.assertEqual(result["status"], "completed")
        self.assertEqual(result["files_verified"], 6)
        self.assertEqual(result["files_changed"], 0)
        self.assertEqual(result["reclaimed_bytes"], 0)
        self.assertEqual(len(tool.load_manifest(Path(result["manifest"]))["eligible"]), 6)
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: a paused scan publishes a manifest that later commands could execute.
    def test_plan_command_returns_paused_without_executable_manifest(self):
        output = io.StringIO()
        with mock.patch.object(tool, "read_task_window", side_effect=OSError("offline fixture")), \
             contextlib.redirect_stdout(output):
            exitcode = tool.main(["plan", "--project-root", str(self.f.root), "--evidence-dir", str(self.f.evidence)])
        result = json.loads(output.getvalue())
        self.assertEqual(exitcode, 1)
        self.assertEqual(result["status"], "paused")
        self.assertEqual(result["pause_reason"], "TASK_QUERY_FAILED")
        self.assertFalse((self.f.evidence / "manifest.json").exists())

    # Break caught: an invalid output location is created, or failure uses the wrong exit status.
    def test_invalid_evidence_returns_failed_without_creating_output(self):
        invalid = self.f.root / "invalid-output"
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            exitcode = tool.main(["plan", "--project-root", str(self.f.root), "--evidence-dir", str(invalid)])
        result = json.loads(output.getvalue())
        self.assertEqual(exitcode, 2)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["error_code"], "SCOPE_REJECTED")
        self.assertFalse(invalid.exists())


class NativeTests(SyntheticTests):
    # Break caught: native compression changes bytes/seek results/identity/mtime, or never compresses.
    def test_compression_preserves_hash_seek_identity_and_mtime(self):
        self.assertTrue(callable(getattr(tool, "set_compression", None)), "native compression is missing")
        target = self.f.root / ALLOWED[0] / "synthetic/body.md"
        data = b"transparent native compression\n" * 69_906
        data = (data + b"x" * 2_097_152)[:2_097_152]
        target.write_bytes(data)
        stamp = 1_700_000_000_123_456_700
        os.utime(target, ns=(stamp, stamp))
        offsets = (0, len(data) // 3, len(data) - 4096)
        with target.open("r+b", buffering=0) as stream:
            before = tool._file_metadata(stream)
            digest_before = hashlib.sha256(stream.read()).hexdigest()
            blocks_before = []
            for offset in offsets:
                stream.seek(offset)
                blocks_before.append(stream.read(4096))
            tool.set_compression(stream, True)
            after = tool._file_metadata(stream)
            stream.seek(0)
            self.assertEqual(hashlib.sha256(stream.read()).hexdigest(), digest_before)
            for offset, expected in zip(offsets, blocks_before):
                stream.seek(offset)
                self.assertEqual(stream.read(4096), expected)
            self.assertEqual(tuple(after[k] for k in ("file_id", "logical_bytes", "mtime_ns")),
                             tuple(before[k] for k in ("file_id", "logical_bytes", "mtime_ns")))
            self.assertTrue(after["was_compressed"])
            self.assertLess(after["allocated_before_bytes"], before["allocated_before_bytes"])


class IntegrityTests(SyntheticTests):
    def prepare_failure(self):
        self.require_task2()
        path, manifest = self.freeze()
        first, second = [self.f.root / entry["relative_path"] for entry in manifest["eligible"][:2]]
        return path, first, second, self.fingerprint(second)

    # Break caught: trusting matching size/mtime instead of rehashing the complete frozen content.
    def test_changed_file_and_corrupt_compressor_stop_before_next_file(self):
        path, first, second, second_before = self.prepare_failure()
        stamp = first.stat().st_mtime_ns
        first.write_bytes(b"!" + first.read_bytes()[1:])
        os.utime(first, ns=(stamp, stamp))
        first_before = self.fingerprint(first)
        result = tool.apply_manifest(path)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["error_code"], "SOURCE_CHANGED")
        self.assertEqual(result["files_changed"], 0)
        self.assertEqual(self.fingerprint(first), first_before)
        self.assertEqual(self.fingerprint(second), second_before)

    # Break caught: a short-file reader's cached bytes hide real writes on the same OS handle.
    def test_corrupt_short_file_on_same_os_handle_is_hash_mismatch(self):
        path, first, second, second_before = self.prepare_failure()
        native = tool.set_compression

        def corrupt(stream, enabled):
            native(stream, enabled)
            os.lseek(stream.fileno(), 0, os.SEEK_SET)
            os.write(stream.fileno(), b"!")

        with mock.patch.object(tool, "set_compression", side_effect=corrupt):
            result = tool.apply_manifest(path)
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["error_code"], "HASH_MISMATCH")
        self.assertEqual(result["files_changed"], 1)
        self.assertEqual(first.read_bytes()[:1], b"!")
        self.assertEqual(self.fingerprint(second), second_before)

    # Break caught: reopening a replacement with equal bytes and timestamps as the original file.
    def test_replaced_file_identity_stops_before_compression(self):
        path, first, second, second_before = self.prepare_failure()
        data, stamp = first.read_bytes(), first.stat().st_mtime_ns
        first.rename(first.with_name("original-preserved.md"))
        first.write_bytes(data)
        os.utime(first, ns=(stamp, stamp))
        before = self.fingerprint(first)
        result = tool.apply_manifest(path)
        self.assertEqual((result["status"], result["error_code"]), ("failed", "SOURCE_CHANGED"))
        self.assertEqual(self.fingerprint(first), before)
        self.assertEqual(self.fingerprint(second), second_before)

    # Break caught: compressing a newly shared hardlink or ignoring non-compression attributes.
    def test_new_hardlink_and_attribute_change_are_rejected(self):
        path, first, second, second_before = self.prepare_failure()
        os.link(first, self.f.root / "new-shared-link.md")
        result = tool.apply_manifest(path)
        self.assertEqual(result["error_code"], "SOURCE_CHANGED")
        self.assertEqual(result["files_changed"], 0)
        self.assertEqual(self.fingerprint(second), second_before)
        self.assertFalse(first.stat().st_file_attributes & 0x800)

    # Break caught: a native error is ignored and later entries are still modified.
    def test_native_failure_stops_before_next_file(self):
        path, first, second, second_before = self.prepare_failure()
        before = self.fingerprint(first)
        with mock.patch.object(tool, "set_compression", side_effect=OSError("injected native failure")):
            result = tool.apply_manifest(path)
        self.assertEqual((result["status"], result["error_code"]), ("failed", "IO_FAILED"))
        self.assertEqual(result["files_changed"], 0)
        self.assertEqual(self.fingerprint(first), before)
        self.assertEqual(self.fingerprint(second), second_before)

    # Break caught: falling back to permissive sharing when a real writer already owns the source.
    def test_writer_lock_is_not_bypassed(self):
        path, first, second, second_before = self.prepare_failure()
        before = self.fingerprint(first)
        with first.open("r+b", buffering=0):
            result = tool.apply_manifest(path)
        self.assertEqual((result["status"], result["error_code"]), ("failed", "SOURCE_LOCKED"))
        self.assertEqual(self.fingerprint(first), before)
        self.assertEqual(self.fingerprint(second), second_before)

    # Break caught: the handle allows a second writer or deletion while compression is in progress.
    def test_locked_handle_blocks_writers_and_directory_rename(self):
        path, first, second, second_before = self.prepare_failure()
        native = tool.set_compression
        attempts = []

        def protected(stream, enabled):
            if Path(stream.name) == first:
                for operation in (lambda: first.open("r+b"),
                                  lambda: first.rename(first.with_name("moved.md")),
                                  lambda: first.parent.rename(first.parent.with_name("moved-parent"))):
                    try:
                        handle = operation()
                    except OSError as exc:
                        attempts.append(exc)
                    else:
                        if hasattr(handle, "close"):
                            handle.close()
                        self.fail("a protected source could be written or renamed")
            native(stream, enabled)

        with mock.patch.object(tool, "set_compression", side_effect=protected):
            result = tool.apply_manifest(path)
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual(len(attempts), 3)
        self.assertTrue(all(exc.winerror in (5, 32) or (isinstance(exc, PermissionError) and exc.errno == 13)
                            for exc in attempts), attempts)

    # Break caught: changing a non-compression attribute is accepted during verify/resume.
    def test_noncompression_attribute_change_is_rejected(self):
        path, first, second, second_before = self.prepare_failure()
        kernel = tool._kernel
        kernel.SetFileAttributesW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD]
        kernel.SetFileAttributesW.restype = wintypes.BOOL
        attrs = first.stat().st_file_attributes
        self.assertTrue(kernel.SetFileAttributesW(str(first), attrs | 0x2))
        result = tool.apply_manifest(path)
        self.assertEqual(result["error_code"], "SOURCE_CHANGED")
        self.assertEqual(result["files_changed"], 0)
        self.assertEqual(self.fingerprint(second), second_before)

    # Break caught: apply leaves the mtime changed by a native attribute operation.
    def test_mtime_is_restored_on_the_same_handle(self):
        path, first, second, second_before = self.prepare_failure()
        native = tool.set_compression
        original = first.stat().st_mtime_ns
        kernel = tool._kernel
        kernel.SetFileTime.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.FILETIME),
                                      ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME)]
        kernel.SetFileTime.restype = wintypes.BOOL

        def change_time(stream, enabled):
            import msvcrt
            native(stream, enabled)
            ticks = (original + 2_000_000_000) // 100 + 116_444_736_000_000_000
            changed = wintypes.FILETIME(ticks & 0xFFFFFFFF, ticks >> 32)
            self.assertTrue(kernel.SetFileTime(msvcrt.get_osfhandle(stream.fileno()), None, None,
                                               ctypes.byref(changed)))

        with mock.patch.object(tool, "set_compression", side_effect=change_time):
            result = tool.apply_manifest(path)
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual(first.stat().st_mtime_ns, original)


class ApplyVerifyTests(SyntheticTests):
    # Break caught: apply misses eligible files, touches skipped/external files, or miscounts savings.
    def test_apply_and_verify_all_candidates_with_truthful_totals(self):
        self.require_task2()
        for relative in self.f.expected_paths:
            (self.f.root / relative).write_bytes(b"native compression fixture\n" * 81_000)
        precompressed = self.f.root / ALLOWED[0] / "synthetic/units.idx"
        compress_fixture_file(precompressed)
        skipped_before = self.fingerprint(precompressed)
        outside_before = self.fingerprint(self.f.outside)
        path, manifest = self.freeze()
        result = tool.apply_manifest(path)
        self.assertEqual((result["status"], result["error_code"]), ("completed", None), result)
        self.assertEqual((result["files_verified"], result["files_changed"], result["files_already"]), (5, 5, 0))
        self.assertEqual(result["logical_bytes"], manifest["totals"]["logical_bytes"])
        self.assertEqual(result["allocated_before_bytes"], manifest["totals"]["allocated_before_bytes"])
        self.assertEqual(result["reclaimed_bytes"], result["allocated_before_bytes"] - result["allocated_after_bytes"])
        self.assertGreater(result["reclaimed_bytes"], 0)
        before_verify = self.f.source_fingerprints()
        verified = tool.verify_manifest(path)
        self.assertEqual(verified["status"], "completed", verified)
        self.assertEqual((verified["files_verified"], verified["files_compressed"],
                          verified["files_uncompressed"], verified["files_changed"]), (5, 5, 0, 0))
        self.assertEqual(self.f.source_fingerprints(), before_verify)
        self.assertEqual(self.fingerprint(precompressed), skipped_before)
        self.assertEqual(self.fingerprint(self.f.outside), outside_before)
        self.assertEqual(json.loads(path.read_text(encoding="utf-8")), manifest)

    # Break caught: verify treats an original uncompressed candidate as a failure or sets attributes.
    def test_verify_reports_mixed_current_states_without_writing_sources(self):
        self.require_task2()
        path, manifest = self.freeze()
        compress_fixture_file(self.f.root / manifest["eligible"][0]["relative_path"])
        before = self.f.source_fingerprints()
        result = tool.verify_manifest(path)
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual((result["files_verified"], result["files_compressed"],
                          result["files_uncompressed"], result["files_changed"]), (6, 1, 5, 0))
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: verify checks just samples and misses a changed byte outside all three blocks.
    def test_verify_hashes_complete_file_beyond_samples(self):
        self.require_task2()
        first = self.f.root / sorted(self.f.expected_paths)[0]
        first.write_bytes(b"A" * (2_097_152 + 123))
        path, manifest = self.freeze()
        stamp = first.stat().st_mtime_ns
        with first.open("r+b") as stream:
            stream.seek(1_048_577)
            stream.write(b"!")
        os.utime(first, ns=(stamp, stamp))
        before = self.f.source_fingerprints()
        result = tool.verify_manifest(path)
        self.assertEqual((result["status"], result["error_code"]), ("failed", "SOURCE_CHANGED"))
        self.assertEqual(self.f.source_fingerprints(), before)


class ResumeTests(SyntheticTests):
    # Break caught: native success without a receipt is rewritten, or old JSONL is trusted as state.
    def test_resume_after_native_success_before_receipt(self):
        self.require_task2()
        path, manifest = self.freeze()
        first = self.f.root / manifest["eligible"][0]["relative_path"]
        old = path.parent / "runs/old-incomplete/events.jsonl"
        old.parent.mkdir(parents=True)
        old.write_bytes(b'{"untrusted": "all completed"}\n{truncated')
        native = tool.set_compression

        def interrupt_after_native(stream, enabled):
            native(stream, enabled)
            raise KeyboardInterrupt("native success before receipt")

        with mock.patch.object(tool, "set_compression", side_effect=interrupt_after_native), self.assertRaises(KeyboardInterrupt):
            tool.apply_manifest(path)
        self.assertTrue(first.stat().st_file_attributes & 0x800)
        first_before = self.fingerprint(first)
        native_writes = []

        def record_native_write(stream, enabled):
            native_writes.append(Path(stream.name))
            native(stream, enabled)

        with mock.patch.object(tool, "set_compression", side_effect=record_native_write):
            result = tool.apply_manifest(path)
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual((result["files_verified"], result["files_changed"], result["files_already"]), (6, 5, 1))
        self.assertNotIn(first, native_writes)
        self.assertEqual(self.fingerprint(first), first_before)
        self.assertEqual(old.read_bytes(), b'{"untrusted": "all completed"}\n{truncated')
        run_dirs = list((path.parent / "runs").iterdir())
        self.assertEqual(len(run_dirs), 3)
        self.assertEqual(json.loads((path.parent / "runs" / result["run_id"] / "summary.json").read_text(encoding="utf-8")), result)

    # Break caught: a second completed apply repeats native writes or claims skipped items as changed.
    def test_completed_apply_is_idempotent_with_new_immutable_run_evidence(self):
        self.require_task2()
        path, manifest = self.freeze()
        first = tool.apply_manifest(path)
        self.assertTrue((path.parent / "runs" / first["run_id"]).is_dir(), "new run evidence is missing")
        frozen_run = {item.name: item.read_bytes() for item in (path.parent / "runs" / first["run_id"]).iterdir()}
        before = self.f.source_fingerprints()
        native = tool.set_compression
        native_writes = []

        def record_native_write(stream, enabled):
            native_writes.append(str(stream.name))
            native(stream, enabled)

        with mock.patch.object(tool, "set_compression", side_effect=record_native_write):
            second = tool.apply_manifest(path)
        self.assertEqual(second["status"], "completed", second)
        self.assertNotEqual(first["run_id"], second["run_id"])
        self.assertEqual((second["files_verified"], second["files_changed"], second["files_already"]), (6, 0, 6))
        self.assertEqual(second["reclaimed_bytes"], 0)
        self.assertEqual(native_writes, [])
        self.assertEqual(self.f.source_fingerprints(), before)
        self.assertEqual({item.name: item.read_bytes() for item in (path.parent / "runs" / first["run_id"]).iterdir()}, frozen_run)
        for result in (first, second):
            run_dir = path.parent / "runs" / result["run_id"]
            self.assertEqual({item.name for item in run_dir.iterdir()}, {"events.jsonl", "task-windows.jsonl", "summary.json"})
            self.assertEqual(json.loads((run_dir / "summary.json").read_text(encoding="utf-8")), result)
            self.assertTrue((run_dir / "task-windows.jsonl").read_text(encoding="utf-8").strip())
            events = [json.loads(line) for line in (run_dir / "events.jsonl").read_text(encoding="utf-8").splitlines()]
            self.assertEqual(sum(event["event"] == "file_verified" for event in events), 6)
            self.assertEqual(sum(event["event"] == "trial_completed" for event in events), 1)


class GlobalGuardTests(SyntheticTests):
    def global_name(self):
        identity = hashlib.sha256(os.path.normcase(str(self.f.root)).encode("utf-8")).hexdigest()
        return "Global\\HistoricalArtifactCompression-" + identity

    def child_guard(self, action):
        child = Path(self.f.temp.name) / "guard-child.py"
        child.write_text('''import ctypes
from ctypes import wintypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location("guard_tool", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
root = Path(sys.argv[2])
if sys.argv[3] == "probe-global":
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenMutexW.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR]
    kernel.OpenMutexW.restype = wintypes.HANDLE
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    identity = hashlib.sha256(os.path.normcase(str(root)).encode("utf-8")).hexdigest()
    handle = kernel.OpenMutexW(0x00100000, False, "Global\\\\HistoricalArtifactCompression-" + identity)
    result = {"status": "global-opened" if handle else "missing", "winerror": ctypes.get_last_error()}
    if handle:
        kernel.CloseHandle(handle)
else:
    try:
        with module._compression_run(root):
            result = {"status": "entered"}
    except module.ToolError as exc:
        result = {"status": "blocked", "error_code": exc.code}
    except OSError as exc:
        result = {"status": "failed", "winerror": exc.winerror}
result["pid"] = os.getpid()
print(json.dumps(result))
''', encoding="utf-8")
        with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            result = subprocess.run(
                [sys.executable, "-B", str(child), str(SCRIPT), str(self.f.root).swapcase(), action],
                stdout=stdout, stderr=stderr, check=False, timeout=30,
            )
            stdout.seek(0)
            stderr.seek(0)
            error = stderr.read()
            self.assertEqual(result.returncode, 0, error.decode("utf-8", errors="replace"))
            self.assertEqual(error, b"")
            observation = json.loads(stdout.read().decode("utf-8"))
        self.assertNotEqual(observation["pid"], os.getpid(), "the contender must be an independent process")
        return observation

    # Break caught: a Local object cannot be opened through the cross-session Global namespace.
    def test_guard_is_visible_in_global_namespace_from_independent_process(self):
        before = self.f.source_fingerprints()
        with tool._compression_run(self.f.root):
            observed = self.child_guard("probe-global")
        self.assertEqual(observed["status"], "global-opened", observed)
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: an independent process enters the same normalized root, or release leaks the guard.
    def test_independent_process_is_blocked_and_reenters_after_release(self):
        before = self.f.source_fingerprints()
        with tool._compression_run(self.f.root):
            blocked = self.child_guard("guard")
        self.assertEqual((blocked["status"], blocked.get("error_code")), ("blocked", "SOURCE_LOCKED"), blocked)
        entered = self.child_guard("guard")
        self.assertEqual(entered["status"], "entered", entered)
        with tool._compression_run(self.f.root):
            self.assertEqual(self.child_guard("guard")["status"], "blocked")
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: native object creation fails but apply falls back to a session-local/unlocked run.
    def test_global_creation_failure_refuses_apply_without_source_writes(self):
        path, manifest = self.freeze()
        before = self.f.source_fingerprints()
        kernel = tool._kernel
        kernel.CreateEventW.argtypes = [wintypes.LPVOID, wintypes.BOOL, wintypes.BOOL, wintypes.LPCWSTR]
        kernel.CreateEventW.restype = wintypes.HANDLE
        handle = kernel.CreateEventW(None, True, False, self.global_name())
        self.assertTrue(handle, ctypes.get_last_error())
        try:
            result = tool.apply_manifest(path)
        finally:
            kernel.CloseHandle(handle)
        self.assertEqual((result["status"], result["error_code"], result["files_changed"]),
                         ("failed", "IO_FAILED", 0), result)
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: access denied to the shared Global object silently permits an unguarded apply.
    def test_global_access_denied_refuses_apply_without_source_writes(self):
        path, manifest = self.freeze()
        before = self.f.source_fingerprints()
        kernel = tool._kernel
        advapi = ctypes.WinDLL("advapi32", use_last_error=True)
        convert = advapi.ConvertStringSecurityDescriptorToSecurityDescriptorW
        convert.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, ctypes.POINTER(wintypes.LPVOID),
                            ctypes.POINTER(wintypes.DWORD)]
        convert.restype = wintypes.BOOL
        kernel.LocalFree.argtypes = [wintypes.LPVOID]
        kernel.LocalFree.restype = wintypes.LPVOID

        class SecurityAttributes(ctypes.Structure):
            _fields_ = [("length", wintypes.DWORD), ("descriptor", wintypes.LPVOID), ("inherit", wintypes.BOOL)]

        descriptor = wintypes.LPVOID()
        self.assertTrue(convert("D:(D;;GA;;;WD)", 1, ctypes.byref(descriptor), None), ctypes.get_last_error())
        try:
            attributes = SecurityAttributes(ctypes.sizeof(SecurityAttributes), descriptor, False)
            handle = kernel.CreateMutexW(ctypes.byref(attributes), False, self.global_name())
            self.assertTrue(handle, ctypes.get_last_error())
            try:
                denied = kernel.CreateMutexW(None, False, self.global_name())
                error = ctypes.get_last_error()
                if denied:
                    kernel.CloseHandle(denied)
                self.assertFalse(denied, "the test mutex must really deny access")
                self.assertEqual(error, 5, "the native fixture must produce ERROR_ACCESS_DENIED")
                result = tool.apply_manifest(path)
            finally:
                kernel.CloseHandle(handle)
        finally:
            kernel.LocalFree(descriptor)
        self.assertEqual((result["status"], result["error_code"], result["files_changed"]),
                         ("failed", "IO_FAILED", 0), result)
        self.assertEqual(self.f.source_fingerprints(), before)


class ConcurrencyTests(SyntheticTests):
    # Break caught: two apply runs interleave files under the same project root.
    def test_second_apply_is_rejected_until_first_guard_is_released(self):
        self.require_task2()
        path, manifest = self.freeze()
        before = self.f.source_fingerprints()
        nested = []

        def window(now):
            with mock.patch.object(tool, "read_task_window", side_effect=lambda value: (False, value + timedelta(hours=1))):
                result = tool.apply_manifest(path)
            nested.append((result, self.f.source_fingerprints()))
            return False, now + timedelta(hours=1)

        with mock.patch.object(tool, "read_task_window", side_effect=window):
            first = tool.apply_manifest(path)
        self.assertEqual(len(nested), 1)
        blocked, after_blocked = nested[0]
        self.assertEqual((blocked["status"], blocked["error_code"], blocked["files_changed"]),
                         ("failed", "SOURCE_LOCKED", 0))
        self.assertEqual(after_blocked, before)
        self.assertEqual((first["status"], first["files_changed"]), ("completed", 6))
        resumed = tool.apply_manifest(path)
        self.assertEqual((resumed["status"], resumed["files_changed"], resumed["files_already"]), ("completed", 0, 6))


class RunEvidenceRaceTests(SyntheticTests):
    def assert_run_directory_is_stable(self, target):
        self.require_task2()
        path, manifest = self.freeze()
        outside = Path(self.f.temp.name) / "outside-run-evidence"
        outside.mkdir()
        sentinel = outside / "sentinel.txt"
        sentinel.write_bytes(b"preserved external evidence")
        blocked = []

        def window(now):
            if not blocked:
                nodes = {"root": self.f.root, "evidence": path.parent}
                if target == "run":
                    candidates = list((path.parent / "runs").iterdir()) if (path.parent / "runs").exists() else []
                    self.assertEqual(len(candidates), 1, "the run leaf must be created and locked before querying")
                    nodes["run"] = candidates[0]
                node = nodes[target]
                try:
                    node.rename(node.with_name(node.name + "-displaced"))
                except OSError as exc:
                    blocked.append(exc.winerror)
                else:
                    self.f.junction(node, outside)
            return False, now + timedelta(hours=1)

        with mock.patch.object(tool, "read_task_window", side_effect=window):
            result = tool.apply_manifest(path)
        self.assertEqual(blocked, [32], "ancestor and leaf directory handles must deny the actual rename")
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual({item.name: item.read_bytes() for item in outside.iterdir()}, {"sentinel.txt": b"preserved external evidence"})
        run_dir = path.parent / "runs" / result["run_id"]
        self.assertEqual(json.loads((run_dir / "summary.json").read_text(encoding="utf-8")), result)

    # Break caught: runs evidence is written after a SourceRoot ancestor has been exchanged.
    def test_run_root_ancestor_is_locked(self):
        self.assert_run_directory_is_stable("root")

    # Break caught: only runs leaf is locked, while the plan/evidence directory can be exchanged.
    def test_run_evidence_ancestor_is_locked(self):
        self.assert_run_directory_is_stable("evidence")

    # Break caught: _write_new is relied upon alone without pinning the run directory itself.
    def test_run_leaf_is_locked(self):
        self.assert_run_directory_is_stable("run")


class ApplyCliTests(SyntheticTests):
    def run_command(self, command, path):
        try:
            return tool.main([command, "--manifest", str(path)])
        except SystemExit as exc:
            self.fail(f"{command} did not return its JSON summary: {exc}")

    # Break caught: apply/verify are absent, or CLI totals differ from the new APIs.
    def test_apply_and_verify_commands_return_completed(self):
        self.require_task2()
        path, manifest = self.freeze()
        for command in ("apply", "verify"):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                exitcode = self.run_command(command, path)
            result = json.loads(output.getvalue())
            self.assertEqual((exitcode, result["status"], result["files_verified"]), (0, "completed", 6))

    # Break caught: paused apply returns success or makes a source change before checking tasks.
    def test_paused_apply_exitcode_is_one(self):
        self.require_task2()
        path, manifest = self.freeze()
        before = self.f.source_fingerprints()
        output = io.StringIO()
        with mock.patch.object(tool, "read_task_window", side_effect=lambda now: (True, now + timedelta(hours=1))), \
             contextlib.redirect_stdout(output):
            exitcode = self.run_command("apply", path)
        result = json.loads(output.getvalue())
        self.assertEqual((exitcode, result["status"], result["files_changed"]), (1, "paused", 0))
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: changed sources or frozen bytes return a paused/success exit code instead of failure.
    def test_failed_apply_exitcode_is_two(self):
        self.require_task2()
        path, manifest = self.freeze()
        first = self.f.root / manifest["eligible"][0]["relative_path"]
        first.write_bytes(b"changed fixture")
        before = self.f.source_fingerprints()
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            exitcode = self.run_command("apply", path)
        result = json.loads(output.getvalue())
        self.assertEqual((exitcode, result["status"], result["error_code"]), (2, "failed", "SOURCE_CHANGED"))
        self.assertEqual(self.f.source_fingerprints(), before)


def volume_allocation_unit(root):
    """Read the fixture volume independently of the maintenance implementation."""
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.GetDiskFreeSpaceW.argtypes = [wintypes.LPCWSTR] + [ctypes.POINTER(wintypes.DWORD)] * 4
    kernel.GetDiskFreeSpaceW.restype = wintypes.BOOL
    sectors, sector_bytes, free, total = (wintypes.DWORD() for _ in range(4))
    if not kernel.GetDiskFreeSpaceW(root.anchor, ctypes.byref(sectors), ctypes.byref(sector_bytes),
                                    ctypes.byref(free), ctypes.byref(total)):
        raise ctypes.WinError(ctypes.get_last_error())
    return sectors.value * sector_bytes.value


class RollbackTests(SyntheticTests):
    def require_rollback(self):
        self.assertTrue(callable(getattr(tool, "rollback_manifest", None)), "rollback_manifest is missing")

    def compressed_plan(self):
        self.require_rollback()
        path, manifest = self.freeze()
        applied = tool.apply_manifest(path)
        self.assertEqual(applied["status"], "completed", applied)
        return path, manifest

    # Break caught: rollback uses directory scanning/logs rather than only original eligible entries.
    def test_only_manifest_candidates_are_restored_and_repeat_is_noop(self):
        self.require_rollback()
        folder = self.f.root / ALLOWED[0] / "中文 与 空格"
        folder.mkdir()
        (folder / "body.md").write_bytes(b"transparent rollback\n" * 100_003)
        (folder / "units.jsonl").write_bytes(os.urandom(131_079))
        (folder / "units.idx").write_bytes(b"")
        precompressed = self.f.root / ALLOWED[0] / "synthetic/units.idx"
        compress_fixture_file(precompressed)
        linked = self.f.root / ALLOWED[1] / "synthetic/units.idx"
        os.link(linked, self.f.root / "shared-outside.idx")
        original = self.f.source_fingerprints()
        path, manifest = self.freeze()
        self.assertEqual({entry["reason"] for entry in manifest["skipped"]}, {"already_compressed", "hardlink"})
        hashes = {entry["relative_path"]: hashlib.sha256(original[entry["relative_path"]][0]).hexdigest()
                  for entry in manifest["eligible"]}
        self.assertEqual(tool.apply_manifest(path)["status"], "completed")
        old = path.parent / "runs/untrusted/events.jsonl"
        old.parent.mkdir()
        old.write_bytes(b'{"all_restored": true}\n{truncated')
        first = tool.rollback_manifest(path)
        self.assertEqual((first["status"], first["files_changed"], first["files_verified"]), ("completed", 7, 7), first)
        self.assertEqual((first["files_compressed"], first["files_uncompressed"]), (0, 7))
        self.assertEqual(self.f.source_fingerprints(), original)
        self.assertEqual({entry["relative_path"]: self.fingerprint(self.f.root / entry["relative_path"])[0]
                          for entry in manifest["eligible"]}, hashes)
        self.assertLess(first["reclaimed_bytes"], 0)
        run = path.parent / "runs" / first["run_id"]
        saved = {item.name: item.read_bytes() for item in run.iterdir()}
        # No decompression means even zero free space is harmless on an idempotent repeat.
        with mock.patch.object(shutil, "disk_usage", return_value=shutil.disk_usage(self.f.root)._replace(free=0)):
            second = tool.rollback_manifest(path)
        self.assertEqual((second["status"], second["files_changed"], second["files_already"]), ("completed", 0, 7), second)
        self.assertEqual(second["reclaimed_bytes"], 0)
        self.assertEqual(self.f.source_fingerprints(), original)
        self.assertEqual({item.name: item.read_bytes() for item in run.iterdir()}, saved)
        self.assertEqual(old.read_bytes(), b'{"all_restored": true}\n{truncated')
        self.assertNotEqual(first["run_id"], second["run_id"])
        self.assertEqual(json.loads((path.parent / "runs" / second["run_id"] / "summary.json").read_text(encoding="utf-8")), second)
        self.assertEqual(tool.verify_manifest(path)["files_uncompressed"], 7)

    # Break caught: a tampered manifest can authorize decompression before its sidecar is checked.
    def test_modified_manifest_prevents_writes(self):
        path, manifest = self.compressed_plan()
        path.write_bytes(path.read_bytes() + b" ")
        before = self.f.source_fingerprints()
        result = tool.rollback_manifest(path)
        self.assertEqual((result["status"], result["error_code"], result["files_changed"]), ("failed", "MANIFEST_CHANGED", 0))
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: later changed bytes are discovered only after earlier candidates were decompressed.
    def test_changed_later_file_prevents_all_writes(self):
        self.require_rollback()
        target = self.f.root / sorted(self.f.expected_paths)[-1]
        target.write_bytes(b"A" * (2_097_152 + 123))
        path, manifest = self.compressed_plan()
        stamp = target.stat().st_mtime_ns
        with target.open("r+b", buffering=0) as stream:
            stream.seek(1_048_577)
            stream.write(b"!")
        os.utime(target, ns=(stamp, stamp))
        before = self.f.source_fingerprints()
        result = tool.rollback_manifest(path)
        self.assertEqual((result["status"], result["error_code"], result["files_changed"]), ("failed", "SOURCE_CHANGED", 0), result)
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: matching bytes/mtime are enough to accept a replaced or newly shared file.
    def test_replacement_and_new_hardlink_prevent_writes(self):
        path, manifest = self.compressed_plan()
        target = self.f.root / manifest["eligible"][-1]["relative_path"]
        data, stamp = target.read_bytes(), target.stat().st_mtime_ns
        preserved = target.with_name("preserved-original.idx")
        target.rename(preserved)
        target.write_bytes(data)
        os.utime(target, ns=(stamp, stamp))
        before = self.f.source_fingerprints()
        result = tool.rollback_manifest(path)
        self.assertEqual((result["error_code"], result["files_changed"]), ("SOURCE_CHANGED", 0))
        self.assertEqual(self.f.source_fingerprints(), before)
        target.unlink()
        preserved.rename(target)
        os.link(target, self.f.root / "new-shared-link.idx")
        before = self.f.source_fingerprints()
        result = tool.rollback_manifest(path)
        self.assertEqual((result["error_code"], result["files_changed"]), ("SOURCE_CHANGED", 0))
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: logical-current allocation underestimates rounded NTFS growth or misses 1GiB reserve.
    def test_low_space_uses_total_cluster_rounded_growth_before_any_decompression(self):
        self.require_rollback()
        unit = volume_allocation_unit(self.f.root)
        for relative in self.f.expected_paths:
            (self.f.root / relative).write_bytes(b"A" * (unit * 9 + 1))
        path, manifest = self.compressed_plan()
        growth = 0
        for entry in manifest["eligible"]:
            with (self.f.root / entry["relative_path"]).open("rb", buffering=0) as stream:
                current = tool._file_metadata(stream)["allocated_before_bytes"]
            uncompressed = ((entry["logical_bytes"] + unit - 1) // unit) * unit
            growth += max(0, uncompressed - current)
        self.assertGreater(growth, 0)
        usage = shutil.disk_usage(self.f.root)
        before = self.f.source_fingerprints()
        with mock.patch.object(shutil, "disk_usage", return_value=usage._replace(free=(1 << 30) + growth - 1)):
            low = tool.rollback_manifest(path)
        self.assertEqual((low["status"], low["pause_reason"], low["files_changed"]), ("paused", "INSUFFICIENT_SPACE", 0), low)
        self.assertEqual(self.f.source_fingerprints(), before)
        with mock.patch.object(shutil, "disk_usage", return_value=usage._replace(free=(1 << 30) + growth)):
            enough = tool.rollback_manifest(path)
        self.assertEqual((enough["status"], enough["files_changed"]), ("completed", 6), enough)
        self.assertTrue(all(not (self.f.root / entry["relative_path"]).stat().st_file_attributes & 0x800
                            for entry in manifest["eligible"]))

    # Break caught: the initial space check is trusted after another writer consumes available capacity.
    def test_free_space_is_rechecked_immediately_before_native_decompression(self):
        path, manifest = self.compressed_plan()
        usage = shutil.disk_usage(self.f.root)
        native = tool.set_compression
        free = [usage.free]

        def decompress(stream, enabled):
            native(stream, enabled)
            free[0] = 0

        with mock.patch.object(shutil, "disk_usage", side_effect=lambda root: usage._replace(free=free[0])), \
             mock.patch.object(tool, "set_compression", side_effect=decompress):
            result = tool.rollback_manifest(path)
        self.assertEqual((result["status"], result["pause_reason"], result["files_changed"]), ("paused", "INSUFFICIENT_SPACE", 1), result)
        states = [bool((self.f.root / entry["relative_path"]).stat().st_file_attributes & 0x800)
                  for entry in manifest["eligible"]]
        self.assertEqual(states, [False] + [True] * 5)

    # Break caught: rollback has a distinct lock name/lifetime and can interleave with apply.
    def test_rollback_uses_the_same_global_run_guard(self):
        path, manifest = self.compressed_plan()
        before = self.f.source_fingerprints()
        with tool._compression_run(self.f.root):
            result = tool.rollback_manifest(path)
        self.assertEqual((result["status"], result["error_code"], result["files_changed"]), ("failed", "SOURCE_LOCKED", 0), result)
        self.assertEqual(self.f.source_fingerprints(), before)
        self.assertEqual(tool.rollback_manifest(path)["status"], "completed")

    # Break caught: decompression reopens a source or drops the frozen/evidence/source directory handles.
    def test_same_handle_and_all_directory_lifetimes_protect_rollback(self):
        path, manifest = self.compressed_plan()
        first = self.f.root / manifest["eligible"][0]["relative_path"]
        native = tool.set_compression
        blocked = []

        def deny_changes(operations):
            for operation in operations:
                try:
                    opened = operation()
                except OSError as exc:
                    blocked.append(exc)
                else:
                    if hasattr(opened, "close"):
                        opened.close()
                    self.fail("a protected rollback resource could be written or renamed")

        checked = []
        def window(now):
            if not checked:
                checked.append(True)
                runs = list((path.parent / "runs").iterdir())
                current_run = next(item for item in runs if not (item / "summary.json").exists())
                deny_changes((lambda: self.f.root.rename(self.f.root.with_name("displaced-root")),
                              lambda: path.parent.rename(path.parent.with_name("displaced-plan")),
                              lambda: current_run.rename(current_run.with_name("displaced-run")),
                              lambda: path.open("r+b"), lambda: path.with_suffix(".sha256").open("r+b")))
            return False, now + timedelta(hours=1)

        def protected(stream, enabled):
            if Path(stream.name) == first:
                deny_changes((lambda: first.open("r+b"),
                              lambda: first.rename(first.with_name("displaced-source.md")),
                              lambda: first.parent.rename(first.parent.with_name("displaced-parent"))))
            native(stream, enabled)
            # Force a real HANDLE timestamp side effect; rollback must restore the frozen value.
            tool._restore_mtime(stream, first.stat().st_mtime_ns + 2_000_000_000)

        with mock.patch.object(tool, "read_task_window", side_effect=window), \
             mock.patch.object(tool, "set_compression", side_effect=protected):
            result = tool.rollback_manifest(path)
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual(len(blocked), 8)
        self.assertTrue(all(exc.winerror in (5, 32) or (isinstance(exc, PermissionError) and exc.errno == 13)
                            for exc in blocked), blocked)
        self.assertEqual(first.stat().st_mtime_ns, manifest["eligible"][0]["mtime_ns"])

    # Break caught: cached short-file bytes hide corruption made on the exact decompression HANDLE.
    def test_post_decompression_corruption_fails_without_touching_next_file(self):
        path, manifest = self.compressed_plan()
        first, second = [self.f.root / entry["relative_path"] for entry in manifest["eligible"][:2]]
        before = self.fingerprint(second)
        native = tool.set_compression

        def corrupt(stream, enabled):
            native(stream, enabled)
            os.lseek(stream.fileno(), 0, os.SEEK_SET)
            os.write(stream.fileno(), b"!")

        with mock.patch.object(tool, "set_compression", side_effect=corrupt):
            result = tool.rollback_manifest(path)
        self.assertEqual((result["status"], result["error_code"], result["files_changed"]), ("failed", "HASH_MISMATCH", 1), result)
        self.assertEqual(first.read_bytes()[:1], b"!")
        self.assertFalse(first.stat().st_file_attributes & 0x800)
        self.assertEqual(self.fingerprint(second), before)

    # Break caught: final content verification runs before metadata restoration and misses a later fault.
    def test_content_is_reverified_after_restoring_metadata(self):
        path, manifest = self.compressed_plan()
        second = self.f.root / manifest["eligible"][1]["relative_path"]
        before = self.fingerprint(second)
        native, restore = tool.set_compression, tool._restore_mtime

        def change_time(stream, enabled):
            native(stream, enabled)
            restore(stream, manifest["eligible"][0]["mtime_ns"] + 2_000_000_000)

        def restore_with_fault(stream, mtime_ns):
            os.lseek(stream.fileno(), 0, os.SEEK_SET)
            os.write(stream.fileno(), b"!")
            restore(stream, mtime_ns)

        with mock.patch.object(tool, "set_compression", side_effect=change_time), \
             mock.patch.object(tool, "_restore_mtime", side_effect=restore_with_fault):
            result = tool.rollback_manifest(path)
        self.assertEqual((result["status"], result["error_code"], result["files_changed"]), ("failed", "HASH_MISMATCH", 1), result)
        self.assertEqual(self.fingerprint(second), before)

    # Break caught: rollback trusts incomplete old receipts or rewrites an already restored source.
    def test_interrupted_rollback_resumes_from_current_verified_state(self):
        path, manifest = self.compressed_plan()
        old = path.parent / "runs/untrusted/events.jsonl"
        old.parent.mkdir()
        old.write_bytes(b'{"files_changed": 999}\n{truncated')
        native = tool.set_compression

        def interrupt(stream, enabled):
            native(stream, enabled)
            raise KeyboardInterrupt("native decompression before receipt")

        with mock.patch.object(tool, "set_compression", side_effect=interrupt), self.assertRaises(KeyboardInterrupt):
            tool.rollback_manifest(path)
        first = self.f.root / manifest["eligible"][0]["relative_path"]
        before = self.fingerprint(first)
        self.assertFalse(first.stat().st_file_attributes & 0x800)
        result = tool.rollback_manifest(path)
        self.assertEqual((result["status"], result["files_changed"], result["files_already"]), ("completed", 5, 1), result)
        self.assertEqual(self.fingerprint(first), before)
        self.assertEqual(old.read_bytes(), b'{"files_changed": 999}\n{truncated')

    # Break caught: rollback mutates before the same task gates used by apply.
    def test_running_failed_query_and_300_second_boundary_prevent_writes(self):
        path, manifest = self.compressed_plan()
        before = self.f.source_fingerprints()
        cases = ((lambda now: (True, now + timedelta(hours=1)), "TASK_RUNNING"),
                 (lambda now: (False, now + timedelta(seconds=300)), "TASK_IMMINENT"),
                 (mock.Mock(side_effect=OSError("synthetic query failure")), "TASK_QUERY_FAILED"))
        for probe, reason in cases:
            with self.subTest(reason=reason), mock.patch.object(tool, "datetime", BatchClock), \
                 mock.patch.object(tool, "read_task_window", side_effect=probe):
                result = tool.rollback_manifest(path)
            self.assertEqual((result["status"], result["pause_reason"], result["files_changed"]), ("paused", reason, 0), result)
            self.assertEqual(self.f.source_fingerprints(), before)
        with mock.patch.object(tool, "datetime", BatchClock), \
             mock.patch.object(tool, "read_task_window", side_effect=lambda now: (False, now + timedelta(seconds=301))):
            self.assertEqual(tool.rollback_manifest(path)["status"], "completed")

    # Break caught: a long preflight is followed by decompression without refreshing task context.
    def test_task_window_is_rechecked_after_readonly_preflight(self):
        path, manifest = self.compressed_plan()
        before = self.f.source_fingerprints()
        states = iter((False, True))
        with mock.patch.object(tool, "read_task_window", side_effect=lambda now: (next(states), now + timedelta(hours=1))):
            result = tool.rollback_manifest(path)
        self.assertEqual((result["status"], result["pause_reason"], result["files_changed"]), ("paused", "TASK_RUNNING", 0), result)
        self.assertEqual(self.f.source_fingerprints(), before)


class CompleteCliTests(SyntheticTests):
    def command(self, arguments):
        output = io.StringIO()
        try:
            with contextlib.redirect_stdout(output):
                code = tool.main(arguments)
        except SystemExit as exc:
            self.fail(f"CLI did not return a JSON summary: {exc}")
        return code, json.loads(output.getvalue())

    # Break caught: one subcommand is absent or JSON/exit codes do not describe real filesystem state.
    def test_plan_apply_verify_rollback_exit_codes_and_json(self):
        # All process execution is forbidden here: task-window reads have the external boundary fake.
        with mock.patch.object(tool.subprocess, "run", side_effect=AssertionError("model/scheduler mutation forbidden")):
            outputs = [self.command(["plan", "--project-root", str(self.f.root), "--evidence-dir", str(self.f.evidence)])]
            path = Path(outputs[0][1]["manifest"])
            for name in ("apply", "verify", "rollback"):
                outputs.append(self.command([name, "--manifest", str(path)]))
            self.assertEqual([code for code, result in outputs], [0, 0, 0, 0])
            self.assertEqual([result["files_changed"] for code, result in outputs], [0, 6, 0, 6])
            for code, result in outputs:
                self.assertEqual(result["status"], "completed", result)
                self.assertEqual(result["files_verified"], 6)
                self.assertEqual(result["plan_sha256"], hashlib.sha256(path.read_bytes()).hexdigest())
            repeat = self.command(["rollback", "--manifest", str(path)])
            self.assertEqual((repeat[0], repeat[1]["files_changed"], repeat[1]["files_already"]), (0, 0, 6))
            self.assertTrue(all(not (self.f.root / relative).stat().st_file_attributes & 0x800
                                for relative in self.f.expected_paths))

    # Break caught: rollback pause/integrity failure exits successfully or discards the concrete reason.
    def test_rollback_pause_and_failure_have_json_and_nonzero_exitcodes(self):
        path, manifest = self.freeze()
        self.assertEqual(tool.apply_manifest(path)["status"], "completed")
        before = self.f.source_fingerprints()
        with mock.patch.object(tool, "read_task_window", side_effect=lambda now: (True, now + timedelta(hours=1))):
            code, result = self.command(["rollback", "--manifest", str(path)])
        self.assertEqual((code, result["status"], result["pause_reason"]), (1, "paused", "TASK_RUNNING"))
        self.assertEqual(self.f.source_fingerprints(), before)
        path.write_bytes(path.read_bytes() + b" ")
        code, result = self.command(["rollback", "--manifest", str(path)])
        self.assertEqual((code, result["status"], result["error_code"]), (2, "failed", "MANIFEST_CHANGED"))
        self.assertEqual(self.f.source_fingerprints(), before)

    # Break caught: invalid/missing CLI arguments escape as text/SystemExit rather than failed JSON.
    def test_invalid_cli_arguments_return_failed_json(self):
        before = self.f.source_fingerprints()
        for arguments in ([], ["unknown"], ["rollback"], ["rollback", "--ignore-hash"]):
            with self.subTest(arguments=arguments):
                code, result = self.command(arguments)
                self.assertEqual((code, result["status"], result["error_code"]), (2, "failed", "INVALID_ARGUMENT"))
                self.assertEqual(result["files_changed"], 0)
        self.assertEqual(self.f.source_fingerprints(), before)


class ReceiptTests(SyntheticTests):
    def assert_receipt_keeps_source_protected(self, command):
        path, manifest = self.freeze()
        if command == "rollback":
            self.assertEqual(tool.apply_manifest(path)["status"], "completed")
        first = self.f.root / manifest["eligible"][0]["relative_path"]
        append, fsync = tool._append_json, tool.os.fsync
        active = [False]
        denied, allowed = [], []

        def probe(stage):
            try:
                opened = first.open("r+b", buffering=0)
            except OSError as exc:
                denied.append((stage, "writer", exc))
            else:
                opened.close()
                allowed.append((stage, "writer"))
            for label, source in (("source_rename", first), ("parent_rename", first.parent)):
                moved = source.with_name(source.name + "-receipt-probe")
                self.assertTrue(moved.is_absolute() and moved.is_relative_to(self.f.root))
                try:
                    source.rename(moved)
                except OSError as exc:
                    denied.append((stage, label, exc))
                else:
                    allowed.append((stage, label))
                    moved.rename(source)

        def append_receipt(stream, record):
            if record.get("event") == "file_verified" and record["relative_path"] == manifest["eligible"][0]["relative_path"]:
                active[0] = True
                try:
                    probe("append")
                    append(stream, record)
                finally:
                    active[0] = False
            else:
                append(stream, record)

        def durable_receipt(descriptor):
            if active[0]:
                probe("fsync")
            return fsync(descriptor)

        with mock.patch.object(tool, "_append_json", side_effect=append_receipt), \
             mock.patch.object(tool.os, "fsync", side_effect=durable_receipt):
            result = getattr(tool, command + "_manifest")(path)
        self.assertEqual(result["status"], "completed", result)
        self.assertEqual(result["files_verified"], 6)
        self.assertEqual(allowed, [], "source protection ended before the durable file receipt")
        self.assertEqual([(stage, label) for stage, label, exc in denied],
                         [(stage, label) for stage in ("append", "fsync")
                          for label in ("writer", "source_rename", "parent_rename")])
        self.assertTrue(all(exc.winerror in (5, 32) or (isinstance(exc, PermissionError) and exc.errno == 13)
                            for stage, label, exc in denied), denied)
        self.assertEqual(self.fingerprint(first)[0], manifest["eligible"][0]["sha256"])
        # Completion must release both the source HANDLE and target directory handles.
        with first.open("r+b", buffering=0):
            pass
        moved = first.parent.with_name(first.parent.name + "-released")
        self.assertTrue(moved.is_absolute() and moved.is_relative_to(self.f.root))
        first.parent.rename(moved)
        moved.rename(first.parent)

    # Break caught: apply releases the source/parents before receipt append or durable fsync.
    def test_apply_source_is_locked_through_receipt_fsync(self):
        self.assert_receipt_keeps_source_protected("apply")

    # Break caught: read-only verify has the same early-unlock gap at its per-file receipt.
    def test_verify_source_is_locked_through_receipt_fsync(self):
        self.assert_receipt_keeps_source_protected("verify")

    # Break caught: rollback reuses native protection but drops it before its receipt is durable.
    def test_rollback_source_is_locked_through_receipt_fsync(self):
        self.assert_receipt_keeps_source_protected("rollback")

    # Break caught: a receipt durability error is ignored, later files proceed, or handles leak.
    def test_receipt_fsync_failure_stops_next_file_and_releases_resources(self):
        path, manifest = self.freeze()
        first, second = [self.f.root / entry["relative_path"] for entry in manifest["eligible"][:2]]
        for command, changed in (("apply", 1), ("verify", 0), ("rollback", 1)):
            with self.subTest(command=command):
                if command == "rollback":
                    self.assertEqual(tool.apply_manifest(path)["status"], "completed")
                second_before = self.fingerprint(second)
                append, fsync = tool._append_json, tool.os.fsync
                active = [False]

                def append_receipt(stream, record):
                    active[0] = record.get("event") == "file_verified"
                    try:
                        append(stream, record)
                    finally:
                        active[0] = False

                def fail_receipt(descriptor):
                    if active[0]:
                        raise OSError("injected file receipt fsync failure")
                    return fsync(descriptor)

                with mock.patch.object(tool, "_append_json", side_effect=append_receipt), \
                     mock.patch.object(tool.os, "fsync", side_effect=fail_receipt):
                    result = getattr(tool, command + "_manifest")(path)
                self.assertEqual((result["status"], result["error_code"], result["files_changed"], result["files_verified"]),
                                 ("failed", "IO_FAILED", changed, 1), result)
                self.assertIn("receipt fsync failure", result["message"])
                self.assertEqual(self.fingerprint(second), second_before)
                run = path.parent / "runs" / result["run_id"]
                events = [json.loads(line) for line in (run / "events.jsonl").read_text(encoding="utf-8").splitlines()]
                self.assertEqual([row["relative_path"] for row in events if row["event"] == "file_verified"],
                                 [manifest["eligible"][0]["relative_path"]])
                self.assertEqual(json.loads((run / "summary.json").read_text(encoding="utf-8")), result)
                with first.open("r+b", buffering=0), tool._compression_run(self.f.root):
                    pass
                moved = first.parent.with_name(first.parent.name + "-after-failure")
                self.assertTrue(moved.is_absolute() and moved.is_relative_to(self.f.root))
                first.parent.rename(moved)
                moved.rename(first.parent)


class FinalPublicationTests(SyntheticTests):
    # Break caught: the second gate publishes an executable manifest or a retry overwrites its sidecar.
    def test_pause_after_sidecar_keeps_nonexecutable_evidence_and_refuses_overwrite(self):
        manifest = tool.scan_manifest(self.f.root)
        before = self.f.source_fingerprints()
        for index, reason in enumerate(("TASK_RUNNING", "TASK_QUERY_FAILED", "TASK_IMMINENT")):
            with self.subTest(reason=reason):
                evidence = self.f.evidence.with_name("final-gate-" + str(index))
                sidecar = evidence / "manifest.sha256"
                observed_sidecars = []

                def window(now):
                    if not sidecar.is_file():
                        return False, now + timedelta(hours=1)
                    observed_sidecars.append(sidecar.read_bytes())
                    if reason == "TASK_QUERY_FAILED":
                        raise OSError("synthetic final publication query failure")
                    return (True, now + timedelta(hours=1)) if reason == "TASK_RUNNING" else (False, now + timedelta(seconds=300))

                with mock.patch.object(tool, "datetime", BatchClock), \
                     mock.patch.object(tool, "read_task_window", side_effect=window):
                    self.assert_code(reason, lambda: tool.write_manifest(manifest, evidence))
                saved = sidecar.read_bytes()
                self.assertEqual(observed_sidecars, [saved], "the pause must occur after the sidecar exists")
                self.assertRegex(saved, rb"^[0-9a-f]{64}\n$")
                self.assertEqual({item.name for item in evidence.iterdir()}, {"manifest.sha256"})
                missing_manifest = evidence / "manifest.json"
                for operation in (tool.apply_manifest, tool.verify_manifest, tool.rollback_manifest):
                    result = operation(missing_manifest)
                    self.assertEqual((result["status"], result["files_changed"]), ("failed", 0), result)
                self.assert_code("EVIDENCE_EXISTS", lambda: tool.write_manifest(manifest, evidence))
                self.assertEqual(sidecar.read_bytes(), saved)
                self.assertFalse(missing_manifest.exists())
                self.assertEqual(self.f.source_fingerprints(), before)


if __name__ == "__main__":
    unittest.main()
