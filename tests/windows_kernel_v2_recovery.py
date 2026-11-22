"""Exercise the isolated Phase 3 -> v2 -> Phase 3 recovery handoff."""

import argparse
from contextlib import closing, contextmanager
from datetime import datetime
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
from queue import Empty, Queue
import re
import runpy
import shutil
import sqlite3
import subprocess
import threading
import traceback


MAIN = "ai-daily-report.exe"
OLD_MANIFEST = "098b1ea3bf6803931aa0e8da0dab98fef4f74722e190ea52a8ff4f3da18d2eaa"
V3_CACHE = "scan_index_rust_v3.sqlite3"
V4_CACHE = "scan_cache_v4.sqlite3"
BODY_DIR = "daily_cache/snapshots"


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def data_sha(data):
    return hashlib.sha256(data).hexdigest()


def read_json(path):
    return json.loads(path.read_bytes())


def write_json(path, data):
    with path.open("x", encoding="utf-8", newline="\n") as stream:
        json.dump(data, stream, indent=2)
        stream.write("\n")


def readonly(path, *, immutable=False):
    uri = path.resolve(strict=True).as_uri() + "?mode=ro" + ("&immutable=1" if immutable else "")
    db = sqlite3.connect(uri, uri=True)
    db.row_factory = sqlite3.Row
    return closing(db)


def backup(source, target, *, immutable=False):
    assert not target.exists()
    with readonly(source, immutable=immutable) as original, closing(sqlite3.connect(target)) as copy:
        original.backup(copy)


@contextmanager
def model_fixture():
    class Reply(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            raw = self.rfile.read(int(self.headers["Content-Length"]))
            cut = self.server.cut_next
            self.server.cut_next = False
            self.server.requests.append({"path": self.path, "request_sha256": data_sha(raw),
                                         "incomplete_response": cut})
            before_reply = self.server.before_reply
            self.server.before_reply = None
            if before_reply is not None:
                before_reply()
            body = json.loads(raw)
            materials = json.loads(body["messages"][1]["content"])
            source = next(s for s in materials["sources"] if s["source_kind"] != "user_statement")
            report = {"schema_version": 5, "items": [{
                "title": "Purchase approval pending", "text": "17 units await approval.",
                "kind": "attention", "statement_kind": "observed",
                "source_ids": [source["source_id"]],
            }]}
            response = json.dumps({
                "choices": [{"finish_reason": "stop", "message": {"content": json.dumps(report)}}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 30},
            }).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(response)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(response[:8] if cut else response)
            self.wfile.flush()
            self.close_connection = True

    with ThreadingHTTPServer(("127.0.0.1", 0), Reply) as server:
        server.requests = []
        server.cut_next = False
        server.before_reply = None
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield server
        finally:
            server.shutdown()
            thread.join()


class Sandbox:
    def __init__(self, root, commands, port):
        self.root = root
        self.commands = commands
        root.mkdir(parents=True, exist_ok=False)
        for directory in ("work", "db", "data/daily_cache", "logs", "reports", "config", "tmp"):
            (root / directory).mkdir(parents=True)
        self.db = root / "db/reports.sqlite3"
        self.reports = root / "reports"
        self.snapshots = root / "data" / BODY_DIR
        paths = {key: str(root / name) for key, name in (
            ("work_dir", "work"), ("data_dir", "data"), ("db_dir", "db"),
            ("log_dir", "logs"), ("reports_dir", "reports"),
        )}
        settings = {"paths": paths, "report": {"profile": "personal"},
                    "archive": {"allow_work_dir_for_model": True}, "scanner": {"max_workers": 1},
                    "llm": {"provider": "deepseek", "base_url": f"http://127.0.0.1:{port}/v1",
                            "context_window_tokens": 128000, "verified_max_output_tokens": 8192}}
        write_json(root / "config/settings.yaml", settings)
        system = os.environ["SystemRoot"]
        self.env = {"SystemRoot": system, "WINDIR": system, "PATH": str(Path(system) / "System32"),
                    "TEMP": str(root / "tmp"), "TMP": str(root / "tmp"),
                    "DEEPSEEK_API_KEY": "offline-fixture", "NO_PROXY": "127.0.0.1,localhost",
                    "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9"}

    def add_material(self, day):
        path = self.root / "work" / (day + ".md")
        path.write_text("Purchase of 17 units awaits approval.\n", encoding="utf-8")
        stamp = datetime.fromisoformat(day + "T12:00:00+08:00").timestamp()
        os.utime(path, (stamp, stamp))

    def command(self, bundle, *argv, config_dir=None):
        return [str(part) for part in (bundle / MAIN, "--config-dir", config_dir or self.root / "config", *argv)]

    def run(self, bundle, *argv, expected=0, credential=True, config_dir=None):
        command = self.command(bundle, *argv, config_dir=config_dir)
        env = self.env.copy()
        if not credential:
            env.pop("DEEPSEEK_API_KEY")
        proc = subprocess.run(command, cwd=self.root, env=env, stdin=subprocess.DEVNULL,
                              capture_output=True, timeout=120, creationflags=subprocess.CREATE_NO_WINDOW)
        try:
            body = json.loads(proc.stdout or proc.stderr)
        except ValueError:
            body = {}
        error = body.get("error_code") if isinstance(body, dict) else None
        self.commands.append({"command": command, "exit_code": proc.returncode, "error_code": error,
                              "credential_available": credential,
                              "stdout_sha256": data_sha(proc.stdout), "stderr_sha256": data_sha(proc.stderr)})
        assert expected is None or proc.returncode == expected, (argv, expected, proc.returncode, error)
        return body

    def daily(self, bundle, day, *, expected=0, credential=True):
        return self.run(bundle, "daily", "--target-date", day, expected=expected, credential=credential)


def publication(database, reports, day):
    with readonly(database) as db:
        run, checksum, raw = db.execute(
            "SELECT r.run_id,r.expected_sha256,r.report_json FROM report_runs r "
            "JOIN daily_reports d ON r.run_id=d.current_run_id WHERE d.date=?", (day,),
        ).fetchone()
        calls = db.execute("SELECT count(*) FROM report_model_calls").fetchone()[0]
    attachment = json.loads(raw)["evidence_attachment"]
    expected = {checksum, attachment["sha256"]}
    files = {p: digest(p) for p in reports.rglob("*.md") if p.name.startswith(day)}
    assert expected == set(files.values()) and len(files) == 2, "published bytes differ from database"
    return run, files, calls


def regular_publication(box, old, new, server):
    assert not server.requests
    old_day, new_day = "2026-11-19", "2026-11-20"
    for day in (old_day, new_day):
        box.add_material(day)
    assert box.daily(old, old_day)["action"] == "published"
    old_id, old_files, old_calls = publication(box.db, box.reports, old_day)
    assert box.run(new, "repair-output", "--run-id", old_id)["model_calls"] == 0
    assert publication(box.db, box.reports, old_day)[1] == old_files
    assert box.daily(new, new_day)["action"] == "published"
    new_id, new_files, new_calls = publication(box.db, box.reports, new_day)
    assert new_calls > old_calls and new_calls == len(server.requests)
    search = ("search", "--query", "approval", "--format", "json")
    indexed = box.run(new, *search)
    reused = box.run(old, *search)
    assert indexed["results"] == reused["results"] and indexed["results"]
    assert reused["source_rows_read"] == 0
    originals = box.root / "originals"
    originals.mkdir()
    for path in new_files:
        assert path.resolve().is_relative_to(box.reports)
        path.rename(originals / path.name)
    recovered = box.run(old, "repair-output", "--target-date", new_day)
    assert recovered["run_id"] == new_id and recovered["model_calls"] == 0
    assert publication(box.db, box.reports, new_day) == (new_id, new_files, new_calls)
    assert publication(box.db, box.reports, old_day)[1] == old_files
    assert box.daily(old, new_day)["model_calls"] == 0 and len(server.requests) == new_calls
    return {"daily_reports": 2, "verified_files": 4, "local_model_calls": new_calls,
            "old_restored_v2": True, "hot_report_rows_read": 0,
            "v2_publication_sha256": {p.name: h for p, h in new_files.items()}}


def state(database, day, run_id=None):
    with readonly(database) as db:
        row, = db.execute(
            "SELECT * FROM report_runs WHERE target_date=?1 AND (?2 IS NULL OR run_id=?2)",
            (day, run_id),
        ).fetchall()
        run = dict(row)
        calls = [dict(row) for row in db.execute(
            "SELECT * FROM report_model_calls WHERE run_id=? ORDER BY call_id", (run["run_id"],))]
        pointer = db.execute("SELECT current_run_id FROM daily_reports WHERE date=?", (day,)).fetchone()
        count = db.execute("SELECT value FROM report_metadata WHERE key=?",
                           (f"personal_calls:{run['scope_id']}:{day}",)).fetchone()
        run["permanent_calls"] = json.loads(count[0])["used"] if count else 0
    return run, calls, pointer[0] if pointer else None


def retain_snapshots(box, run, calls):
    source = box.snapshots / run["run_id"]
    assert source.resolve(strict=True).is_relative_to(box.snapshots)
    retained = box.root / "handoff/snapshots" / run["run_id"]
    shutil.copytree(source, retained)
    files = {path.name: digest(path) for path in retained.iterdir()}
    assert files and all(name.endswith(hash_value + ".json") for name, hash_value in files.items())
    for record, keys in [(run, ["snapshot"]), *((row, ["request", "response"]) for row in calls)]:
        for key in keys:
            relative = record[f"{key}_path"]
            expected = record[f"{key}_sha256"]
            if relative is not None:
                ref = Path(relative)
                assert ref.parent == Path(run["run_id"]) and files[ref.name] == expected
            else:
                assert expected is None
    for call in calls:
        revision = json.loads(call["usage_json"])["request_audit"]["plan_revision"]
        ref = Path(revision["path"])
        assert ref.parent == Path(run["run_id"]) and files[ref.name] == revision["sha256"]
    frozen = read_json(retained / Path(run["snapshot_path"]).name)
    return frozen, files


def seal_handoff(box, day, run_id=None):
    before = state(box.db, day, run_id)
    run, calls, pointer = before
    handoff = box.root / "handoff"
    handoff.mkdir()
    retained_db = handoff / box.db.name
    backup(box.db, retained_db)
    assert state(retained_db, day, run_id) == before
    assert table_counts(retained_db) == table_counts(box.db)
    frozen, files = retain_snapshots(box, run, calls)
    result = {"run_id": run["run_id"], "generation_id": run["generation_id"], "phase": run["phase"],
              "status": run["status"], "error_code": run["error_code"], "current_run_id": pointer,
              "permanent_calls": run["permanent_calls"], "frozen_input_sha256": run["snapshot_sha256"],
              "frozen_input_version": frozen["version"], "plan_version": frozen["plan"]["version"],
              "retained_database_sha256": digest(retained_db), "snapshot_files": files,
              "model_calls": [{k: row[k] for k in ("call_id", "status", "attempt", "logical_step",
                              "request_fingerprint", "request_sha256", "response_sha256", "error_code")}
                              for row in calls]}
    write_json(handoff / "state.json", result)
    return before, result


def assert_recoverable(box, day, before):
    original, calls, pointer = before
    current, current_calls, current_pointer = state(box.db, day)
    for key in ("run_id", "generation_id", "phase", "status", "snapshot_path", "snapshot_sha256",
                "input_fingerprint", "config_fingerprint", "permanent_calls"):
        assert current[key] == original[key], key
    assert current_calls == calls and current_pointer == pointer
    retained = box.root / "handoff/snapshots" / original["run_id"]
    for path in retained.iterdir():
        assert digest(box.snapshots / original["run_id"] / path.name) == digest(path)
    return current["error_code"]


def committed_handoff(box, old, new, server):
    day = "2026-11-18"
    box.add_material(day)
    blocker = box.reports / day[:7] / "attachments"
    blocker.parent.mkdir()
    blocker.write_bytes(b"synthetic attachment directory failure\n")
    initial_calls = len(server.requests)
    error = box.daily(new, day, expected=1)
    assert error["error_code"] == "IO_FAILED"
    (run, calls, pointer), saved = seal_handoff(box, day)
    assert (run["phase"], pointer, run["permanent_calls"]) == ("db_committed", None, 1)
    assert len(calls) == 1 and calls[0]["status"] == "succeeded" and len(server.requests) == initial_calls + 1
    body = Path(run["output_path"])
    assert body == box.reports / day[:7] / (day + ".md") and not body.exists()
    markdown = run["markdown"].encode("utf-8")
    attachment = json.loads(run["report_json"])["evidence_attachment"]
    assert data_sha(markdown) == run["expected_sha256"]
    assert data_sha(attachment["markdown"].encode("utf-8")) == attachment["sha256"]
    blocker.rename(box.root / "handoff/attachment-directory-blocker")
    restored = box.daily(old, day)
    assert restored["run_id"] == run["run_id"] and restored["model_calls"] == 0
    published_run, files, final_calls = publication(box.db, box.reports, day)
    assert published_run == run["run_id"] and set(files.values()) == {run["expected_sha256"], attachment["sha256"]}
    assert final_calls == 1 and len(server.requests) == initial_calls + 1
    resumed, resumed_calls, resumed_pointer = state(box.db, day)
    assert resumed["permanent_calls"] == 1 and resumed_calls == calls and resumed_pointer == published_run
    assert box.run(new, "repair-output", "--run-id", run["run_id"])["model_calls"] == 0
    assert publication(box.db, box.reports, day) == (published_run, files, final_calls)
    return {"before": saved, "recovered_run_id": published_run, "model_calls_on_handoff": 0,
            "verified_files": {p.name: h for p, h in files.items()}}


def stage_v2_unknown(box, new, server, day):
    box.add_material(day)
    initial_calls = len(server.requests)
    server.cut_next = True
    failed = box.daily(new, day, expected=1)
    assert failed["error_code"] == "MODEL_RESPONSE_INTERRUPTED"
    before = state(box.db, day)
    run, calls, pointer = before
    assert (run["phase"], run["status"], pointer, run["permanent_calls"]) == ("generating", "failed", None, 1)
    assert len(server.requests) == initial_calls + 1 and server.requests[-1]["incomplete_response"]
    call, = calls
    assert call["status"] == "unknown" and not call["response_path"] and not call["response_sha256"]
    assert call["request_sha256"] == server.requests[-1]["request_sha256"]
    return before


def v2_unknown(box, old, new, server):
    day = "2026-11-18"
    staged = stage_v2_unknown(box, new, server, day)
    initial_calls = len(server.requests)
    before, saved = seal_handoff(box, day)
    assert before == staged
    guard = box.daily(new, day, expected=1)
    assert guard["error_code"] == "MODEL_CALL_RESULT_UNKNOWN"
    assert_recoverable(box, day, before)
    declined = box.daily(old, day, expected=1)
    assert declined["error_code"] in ("JSON_INVALID", "SNAPSHOT_CONTEXT_MISMATCH")
    assert len(server.requests) == initial_calls
    assert_recoverable(box, day, before)
    assert_recovery = box.daily(new, day, expected=1)
    assert assert_recovery["error_code"] == "MODEL_CALL_RESULT_UNKNOWN"
    assert len(server.requests) == initial_calls
    assert_recoverable(box, day, before)
    return {"before": saved, "old_error_code": declined["error_code"], "v2_error_code": guard["error_code"],
            "additional_requests": 0, "model_call_rows_retained": 1}


def v2_plan(box, old, new, server):
    day = "2026-11-18"
    box.add_material(day)
    initial_calls = len(server.requests)
    blocked = box.daily(new, day, expected=1, credential=False)
    assert blocked["error_code"] == "MODEL_CREDENTIAL_MISSING", blocked.get("error_code")
    before, saved = seal_handoff(box, day)
    run, calls, pointer = before
    assert (run["phase"], run["status"], calls, pointer, run["permanent_calls"]) == ("prepared", "failed", [], None, 0)
    assert (saved["frozen_input_version"], saved["plan_version"]) == (7, 8)
    assert len(server.requests) == initial_calls
    declined = box.daily(old, day, expected=1)
    assert declined["error_code"] in ("JSON_INVALID", "SNAPSHOT_CONTEXT_MISMATCH")
    assert len(server.requests) == initial_calls
    assert_recoverable(box, day, before)
    resumed = box.daily(new, day)
    assert resumed["run_id"] == run["run_id"] and len(server.requests) == initial_calls + 1
    result = publication(box.db, box.reports, day)
    assert result[0] == run["run_id"] and result[2] == 1
    assert state(box.db, day)[0]["permanent_calls"] == 1
    assert box.run(old, "repair-output", "--run-id", run["run_id"])["model_calls"] == 0
    assert publication(box.db, box.reports, day) == result and len(server.requests) == initial_calls + 1
    return {"before": saved, "old_error_code": declined["error_code"], "old_requests": 0,
            "v2_resume_requests": 1, "restored_run_id": result[0],
            "verified_files": {p.name: h for p, h in result[1].items()}}


def stage_legacy_unknown(box, old, server, *, earlier_run=None):
    day = "2026-11-18"
    options = ()
    if earlier_run is None:
        box.add_material(day)
    else:
        options = ("--regenerate", "--reason", "M4 second pending run for the same date")
    initial_calls = len(server.requests)
    original_reports = box.root / "empty-reports"

    def block_publication():
        # Wait until the running old binary has accepted the output directory.
        box.reports.rename(original_reports)
        box.reports.write_bytes(b"synthetic output-directory failure\n")

    server.cut_next = True
    server.before_reply = block_publication
    staged = box.run(old, "daily", "--target-date", day, *options, expected=1)
    assert staged["error_code"] == "IO_FAILED", staged.get("error_code")
    with readonly(box.db) as db:
        runs = [row["run_id"] for row in db.execute(
            "SELECT run_id FROM report_runs WHERE target_date=? ORDER BY created_at, run_id", (day,)
        )]
    assert runs[:-1] == ([earlier_run] if earlier_run else [])
    before, saved = seal_handoff(box, day, runs[-1])
    run, calls, pointer = before
    assert (saved["frozen_input_version"], saved["plan_version"], pointer) == (6, 7, None)
    call, = calls
    expected_calls = 1 if earlier_run is None else 2
    assert (run["phase"], call["status"], run["permanent_calls"]) == ("generating", "unknown", expected_calls)
    assert len(server.requests) == initial_calls + 1 and call["request_sha256"] == server.requests[-1]["request_sha256"]
    box.reports.rename(box.root / "handoff/report-directory-blocker")
    original_reports.rename(box.reports)
    return day, before, saved


def legacy_unknown(box, old, new, server):
    day, before, saved = stage_legacy_unknown(box, old, server)
    run, calls, _ = before
    call, = calls
    listed = box.run(new, "runs", "--target-date", day)
    assert [row["run_id"] for row in listed] == [run["run_id"]]
    assert_recoverable(box, day, before)
    prior_requests = len(server.requests)
    box.daily(old, day, expected=None)
    more = server.requests[prior_requests:]
    current, after_calls, current_pointer = state(box.db, day)
    preserved = (after_calls[:1] == calls and current["snapshot_sha256"] == run["snapshot_sha256"]
                 and current["generation_id"] == run["generation_id"])
    blocked = (not more and after_calls == calls and preserved and current_pointer is None and
               current["permanent_calls"] == run["permanent_calls"])
    return {"status": "passed" if blocked else "failed",
            "before": saved, "additional_requests": len(more),
            "same_request_repeated": any(row["request_sha256"] == call["request_sha256"] for row in more),
            "new_attempts": [{key: row[key] for key in ("call_id", "logical_step", "attempt", "status",
                                                        "request_fingerprint", "request_sha256")}
                             for row in after_calls[1:]],
            "model_call_rows_after": len(after_calls), "original_call_and_snapshot_preserved": preserved,
            "permanent_calls_after": current["permanent_calls"], "current_run_id_after": current_pointer,
            "after_phase": current["phase"], "after_status": current["status"]}


def rollback_arguments(box, previous, candidate):
    previous_config = box.root / "previous-config"
    shutil.copytree(box.root / "config", previous_config)
    return ("task", "rollback-guard", "--previous-config-dir", previous_config,
            "--previous-root", previous, "--candidate-root", candidate)


def valid_target_sha256(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def guard_handshake(line):
    try:
        message = json.loads(line) if line.endswith(b"\n") else None
    except ValueError:
        message = None
    ready = isinstance(message, dict) and message.get("ready") is True
    fingerprint = message.get("target_sha256") if isinstance(message, dict) else None
    target = fingerprint if valid_target_sha256(fingerprint) else None
    legacy = ready and "target_sha256" not in message
    return {"ready": ready, "target_sha256": target, "legacy_ready_only": legacy}


@contextmanager
def rollback_guard(box, candidate, arguments):
    command = box.command(candidate, *arguments)
    event = {"command": command, **guard_handshake(b"")}
    box.commands.append(event)
    guard = subprocess.Popen(command, cwd=box.root, env=box.env, stdin=subprocess.PIPE,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             creationflags=subprocess.CREATE_NO_WINDOW)
    lines = Queue(maxsize=1)
    reader = threading.Thread(target=lambda: lines.put(guard.stdout.readline()), daemon=True)
    reader.start()
    first = b""
    try:
        first = lines.get(timeout=20)
        event.update(guard_handshake(first))
        event["alive_after_ready"] = event["ready"] and guard.poll() is None
        yield event, guard
    except Empty as error:
        raise AssertionError("rollback-guard did not provide a line or close stdout") from error
    finally:
        guard.stdin.close()
        guard.stdin = None
        reader.join(timeout=10)
        try:
            stdout, stderr = guard.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            guard.kill()
            guard.communicate()
            raise
        try:
            error = json.loads(stderr)
        except ValueError:
            error = {}
        event.update(exit_code=guard.returncode, error_code=error.get("error_code") if isinstance(error, dict) else None,
                     command_unavailable=b"unrecognized subcommand 'rollback-guard'" in stderr,
                     stdout_sha256=data_sha(first + stdout), stderr_sha256=data_sha(stderr))


def guard_holds_lock(box, old, new, server):
    day = "2026-11-18"
    staged = stage_v2_unknown(box, new, server, day)
    state_before, saved = seal_handoff(box, day)
    assert state_before == staged and (saved["frozen_input_version"], saved["plan_version"]) == (7, 8)
    before = len(server.requests)
    database_sha = digest(box.db)
    backup_path = box.root / "copy.sqlite3"
    arguments = rollback_arguments(box, old, new)
    with rollback_guard(box, new, arguments) as (lease, guard):
        if lease["alive_after_ready"] and lease["target_sha256"]:
            blocked = box.run(old, "backup", "--output", backup_path, expected=1)
            assert blocked["error_code"] == "REPORT_ALREADY_RUNNING" and not backup_path.exists()
            assert guard.poll() is None, "guard released the report lock before stdin EOF"
    if not (lease["ready"] and lease["target_sha256"] and lease["alive_after_ready"] and lease["exit_code"] == 0):
        return {"status": "failed", "guard": lease, "failure": "guard did not hold a target-bound lease"}
    identity = box.run(new, *arguments, "--identity-only")
    assert "ready" not in identity and identity["target_sha256"] == lease["target_sha256"]
    changed_targets = {}
    for key in ("db_dir", "data_dir", "reports_dir"):
        alternate = box.root / "target-identity" / key
        config = alternate / "config"
        config.mkdir(parents=True)
        directory = alternate / "changed-path"
        directory.mkdir()
        if key == "db_dir":
            backup(box.db, directory / box.db.name)
        settings = read_json(box.root / "config/settings.yaml")
        settings["paths"][key] = str(directory)
        write_json(config / "settings.yaml", settings)
        changed = box.run(new, "task", "rollback-guard", "--previous-config-dir", config,
                          "--previous-root", old, "--candidate-root", new, "--identity-only", config_dir=config)
        target = changed["target_sha256"]
        assert "ready" not in changed and valid_target_sha256(target) and target != lease["target_sha256"], key
        changed_targets[key] = target
    assert len(set(changed_targets.values())) == 3
    result = box.run(old, "backup", "--output", backup_path)
    assert backup_path.is_file() and result["sha256"] == digest(backup_path)
    assert digest(box.db) == database_sha and len(server.requests) == before
    assert state(box.db, day) == state_before
    return {"guard": lease, "contention_error": blocked["error_code"],
            "v2_only_unknown_allowed": saved,
            "identity_only_target_sha256": identity["target_sha256"],
            "changed_path_target_sha256": changed_targets,
            "lease_released_on_stdin_eof": True, "backup_sha256": result["sha256"], "model_requests": 0}


def guard_refuses_unknown(box, old, new, server, *, earlier_v2_unknown=False):
    earlier_run_id = None
    earlier_receipt = None
    if earlier_v2_unknown:
        earlier_run, earlier_calls, current = stage_v2_unknown(box, new, server, "2026-11-18")
        assert current is None
        earlier_run_id = earlier_run["run_id"]
    day, before, saved = stage_legacy_unknown(box, old, server, earlier_run=earlier_run_id)
    if earlier_run_id is not None:
        earlier_run["permanent_calls"] = 2
        earlier_state = (earlier_run, earlier_calls, None)
        assert state(box.db, day, earlier_run_id) == earlier_state
        assert state(box.root / "handoff/reports.sqlite3", day, earlier_run_id) == earlier_state
        assert earlier_run["created_at"] < before[0]["created_at"]
        frozen, files = retain_snapshots(box, earlier_run, earlier_calls)
        assert (frozen["version"], frozen["plan"]["version"]) == (7, 8)
        earlier_receipt = {"run_id": earlier_run_id, "phase": earlier_run["phase"],
                           "call_status": earlier_calls[0]["status"], "frozen_input_version": frozen["version"],
                           "plan_version": frozen["plan"]["version"], "snapshot_files": files}
    first_command = len(box.commands)
    first_request = len(server.requests)
    db_sha = digest(box.db)
    files_before = file_inventory(box.snapshots)
    arguments = rollback_arguments(box, old, new)
    with rollback_guard(box, new, arguments) as (lease, _):
        allowed = lease["alive_after_ready"] and lease["target_sha256"]
    if allowed and lease["exit_code"] == 0:
        box.daily(old, day, expected=None)
    old_launches = sum(row["command"][0] == str(old / MAIN) for row in box.commands[first_command:])
    extra_requests = len(server.requests) - first_request
    unchanged = (state(box.db, day, saved["run_id"]) == before and digest(box.db) == db_sha and
                 file_inventory(box.snapshots) == files_before)
    refused = (not lease["ready"] and lease["target_sha256"] is None and lease["exit_code"] != 0 and
               lease["error_code"] == "ROLLBACK_MODEL_OUTCOME_UNKNOWN")
    return {"status": "passed" if refused and unchanged and not old_launches and not extra_requests else "failed",
            "guard": lease, "before": saved, "refused_dangerous_state": refused, "old_process_launches": old_launches,
            "earlier_v2_unknown": earlier_receipt,
            "additional_requests": extra_requests, "unknown_ledger_and_materials_unchanged": unchanged}


def table_counts(path):
    with readonly(path) as db:
        names = [row[0] for row in db.execute("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")]
        assert all(name.replace("_", "").isalnum() for name in names)
        return {name: db.execute(f'SELECT count(*) FROM "{name}"').fetchone()[0] for name in names}


def file_inventory(root):
    files = [path for path in root.rglob("*") if path.is_file()]
    assert all(path.resolve(strict=True) == path for path in files)
    return {path.relative_to(root).as_posix(): digest(path) for path in sorted(files)}


def historical_health(box, old, new, baseline_path, published, commands):
    baseline = read_json(baseline_path)
    assert baseline["status"] == "passed" and baseline["unfinished"]["deployment_counts"] == {
        "running": 0, "resumable": 0, "reserved": 0}
    sealed = baseline["databases"]
    source_paths = {name: Path(sealed[name]["path"]).resolve(strict=True)
                    for name in (box.db.name, V3_CACHE)}
    reports_source = source_paths[box.db.name]
    retained_deployment = baseline_path.parents[2] / "kernel-phase3-deployment-20261122"
    for name, path in source_paths.items():
        assert path.name == name and path.is_relative_to(retained_deployment) and path.parent.name == "before"
        assert digest(path) == sealed[name]["sha256"]
        assert all(not path.with_name(path.name + suffix).exists() for suffix in ("-wal", "-shm", "-journal"))
        backup(path, box.root / "db" / name, immutable=True)
    expected_counts = {name: table["rows"] for name, table in sealed[box.db.name]["primary_key_fingerprints"].items()}
    assert {name: expected_counts[name] for name in ("daily_reports", "weekly_reports", "monthly_reports",
            "report_model_calls")} == {"daily_reports": 223, "weekly_reports": 1, "monthly_reports": 1,
                                      "report_model_calls": 1668}
    assert table_counts(box.db) == expected_counts
    expected_files = {name.replace("\\", "/"): checksum
                      for name, checksum in baseline["retained_file_inventory"]["published"]["files"].items()}
    assert len(expected_files) == 283 and file_inventory(published) == expected_files
    shutil.copytree(published, box.reports, dirs_exist_ok=True)
    assert file_inventory(box.reports) == expected_files

    with closing(sqlite3.connect(box.db)) as db, db:
        stored_paths = db.execute("SELECT run_id,target_date,output_path FROM report_runs WHERE output_path IS NOT NULL").fetchall()
        for run_id, day, path in stored_paths:
            assert Path(path).name == day + ".md" and len(day) == 10
            db.execute("UPDATE report_runs SET output_path=? WHERE run_id=?",
                       (str(box.reports / day[:7] / (day + ".md")), run_id))

    archive = reports_source.parent.parent / "runtime/health-after.json"
    witness = read_json(archive)["comparison"]
    expected_checks = sorted(witness["checks"])
    day = witness["target_date"]
    assert day == "2026-11-20" and witness["status"] == "unhealthy" and witness["exit"] == 2
    assert expected_checks == [
        ["DISK_SPACE", "ok"], ["HISTORICAL_REPORT_ATTENTION", "error"], ["PROCESSING_CACHE", "ok"],
        ["REPORT_CONTENT_PARTIAL", "warning"], ["REPORT_NOT_PUBLISHED", "ok"], ["RUN_ATTENTION", "ok"],
        ["SCAN_INDEX_INTEGRITY", "ok"],
    ]

    bootstrap = Sandbox(box.root.parent / "health-v4-bootstrap", commands, 9)
    bootstrap.add_material(day)
    preview = bootstrap.run(new, "daily", "--target-date", day, "--dry-run")
    assert preview["action"] == "dry_run" and preview["model_calls"] == 0
    backup(bootstrap.root / "db" / V4_CACHE, box.root / "db" / V4_CACHE)
    before_sha = digest(box.db)
    readings = {}
    for label, bundle in (("phase3", old), ("v2", new)):
        result = box.run(bundle, "health", "--target-date", day, "--format", "json", expected=2)
        assert result["status"] == witness["status"] and result["target_date"] == day
        assert sorted([row["code"], row["level"]] for row in result["checks"]) == expected_checks
        assert digest(box.db) == before_sha and table_counts(box.db) == expected_counts
        assert not box.db.with_name(box.db.name + "-wal").is_file() or not box.db.with_name(box.db.name + "-wal").stat().st_size
        readings[label] = {row["code"]: row["evidence"] for row in result["checks"]}
    # Disk bytes can change during the two observations. Its level is checked above.
    fixed = lambda rows: {code: evidence for code, evidence in rows.items() if code != "DISK_SPACE"}
    assert fixed(readings["phase3"]) == fixed(readings["v2"])
    assert readings["phase3"]["HISTORICAL_REPORT_ATTENTION"]["reports"] == witness["historical"]
    assert file_inventory(box.reports) == expected_files
    assert all(digest(source) == sealed[name]["sha256"] for name, source in source_paths.items())
    return {"target_date": day, "comparisons": expected_checks, "health_cli_exit": 2,
            "published_hashes_verified": len(expected_files), "relocated_output_paths": len(stored_paths),
            "baseline_table_counts": expected_counts, "phase3_table_counts": table_counts(box.db),
            "v2_table_counts": table_counts(box.db), "shared_database_sha256": before_sha,
            "health_baseline_sha256": digest(archive),
            "shared_evidence_sha256": data_sha(json.dumps(fixed(readings["phase3"]), sort_keys=True).encode())}


def verify(old, new, output, baseline, published, candidate_sha):
    acceptance = baseline.parent.parent
    assert output.is_relative_to(acceptance) and output != acceptance
    assert published.is_relative_to(acceptance) and published.name == "reports"
    assert old != new
    verifier = runpy.run_path(str(baseline.parent / "build_drivers.py"))["package"]
    old_package, new_package = verifier(old, OLD_MANIFEST), verifier(new, candidate_sha)
    assert read_json(new / "manifest.json")["schema_version"] == "ai_daily_rust_release_v2"
    assert read_json(new / "manifest.json")["ocr_enabled"] is False
    output.mkdir(parents=True, exist_ok=False)
    commands = []
    proof = {"status": "failed", "old_manifest_sha256": OLD_MANIFEST, "candidate_manifest_sha256": candidate_sha,
             "baseline_history_sha256": digest(baseline), "test_script_sha256": digest(Path(__file__)),
             "old_executable_sha256": old_package["files"][MAIN], "new_executable_sha256": new_package["files"][MAIN],
             "remote_model_calls": 0, "checks": {}, "negative_controls": {}, "commands": commands}

    def check(name, action, *, results=None):
        report = proof["checks"] if results is None else results
        try:
            report[name] = {"status": "passed", **action()}
        except (AssertionError, OSError, sqlite3.Error, subprocess.TimeoutExpired, ValueError, KeyError, TypeError) as error:
            frame = traceback.extract_tb(error.__traceback__)[-1]
            report[name] = {"status": "failed", "failure_type": type(error).__name__,
                            "location": f"{Path(frame.filename).name}:{frame.lineno}",
                            "failure": str(error)[:500]}

    try:
        with model_fixture() as server:
            for name, case in (("old_new_old", regular_publication), ("db_committed", committed_handoff),
                               ("v2_result_unknown", v2_unknown), ("v2_unfinished_plan", v2_plan),
                               ("rollback_guard_lock", guard_holds_lock), ("rollback_guard_unknown", guard_refuses_unknown)):
                check(name, lambda: case(Sandbox(output / name, commands, server.server_port), old, new, server))
            check("rollback_guard_same_day_unknown", lambda: guard_refuses_unknown(
                Sandbox(output / "rollback_guard_same_day_unknown", commands, server.server_port),
                old, new, server, earlier_v2_unknown=True))
            check("legacy_result_unknown", lambda: legacy_unknown(
                Sandbox(output / "legacy_result_unknown", commands, server.server_port), old, new, server),
                results=proof["negative_controls"])
            proof["loopback_requests"] = server.requests
        check("historical_health", lambda: historical_health(
            Sandbox(output / "history", commands, 9), old, new, baseline, published, commands))
        control = proof["negative_controls"]["legacy_result_unknown"]
        proof["negative_control_reproduced"] = control.get("same_request_repeated") is True
        proof["status"] = "passed" if (proof["negative_control_reproduced"] and
                                     all(check["status"] == "passed" for check in proof["checks"].values())) else "failed"
    finally:
        write_json(output / "receipt.json", proof)
    print(proof["status"], {name: check["status"] for name, check in proof["checks"].items()}, output / "receipt.json")
    return 0 if proof["status"] == "passed" else 2


if __name__ == "__main__":
    assert __debug__
    options = argparse.ArgumentParser(description=__doc__)
    for name in ("previous", "candidate", "output", "baseline-history", "published-evidence"):
        options.add_argument("--" + name, type=Path, required=True)
    options.add_argument("--candidate-manifest-sha256", required=True)
    args = options.parse_args()
    raise SystemExit(verify(
        args.previous.resolve(strict=True), args.candidate.resolve(strict=True), args.output.resolve(),
        args.baseline_history.resolve(strict=True), args.published_evidence.resolve(strict=True),
        args.candidate_manifest_sha256,
    ))
