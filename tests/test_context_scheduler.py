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
from src.services.python_legacy_context_engine import PythonLegacyContextEngine


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
        raise RuntimeError("secret-value-must-not-be-persisted")


def _legacy_scheduler(
    scanner: StubScanner,
    *,
    compressor=None,
) -> ContextScheduler:
    return ContextScheduler(
        engine=PythonLegacyContextEngine(
            scanner_factory=lambda: scanner,
            compressor=compressor,
            request_id_factory=lambda: "11111111-1111-4111-8111-111111111111",
        )
    )


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
        scan_run_id=77,
    )
    scanner = StubScanner(scan_result)
    scheduler = _legacy_scheduler(scanner)

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
    saved_decisions = scanner.scan_index_store.context_decisions[0][1]
    assert [decision.action for decision in saved_decisions] == [
        ACTION_KEEP,
        ACTION_COMPRESS,
    ]
    assert result.status == "ok"
    assert result.summary.source_file_count == 2
    assert scanner.scan_index_store.context_runs[0]["report_mode"] == "weekly"
    assert scanner.scan_index_store.context_runs[0]["scan_run_id"] == 77
    assert scanner.scan_index_store.context_decisions[0][0] == 123


def test_scheduler_uses_scan_result_run_id_instead_of_latest_scan_run(
    tmp_path: Path,
) -> None:
    """scan_run_id 必须绑定本次 scan result，避免并发 CLI run 读到别人的 latest。"""
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
        scan_run_id=77,
    )

    class RacingStore(StubStore):
        def latest_scan_run_detail(self) -> dict[str, int]:
            return {"run_id": 999}

    store = RacingStore()
    scanner = StubScanner(scan_result, store=store)
    scheduler = _legacy_scheduler(scanner)

    scheduler.build_context(
        ContextScheduleRequest(
            report_mode="weekly",
            source="scan",
            start_date=date(2026, 5, 10),
            end_date=date(2026, 5, 24),
        )
    )

    assert store.context_runs[0]["scan_run_id"] == 77


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
        scan_run_id=77,
    )
    scanner = StubScanner(scan_result)
    scheduler = _legacy_scheduler(scanner)

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="monthly",
            source="scan",
            start_date=date(2026, 5, 1),
            end_date=date(2026, 5, 24),
        )
    )

    decision = scanner.scan_index_store.context_decisions[0][1][0]
    assert decision.action == ACTION_METADATA_ONLY
    assert decision.reason == "file_size_policy"
    assert "sheet preview" not in result.file_context


def test_legacy_adapter_marks_error_context_partial_even_if_count_drifted(
    tmp_path: Path,
) -> None:
    """逐文件错误证据不能因旧汇总计数漂移而伪装成 ok。"""
    failed_file = tmp_path / "broken.txt"
    scan_result = ScanResult(
        total_files=1,
        success_count=1,
        error_count=0,
        contexts=[
            FileContext(
                file_path=str(failed_file),
                file_type=".txt",
                content="",
                error="synthetic parser failure",
                parser_backend="not_parsed",
            )
        ],
        scan_run_id=77,
    )

    result = _legacy_scheduler(StubScanner(scan_result)).build_context(
        ContextScheduleRequest(
            report_mode="daily",
            source="scan",
            start_date=date(2026, 5, 24),
            end_date=date(2026, 5, 24),
        )
    )

    assert result.status == "partial"
    assert [warning.error_code for warning in result.warnings] == [
        "PARSER_FAILED"
    ]
    assert result.summary.success_count == 0
    assert result.summary.error_file_count == 1


def test_legacy_adapter_maps_timeout_and_stage_metrics_from_scan_audit(
    tmp_path: Path,
) -> None:
    """legacy envelope 应把 timeout 与普通错误拆开，并保留 scanner 阶段证据。"""

    class MetricsStore(StubStore):
        def get_scan_run_detail(self, run_id: int) -> dict[str, int]:
            assert run_id == 77
            return {
                "run_id": 77,
                "total_duration_ms": 7,
                "discovery_duration_ms": 2,
                "parse_duration_ms": 4,
                "success_count": 0,
                "error_count": 1,
                "timeout_count": 1,
            }

    scan_result = ScanResult(
        total_files=1,
        success_count=0,
        error_count=1,
        contexts=[
            FileContext(
                file_path=str(tmp_path / "slow.pdf"),
                file_type=".pdf",
                content="",
                error="timeout: synthetic deadline",
                parser_backend="not_parsed",
            )
        ],
        scan_run_id=77,
    )

    result = _legacy_scheduler(
        StubScanner(scan_result, store=MetricsStore())
    ).build_context(
        ContextScheduleRequest(
            report_mode="daily",
            source="scan",
            start_date=date(2026, 5, 24),
            end_date=date(2026, 5, 24),
        )
    )

    assert result.status == "partial"
    assert result.summary.success_count == 0
    assert result.summary.timeout_count == 1
    assert result.summary.error_file_count == 0
    assert result.summary.discovery_duration_ms == 2
    assert result.summary.parse_duration_ms == 4


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
        scan_run_id=77,
    )
    scanner = StubScanner(scan_result)
    scheduler = _legacy_scheduler(
        scanner,
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

    assert result.status == "error"
    assert result.error is not None
    assert result.error.error_code == "INTERNAL_ERROR"
    assert result.file_context == ""
    assert scanner.scan_index_store.context_runs[0]["status"] == "error"
    assert scanner.scan_index_store.context_runs[0]["error"] == (
        "PYTHON_LEGACY_CONTEXT_FAILED"
    )
    assert "secret-value" not in result.error.message
    assert "secret-value" not in str(scanner.scan_index_store.context_runs)


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
        scan_run_id=77,
    )
    store = FailingDecisionStore()
    scanner = StubScanner(scan_result, store=store)
    scheduler = _legacy_scheduler(scanner)

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="weekly",
            source="scan",
            start_date=date(2026, 5, 10),
            end_date=date(2026, 5, 24),
        )
    )

    assert result.status == "error"
    assert result.error is not None
    assert result.file_context == ""
    assert result.context_run_id is None
    assert [warning.message for warning in result.warnings] == [
        "Python legacy audit persistence failed"
    ]
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
        scan_run_id=77,
    )
    store = AtomicSaveFailingStore()
    scanner = StubScanner(scan_result, store=store)
    scheduler = _legacy_scheduler(scanner)

    result = scheduler.build_context(
        ContextScheduleRequest(
            report_mode="weekly",
            source="scan",
            start_date=date(2026, 5, 10),
            end_date=date(2026, 5, 24),
        )
    )

    assert result.status == "error"
    assert result.error is not None
    assert result.file_context == ""
    assert [run["status"] for run in store.context_runs] == ["error"]
    error_run = store.context_runs[0]
    assert error_run["included_file_count"] == 1
    assert error_run["input_chars"] > 0
    assert error_run["output_chars"] > 0

    saved_context_run_id, saved_decisions = store.context_decisions[0]
    assert saved_context_run_id == 123
    assert saved_decisions[0].action == ACTION_KEEP
    assert saved_decisions[0].output_chars > 0
    assert result.summary.included_file_count == 1
