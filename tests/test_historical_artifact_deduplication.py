"""Isolated NTFS tests; production tasks and historical inputs are never used."""

import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/deduplicate_historical_artifacts.py"
spec = importlib.util.spec_from_file_location("historical_dedup", SCRIPT)
tool = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tool)


class DeduplicationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="dedup-isolated-")
        self.root = Path(self.temp.name)
        self.window = mock.patch.object(tool, "quiet_window")
        self.window.start()
        self.paths = []
        for relative in tool.ALLOWED_ROOTS:
            directory = self.root / relative
            directory.mkdir(parents=True)
            path = directory / "same.txt"
            path.write_bytes(b"immutable reference\n" * 257)
            os.utime(path, ns=(1_700_000_000_000_000_000,) * 2)
            self.paths.append(path)
        self.output = self.root / tool.EVIDENCE_ROOT / "case"

    def tearDown(self):
        self.window.stop()
        for path in self.root.rglob("*"):
            if path.is_file():
                tool.attrs(path, path.stat().st_file_attributes & ~tool.READONLY)
        self.temp.cleanup()

    def plan(self):
        value = tool.plan(self.root, self.output)
        return Path(value["manifest"]), value["sha256"]

    def run_action(self, action, plan=None):
        path, sha = plan or self.plan()
        return tool.operate(path, sha, action)

    def test_apply_verify_idempotent_and_restore_bytes_mtime(self):
        expected = [(p.read_bytes(), p.stat().st_mtime_ns) for p in self.paths]
        plan = self.plan()
        self.assertEqual(self.run_action("apply", plan)["changed"], 3)
        self.assertEqual(len({p.stat().st_ino for p in self.paths}), 1)
        self.assertTrue(all(p.stat().st_file_attributes & 1 for p in self.paths))
        self.run_action("verify", plan)
        self.assertEqual(self.run_action("apply", plan)["changed"], 0)
        self.assertEqual(self.run_action("rollback", plan)["changed"], 3)
        self.assertEqual(len({p.stat().st_ino for p in self.paths}), 4)
        self.assertEqual([(p.read_bytes(), p.stat().st_mtime_ns) for p in self.paths], expected)
        self.assertEqual(self.run_action("rollback", plan)["changed"], 0)

    def test_different_timestamp_not_shared(self):
        os.utime(self.paths[-1], ns=(1_710_000_000_000_000_000,) * 2)
        plan = self.plan()
        self.run_action("apply", plan)
        self.assertNotEqual(self.paths[0].stat().st_ino, self.paths[-1].stat().st_ino)

    def test_different_content_not_shared(self):
        self.paths[-1].write_bytes(b"other")
        plan = self.plan()
        self.run_action("apply", plan)
        self.assertEqual(self.paths[-1].read_bytes(), b"other")

    def test_existing_hardlinks_excluded(self):
        os.link(self.paths[0], self.root / "independent-backup.txt")
        path, _ = self.plan()
        manifest = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(manifest["skipped"][0]["reason"], "existing_hardlink")

    def test_additional_stream_excluded(self):
        Path(str(self.paths[0]) + ":evidence").write_bytes(b"private stream")
        path, _ = self.plan()
        manifest = json.loads(path.read_text(encoding="utf-8"))
        self.assertEqual(manifest["skipped"][0]["reason"], "additional_streams")

    def test_content_change_after_plan_rejected(self):
        plan = self.plan()
        self.paths[0].write_bytes(b"changed")
        with self.assertRaises(ValueError):
            self.run_action("apply", plan)
        self.assertEqual(len({p.stat().st_ino for p in self.paths}), 4)

    def test_manifest_tampering_rejected(self):
        path, sha = self.plan()
        path.write_bytes(path.read_bytes() + b" ")
        with self.assertRaisesRegex(ValueError, "hash"):
            tool.operate(path, sha, "apply")

    def test_outside_manifest_path_rejected_even_with_new_hash(self):
        path, _ = self.plan()
        manifest = json.loads(path.read_text(encoding="utf-8"))
        manifest["groups"][0][1]["path"] = "../outside"
        raw = json.dumps(manifest).encode()
        path.write_bytes(raw)
        with self.assertRaises(ValueError):
            tool.operate(path, hashlib.sha256(raw).hexdigest(), "apply")

    def test_writer_held_open_prevents_replacement(self):
        plan = self.plan()
        with self.paths[0].open("ab") as writer:
            with self.assertRaises(OSError):
                self.run_action("apply", plan)
            writer.write(b"")
        self.assertEqual(len({p.stat().st_ino for p in self.paths}), 4)

    def test_failure_before_replace_keeps_original_and_resumes(self):
        plan = self.plan()
        with mock.patch.object(tool, "replace", side_effect=OSError("injected replace failure")):
            with self.assertRaises(OSError):
                self.run_action("apply", plan)
        self.assertEqual(len({p.stat().st_ino for p in self.paths}), 4)
        self.run_action("apply", plan)
        self.run_action("verify", plan)
        self.assertFalse(any(".dedup-" in p.name for p in self.root.rglob("*")))

    def test_failure_after_replace_keeps_original_backup_and_resumes(self):
        plan = self.plan()
        original = tool.event
        failed = False
        def fail_once(directory, **value):
            nonlocal failed
            if value.get("action") == "verified" and not failed:
                failed = True
                raise OSError("injected journal failure")
            return original(directory, **value)
        with mock.patch.object(tool, "event", side_effect=fail_once):
            with self.assertRaises(OSError):
                self.run_action("apply", plan)
        self.assertTrue(any(p.name.endswith(".old") for p in self.root.rglob("*")))
        self.run_action("apply", plan)
        self.run_action("verify", plan)

    def test_rollback_checks_capacity_before_writes(self):
        plan = self.plan()
        self.run_action("apply", plan)
        with mock.patch.object(tool.shutil, "disk_usage", return_value=mock.Mock(free=0)):
            with self.assertRaises(RuntimeError):
                self.run_action("rollback", plan)
        self.run_action("verify", plan)

    def test_external_canonical_link_after_plan_rejected_before_changes(self):
        plan = self.plan()
        manifest = json.loads(plan[0].read_text(encoding="utf-8"))
        source = self.root / manifest["groups"][0][0]["path"]
        os.link(source, self.root / "external")
        with self.assertRaisesRegex(ValueError, "external"):
            self.run_action("apply", plan)
        self.assertEqual(len({p.stat().st_ino for p in self.paths}), 4)

    def test_concurrent_operation_rejected(self):
        plan = self.plan()
        with tool.operation_lock(self.output):
            with self.assertRaises(OSError):
                self.run_action("apply", plan)

    def test_torn_journal_line_does_not_hide_the_next_prepared_identity(self):
        plan = self.plan()
        self.run_action("apply", plan)
        with (self.output / "events.jsonl").open("ab") as stream:
            stream.write(b'{"incomplete":')
        self.run_action("rollback", plan)
        self.run_action("apply", plan)
        self.run_action("verify", plan)

    def test_original_readonly_and_creation_times_restored(self):
        for path in self.paths:
            tool.attrs(path, path.stat().st_file_attributes | tool.READONLY)
        expected = [tool.inspect(p)["creation_ticks"] for p in self.paths]
        plan = self.plan()
        self.run_action("apply", plan)
        self.run_action("rollback", plan)
        self.assertEqual([tool.inspect(p)["creation_ticks"] for p in self.paths], expected)
        self.assertTrue(all(p.stat().st_file_attributes & 1 for p in self.paths))
        self.run_action("apply", plan)
        self.run_action("verify", plan)

    def test_rollback_interrupted_copy_resumes(self):
        plan = self.plan()
        self.run_action("apply", plan)
        with mock.patch.object(tool.shutil, "copyfileobj", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                self.run_action("rollback", plan)
        self.run_action("rollback", plan)
        self.assertEqual(len({p.stat().st_ino for p in self.paths}), 4)
        self.assertFalse(any(".dedup-" in p.name for p in self.root.rglob("*")))

    def test_rollback_interrupted_after_replace_resumes(self):
        plan = self.plan()
        self.run_action("apply", plan)
        original = tool.event
        def fail_restored(directory, **value):
            if value.get("action") == "restored":
                raise OSError("journal failure")
            return original(directory, **value)
        with mock.patch.object(tool, "event", side_effect=fail_restored):
            with self.assertRaises(OSError):
                self.run_action("rollback", plan)
        self.run_action("rollback", plan)
        self.assertEqual(len({p.stat().st_ino for p in self.paths}), 4)
        self.run_action("apply", plan)
        self.run_action("verify", plan)



class TaskWindowTests(unittest.TestCase):
    def test_current_tasks_are_required_and_busy_or_imminent_tasks_block_writes(self):
        rows = [
            {"name": "AI Daily Report", "state": 3, "seconds": 3600},
            {"name": "AI Daily Report Health", "state": 3, "seconds": 3600},
        ]
        with mock.patch.object(tool.subprocess, "run") as run:
            run.return_value.stdout = json.dumps(rows).encode()
            tool.quiet_window()
            for invalid in [rows[:1], [dict(rows[0], state=4), rows[1]], [rows[0], dict(rows[1], seconds=299)]]:
                run.return_value.stdout = json.dumps(invalid).encode()
                with self.assertRaises(RuntimeError):
                    tool.quiet_window()


if __name__ == "__main__":
    unittest.main()
