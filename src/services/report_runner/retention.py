"""Delete only expired, application-owned snapshots with no unfinished dependants."""

from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
import re

from ...models.report_settings import RetentionSettings
from ..report_run_store import ReportRunStore
from .errors import ReportRunError
from .snapshots import SnapshotStore, is_redirect


@dataclass(frozen=True, slots=True)
class RetentionResult:
    deleted_files: int
    reclaimed_bytes: int
    deleted_runs: int


def cleanup_snapshots(runs: ReportRunStore, snapshots: SnapshotStore, settings: RetentionSettings,
                      *, now: datetime) -> RetentionResult:
    now = now.astimezone(UTC)
    file_cutoff = now - timedelta(days=settings.input_snapshot_days)
    run_cutoff = now - timedelta(days=settings.terminal_run_days)
    with runs.store._get_conn() as connection:
        rows = [dict(row) for row in connection.execute("SELECT * FROM report_runs")]
        current = {row[0] for row in connection.execute("SELECT current_run_id FROM daily_reports WHERE current_run_id IS NOT NULL")}
    protected = {run["run_id"] for run in rows if run["status"] == "running" or
        (run["run_kind"] == "report" and run["phase"] in {"prepared", "generating", "db_committed"}
         and run["status"] != "succeeded" and run["error_code"] != "RUN_SUPERSEDED")}
    generations = {run["generation_id"] for run in rows if run["run_id"] in protected}
    protected.update(run["run_id"] for run in rows if run["generation_id"] in generations)
    deleted_files = reclaimed = deleted_runs = 0
    for run in rows:
        if run["run_id"] in protected or not run["finished_at"]:
            continue
        finished = datetime.fromisoformat(run["finished_at"])
        if finished >= file_cutoff:
            continue
        if not re.fullmatch(r"[0-9a-f]{32}", run["run_id"]):
            raise ReportRunError("RETENTION_RUN_PATH_INVALID")
        directory = snapshots.root / run["run_id"]
        if not directory.resolve().is_relative_to(snapshots.root) or (directory.exists() and is_redirect(directory)):
            raise ReportRunError("RETENTION_PATH_REDIRECTED")
        candidates = []
        if directory.exists():
            for path in directory.iterdir():
                if (not path.resolve().is_relative_to(snapshots.root) or is_redirect(path) or not path.is_file()
                        or not re.fullmatch(r"(?:[a-z][a-z0-9_-]*-[0-9a-f]{64}\.json|\.pending-[0-9a-f]{32})", path.name)):
                    raise ReportRunError("RETENTION_UNRECOGNIZED_FILE")
                candidates.append((path, path.stat().st_size))
        # Resolve and verify the complete deletion set before the first unlink.
        for path, size in candidates:
            if path.parent.resolve() != directory.resolve() or is_redirect(path):
                raise ReportRunError("RETENTION_PATH_REDIRECTED")
            path.unlink()
            deleted_files += 1
            reclaimed += size
        if directory.exists():
            directory.rmdir()
        with runs.store._get_conn() as connection:
            connection.execute("BEGIN IMMEDIATE")
            connection.execute("UPDATE report_runs SET snapshot_path=NULL,snapshot_sha256=NULL WHERE run_id=?", (run["run_id"],))
            connection.execute("""UPDATE report_model_calls SET request_path=NULL,request_sha256=NULL,response_path=NULL,
                response_sha256=NULL WHERE run_id=?""", (run["run_id"],))
            # Current reports and their source metadata live indefinitely. Call
            # reservations survive at least the terminal retention interval.
            if finished < run_cutoff and run["run_id"] not in current:
                connection.execute("DELETE FROM report_runs WHERE run_id=?", (run["run_id"],))
                deleted_runs += 1
    return RetentionResult(deleted_files, reclaimed, deleted_runs)
