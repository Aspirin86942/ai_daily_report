"""测试 context scheduler 的策略调度与审计落库。"""

from datetime import date
from pathlib import Path

from src.models.schemas import FileContext, ScanResult
from src.services.context_compressor import (
    ACTION_COMPRESS,
    ACTION_KEEP,
    ACTION_METADATA_ONLY,
)
from src.services.context_scheduler import ContextScheduleRequest, ContextScheduler


class StubStore:
    def __init__(self) -> None:
        self.context_runs: list[dict[str, object]] = []
        self.context_decisions: list[object] = []

    def latest_scan_run_detail(self) -> dict[str, int]:
        return {"run_id": 77}

    def save_context_run(self, **kwargs) -> int:
        self.context_runs.append(kwargs)
        return 123

    def save_context_decisions(self, context_run_id: int, decisions) -> None:
        self.context_decisions.append((context_run_id, list(decisions)))

    def save_context_run_with_decisions(self, *, decisions, **kwargs) -> int:
        context_run_id = self.save_context_run(**kwargs)
        self.save_context_decisions(context_run_id, decisions)
        return context_run_id


class FailingDecisionStore(StubStore):
    def save_context_decisions(self, context_run_id: int, decisions) -> None:
        raise RuntimeError("decision insert failed")

    def save_context_run_with_decisions(self, *, decisions, **kwargs) -> int:
        raise RuntimeError("decision insert failed")


class AtomicSaveFailingStore(StubStore):
    def save_context_run_with_decisions(self, *, decisions, **kwargs) -> int:
        raise RuntimeError("atomic save failed")


class StubScanner:
    def __init__(self, scan_result: ScanResult, store: StubStore | None = None) -> None:
        self.scan_result = scan_result
        self.calls: list[tuple[date, date, bool]] = []
        self.scan_index_store = store or StubStore()

    def scan_files(
        self,
        start_date: date,
        end_date: date,
        summary_mode: bool = False,
    ) -> ScanResult:
        self.calls.append((start_date, end_date, summary_mode))
        return self.scan_result


class FailingCompressor:
    def compress(self, **kwargs: object) -> None:
        raise RuntimeError("compress failed")


def test_scheduler_builds_weekly_context_and_records_audit(tmp_path: Path) -> None:
    """weekly scan 应启用 summary mode，并保存 run 与逐文件决策。"""
    start_date = date(2026, 5, 10)
    end_date = date(2026, 5, 24)
    small_file = tmp_path / "notes.md"
    large_file = tmp_path / "logs" / "app.log"
    small_file.write_text("# Small\nweekly evidence", encoding="utf-8")
    large_file.parent.mkdir()
    large_file.write_text("large log line\n" * 700, encoding="utf-8")
    scan_result = ScanResult(
        total_files=2,
        success_count=2,
        error_count=0,
        contexts=[
            FileContext(
                file_path=str(large_file),
                file_type=".log",
                content=large_file.read_text(encoding="utf-8"),
                parser_backend="light_text_v1",
            ),
            FileContext(
                file_path=str(small_file),
                file_type=".md",
                content=small_file.read_text(encoding="utf-8"),
                parser_backend="light_text_v1",
            ),
        ],
    )
    scanner = StubScanner(scan_result)
    scheduler = ContextScheduler(scanner_factory=lambda: scanner)

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="weekly",
            source="scan",
            start_date=start_date,
            end_date=end_date,
        )
    )

    assert scanner.calls == [(start_date, end_date, True)]
    assert result.context_run_id == 123
    assert "# Small" in result.file_context
    assert [decision.action for decision in result.decisions] == [
        ACTION_KEEP,
        ACTION_COMPRESS,
    ]
    assert scanner.scan_index_store.context_runs[0]["report_mode"] == "weekly"
    assert scanner.scan_index_store.context_runs[0]["scan_run_id"] == 77
    assert scanner.scan_index_store.context_decisions[0][0] == 123


def test_scheduler_marks_oversized_file_as_metadata_only(tmp_path: Path) -> None:
    """超过 large file 策略的文件只进入元数据摘要，不把预览正文放进上下文。"""
    huge_file = tmp_path / "evidence.xlsx"
    huge_file.write_bytes(b"0" * (11 * 1024 * 1024))
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[
            FileContext(
                file_path=str(huge_file),
                file_type=".xlsx",
                content="sheet preview",
                parser_backend="office_v1",
                truncated=True,
            )
        ],
    )
    scanner = StubScanner(scan_result)
    scheduler = ContextScheduler(scanner_factory=lambda: scanner)

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="monthly",
            source="scan",
            start_date=date(2026, 5, 1),
            end_date=date(2026, 5, 24),
        )
    )

    assert result.decisions[0].action == ACTION_METADATA_ONLY
    assert result.decisions[0].reason == "file_size_policy"
    assert "sheet preview" not in result.file_context


def test_scheduler_records_error_run_when_compressor_fails(tmp_path: Path) -> None:
    """compressor 失败时仍返回可读 fallback，并尽量保存 error run 审计。"""
    sample_file = tmp_path / "daily.md"
    sample_file.write_text("daily evidence", encoding="utf-8")
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[
            FileContext(
                file_path=str(sample_file),
                file_type=".md",
                content="daily evidence",
                parser_backend="light_text_v1",
            )
        ],
    )
    scanner = StubScanner(scan_result)
    scheduler = ContextScheduler(
        scanner_factory=lambda: scanner,
        compressor=FailingCompressor(),
    )

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="daily",
            source="scan",
            start_date=date(2026, 5, 24),
            end_date=date(2026, 5, 24),
        )
    )

    assert result.error == "compress failed"
    assert "文件上下文构建失败" in result.file_context
    assert scanner.scan_index_store.context_runs[0]["status"] == "error"
    assert scanner.scan_index_store.context_runs[0]["error"] == "compress failed"


def test_scheduler_does_not_leave_success_run_when_decision_save_fails(
    tmp_path: Path,
) -> None:
    """逐文件决策落库失败时，不应残留误导性的 success run。"""
    sample_file = tmp_path / "weekly.md"
    sample_file.write_text("weekly evidence", encoding="utf-8")
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[
            FileContext(
                file_path=str(sample_file),
                file_type=".md",
                content="weekly evidence",
                parser_backend="light_text_v1",
            )
        ],
    )
    store = FailingDecisionStore()
    scanner = StubScanner(scan_result, store=store)
    scheduler = ContextScheduler(scanner_factory=lambda: scanner)

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="weekly",
            source="scan",
            start_date=date(2026, 5, 10),
            end_date=date(2026, 5, 24),
        )
    )

    assert result.error == "decision insert failed"
    assert "文件上下文构建失败" in result.file_context
    assert [run["status"] for run in store.context_runs] == ["error"]


def test_scheduler_preserves_compressed_audit_when_atomic_save_fails(
    tmp_path: Path,
) -> None:
    """压缩成功后持久化失败时，error audit 应保留真实压缩统计和决策。"""
    sample_file = tmp_path / "weekly.md"
    sample_file.write_text("weekly evidence", encoding="utf-8")
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[
            FileContext(
                file_path=str(sample_file),
                file_type=".md",
                content="weekly evidence",
                parser_backend="light_text_v1",
            )
        ],
    )
    store = AtomicSaveFailingStore()
    scanner = StubScanner(scan_result, store=store)
    scheduler = ContextScheduler(scanner_factory=lambda: scanner)

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="weekly",
            source="scan",
            start_date=date(2026, 5, 10),
            end_date=date(2026, 5, 24),
        )
    )

    assert result.error == "atomic save failed"
    assert "文件上下文构建失败" in result.file_context
    assert [run["status"] for run in store.context_runs] == ["error"]
    error_run = store.context_runs[0]
    assert error_run["included_file_count"] == 1
    assert error_run["input_chars"] > 0
    assert error_run["output_chars"] > 0

    saved_context_run_id, saved_decisions = store.context_decisions[0]
    assert saved_context_run_id == 123
    assert saved_decisions[0].action == ACTION_KEEP
    assert saved_decisions[0].output_chars > 0
    assert result.decisions[0].action == ACTION_KEEP
    assert result.decisions[0].output_chars > 0
