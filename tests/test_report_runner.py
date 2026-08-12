"""ReportRunner.run 公共 pipeline 的行为测试。"""

from __future__ import annotations

from datetime import date
from pathlib import Path

from src.models.scanner_contract import ContextSummary, Diagnostic
from src.models.schemas import DailyReportData, MonthlyReportData, WeeklyReportData
from src.models.scanner_contract import ContextEnvelope
from src.services.native_scanner import ScanResult
from src.services.report_runner.outcomes import (
    ErrorCode,
    ReportRunFailure,
    ReportRunSuccess,
)
from src.services.report_runner.model_port import LLMModelPort
from src.services.report_runner.requests import (
    DailyReportRunRequest,
    MonthlyReportRunRequest,
    WeeklyReportRunRequest,
)


class FakeScanner:
    def __init__(self, result: ScanResult) -> None:
        self._result = result
        self.calls: list[object] = []

    def build_context(self, request: object) -> ScanResult:
        self.calls.append(request)
        return self._result


class FakeStore:
    def __init__(self, events: list[str] | None = None) -> None:
        self.events = events if events is not None else []
        self.saved: list[tuple[str, object]] = []

    def get_yesterday_plan(self, target_date=None) -> str:
        self.events.append("read_yesterday_plan")
        return "昨日计划"

    def save_report(self, report: DailyReportData) -> None:
        self.events.append("save_sqlite")
        self.saved.append(("daily", report))

    def save_weekly_report(self, report) -> None:
        self.events.append("save_sqlite")
        self.saved.append(("weekly", report))

    def save_monthly_report(self, report) -> None:
        self.events.append("save_sqlite")
        self.saved.append(("monthly", report))


class FakeRenderer:
    def __init__(self, events: list[str] | None = None) -> None:
        self.events = events if events is not None else []
        self.written: list[Path] = []

    def render_markdown(self, report: DailyReportData) -> str:
        self.events.append("render")
        return f"markdown:{report.date}"

    def render_weekly_markdown(self, report) -> str:
        self.events.append("render")
        return "weekly-md"

    def render_monthly_markdown(self, report) -> str:
        self.events.append("render")
        return "monthly-md"

    def save_markdown(self, content: str, report_date: str) -> Path:
        self.events.append("save_markdown")
        path = Path(f"{report_date}.md")
        self.written.append(path)
        return path

    def save_weekly_markdown(self, content: str, year: int, week: int) -> Path:
        self.events.append("save_markdown")
        path = Path(f"{year}-W{week:02d}.md")
        self.written.append(path)
        return path

    def save_monthly_markdown(self, content: str, year_month: str) -> Path:
        self.events.append("save_markdown")
        path = Path(f"{year_month}.md")
        self.written.append(path)
        return path


class RecordingModelPort:
    def __init__(self, report: DailyReportData | None = None) -> None:
        self._report = report or DailyReportData(
            date="2026-05-25",
            completed_work="c",
            work_summary="w",
            next_plan="n",
        )
        self.calls: list[object] = []

    def generate(self, request: object) -> DailyReportData:
        self.calls.append(request)
        return self._report


class FixedInputAdapter:
    def __init__(self, value: str = "今天工作") -> None:
        self.value = value
        self.calls = 0

    def read(self) -> str:
        self.calls += 1
        return self.value


def _context(status: str = "ok") -> ScanResult:
    diagnostic = Diagnostic(
        error_code="RUST_CORE_CRASHED" if status == "error" else "PARSER_FAILED",
        message="synthetic scanner diagnostic",
        retryable=False,
        stage="process" if status == "error" else "parse",
        file_path=None,
        backend=None,
    )
    envelope = ContextEnvelope(
        contract="ai_daily_context",
        protocol_version=1,
        request_id="11111111-1111-4111-8111-111111111111",
        engine_version="0.1.0",
        engine_build="test-build",
        file_context="ctx" if status != "error" else "",
        status=status,
        summary=ContextSummary(
            source_file_count=1,
            success_count=0 if status == "error" else 1,
            timeout_count=0,
            included_file_count=0 if status == "error" else 1,
            omitted_file_count=0,
            error_file_count=1 if status == "error" else 0,
            input_chars=0,
            output_chars=0,
            total_duration_ms=1,
            discovery_duration_ms=0,
            parse_duration_ms=0,
            compression_duration_ms=0,
        ),
        scan_run_id=None if status == "error" else 1,
        context_run_id=None if status == "error" else 1,
        warnings=[diagnostic] if status == "partial" else [],
        error=diagnostic if status == "error" else None,
    )
    return ScanResult(envelope=envelope, evidence=None)


def _make_runner(**overrides):
    from src.services.report_runner.runner import ReportRunner

    defaults = {
        "scanner": FakeScanner(_context()),
        "store": FakeStore(),
        "renderer": FakeRenderer(),
        "model_port": RecordingModelPort(),
        "daily_input": FixedInputAdapter(),
    }
    defaults.update(overrides)
    return ReportRunner(**defaults)


def test_daily_success_publishes_sqlite_then_markdown():
    events: list[str] = []
    runner = _make_runner(
        store=FakeStore(events),
        renderer=FakeRenderer(events),
    )

    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date(2026, 5, 25),
            save=True,
            user_input="今天工作",
        )
    )

    assert isinstance(outcome, ReportRunSuccess)
    assert outcome.status == "ok"
    assert outcome.markdown == "markdown:2026-05-25"
    assert outcome.publication.sqlite_state == "committed"
    assert outcome.publication.markdown_state == "written"
    assert events == [
        "read_yesterday_plan",
        "render",
        "save_sqlite",
        "save_markdown",
    ]


def test_daily_no_save_skips_publication():
    store = FakeStore()
    renderer = FakeRenderer()
    runner = _make_runner(store=store, renderer=renderer)

    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date(2026, 5, 25),
            save=False,
            user_input="今天工作",
        )
    )

    assert isinstance(outcome, ReportRunSuccess)
    assert outcome.publication.requested is False
    assert outcome.publication.sqlite_state == "not_attempted"
    assert outcome.publication.markdown_state == "not_attempted"
    assert outcome.markdown != ""
    assert store.saved == []
    assert renderer.written == []


def test_daily_scanner_error_fails_before_input_and_llm():
    model_port = RecordingModelPort()
    daily_input = FixedInputAdapter()
    runner = _make_runner(
        scanner=FakeScanner(_context(status="error")),
        model_port=model_port,
        daily_input=daily_input,
    )

    outcome = runner.run(
        DailyReportRunRequest(as_of_date=date(2026, 5, 25), save=False)
    )

    assert isinstance(outcome, ReportRunFailure)
    assert outcome.phase == "source"
    assert outcome.error.error_code is ErrorCode.SCANNER_FAILED
    assert model_port.calls == []
    assert daily_input.calls == 0


def test_daily_empty_input_fails_before_llm():
    model_port = RecordingModelPort()
    runner = _make_runner(
        model_port=model_port,
        daily_input=FixedInputAdapter(value="   "),
    )

    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date(2026, 5, 25),
            save=False,
            user_input=None,
        )
    )

    assert isinstance(outcome, ReportRunFailure)
    assert outcome.error.error_code is ErrorCode.EMPTY_DAILY_INPUT
    assert model_port.calls == []


def test_daily_full_path_writes_real_sqlite_and_markdown(tmp_path):
    from src.services.report_gen import ReportGenerator
    from src.services.sqlite_store import SQLiteStore

    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    store.save_report(
        DailyReportData(
            date="2026-05-24",
            completed_work="昨日工作",
            work_summary="昨日小结",
            next_plan="昨日延续计划",
        )
    )
    renderer = ReportGenerator(reports_dir=tmp_path / "reports")

    class RecordingClient:
        def __init__(self) -> None:
            self.calls = 0

        def generate_report(
            self, *, user_input: str, file_context: str, yesterday_plan: str
        ) -> DailyReportData:
            self.calls += 1
            assert user_input == "今天工作"
            assert file_context == "ctx"
            assert yesterday_plan == "昨日延续计划"
            return DailyReportData(
                date="2026-05-25",
                completed_work="完成日报",
                work_summary="日报摘要",
                next_plan="后续计划",
            )

    client = RecordingClient()
    runner = _make_runner(
        store=store,
        renderer=renderer,
        model_port=LLMModelPort(client_factory=lambda: client),
    )

    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date(2026, 5, 25),
            save=True,
            user_input="今天工作",
        )
    )

    assert isinstance(outcome, ReportRunSuccess)
    assert store.get_report("2026-05-25") is not None
    markdown_path = tmp_path / "reports" / "2026-05" / "2026-05-25.md"
    assert markdown_path.is_file()
    assert markdown_path.read_text(encoding="utf-8") == outcome.markdown
    assert outcome.publication.sqlite_state == "committed"
    assert outcome.publication.markdown_state == "written"
    assert outcome.publication.markdown_path == markdown_path
    assert client.calls == 1


def test_daily_date_override_keeps_scan_window(tmp_path):
    from src.services.report_gen import ReportGenerator
    from src.services.sqlite_store import SQLiteStore

    store = SQLiteStore(db_path=tmp_path / "reports.sqlite3")
    renderer = ReportGenerator(reports_dir=tmp_path / "reports")

    class RecordingClient:
        def generate_report(
            self, *, user_input: str, file_context: str, yesterday_plan: str
        ) -> DailyReportData:
            return DailyReportData(
                date="2026-05-25",
                completed_work="c",
                work_summary="w",
                next_plan="n",
            )

    scanner = FakeScanner(_context())
    runner = _make_runner(
        scanner=scanner,
        store=store,
        renderer=renderer,
        model_port=LLMModelPort(client_factory=RecordingClient),
    )

    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date(2026, 5, 25),
            save=True,
            user_input="x",
            report_date_override="2026-05-20",
        )
    )

    assert isinstance(outcome, ReportRunSuccess)
    schedule = scanner.calls[0]
    assert schedule.start_date == date(2026, 5, 24)
    assert schedule.end_date == date(2026, 5, 25)
    assert outcome.report.date == "2026-05-20"
    assert store.get_report("2026-05-20") is not None
    assert (tmp_path / "reports" / "2026-05" / "2026-05-20.md").is_file()


def test_daily_scanner_error_does_not_construct_llm_client():
    factory_calls = 0

    def client_factory():
        nonlocal factory_calls
        factory_calls += 1
        raise AssertionError("scanner error 后不得构造 LLM client")

    runner = _make_runner(
        scanner=FakeScanner(_context(status="error")),
        model_port=LLMModelPort(client_factory=client_factory),
    )

    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=date(2026, 5, 25),
            save=False,
            user_input="x",
        )
    )

    assert isinstance(outcome, ReportRunFailure)
    assert outcome.error.error_code is ErrorCode.SCANNER_FAILED
    assert factory_calls == 0


def test_weekly_db_zero_scanner_calls_and_publishes(tmp_path):
    from src.services.report_gen import ReportGenerator
    from src.services.sqlite_store import SQLiteStore

    store = SQLiteStore(db_path=tmp_path / "weekly.sqlite3")
    store.save_report(
        DailyReportData(
            date="2026-05-11",
            completed_work="c",
            work_summary="w",
            next_plan="n",
        )
    )
    renderer = ReportGenerator(reports_dir=tmp_path / "reports")

    class RecordingClient:
        def generate_weekly_report(
            self,
            *,
            reports,
            file_context: str,
            year: int,
            week: int,
            missing_days: list[str],
            data_source: str,
        ) -> WeeklyReportData:
            assert [report.date for report in reports] == ["2026-05-11"]
            assert file_context == "无文件证据"
            assert missing_days == [
                "2026-05-12",
                "2026-05-13",
                "2026-05-14",
                "2026-05-15",
            ]
            assert data_source == "db"
            return WeeklyReportData(
                week_label=f"{year}-W{week:02d}",
                date_range="2026-05-11 ~ 2026-05-17",
                completed_work="cw",
                self_growth="",
                improvement_actions="",
                work_summary="",
                next_plan="",
                support_needed="",
                other_notes="",
            )

    scanner = FakeScanner(_context())
    runner = _make_runner(
        scanner=scanner,
        store=store,
        renderer=renderer,
        model_port=LLMModelPort(client_factory=RecordingClient),
    )

    outcome = runner.run(
        WeeklyReportRunRequest(
            as_of_date=date(2026, 5, 18),
            source="db",
            save=True,
            week_label="2026-W20",
        )
    )

    assert scanner.calls == []
    assert isinstance(outcome, ReportRunSuccess)
    assert outcome.source_evidence.report_count == 1
    assert outcome.publication.markdown_state == "written"
    assert (tmp_path / "reports" / "weekly" / "2026-W20.md").is_file()


def test_weekly_scan_calls_scanner_once_and_appends_supplement(tmp_path):
    from src.services.report_gen import ReportGenerator

    renderer = ReportGenerator(reports_dir=tmp_path / "reports")
    scanner = FakeScanner(_context())

    class RecordingClient:
        def generate_weekly_report(
            self,
            *,
            reports,
            file_context: str,
            year: int,
            week: int,
            missing_days: list[str],
            data_source: str,
        ) -> WeeklyReportData:
            assert reports == []
            assert missing_days == []
            assert file_context == "ctx\n\n---\n\n用户补充: 补丁"
            assert data_source == "scan"
            return WeeklyReportData(
                week_label=f"{year}-W{week:02d}",
                date_range="2026-05-11 ~ 2026-05-17",
                completed_work="cw",
                self_growth="",
                improvement_actions="",
                work_summary="",
                next_plan="",
                support_needed="",
                other_notes="",
            )

    runner = _make_runner(
        scanner=scanner,
        renderer=renderer,
        model_port=LLMModelPort(client_factory=RecordingClient),
    )

    outcome = runner.run(
        WeeklyReportRunRequest(
            as_of_date=date(2026, 5, 18),
            source="scan",
            save=False,
            week_label="2026-W20",
            supplemental_input="补丁",
        )
    )

    assert isinstance(outcome, ReportRunSuccess)
    assert len(scanner.calls) == 1
    schedule = scanner.calls[0]
    assert schedule.start_date == date(2026, 5, 11)
    assert schedule.end_date == date(2026, 5, 17)


def test_weekly_db_no_reports_fails_before_llm(tmp_path):
    from src.services.sqlite_store import SQLiteStore

    store = SQLiteStore(db_path=tmp_path / "empty.sqlite3")
    factory_calls = 0

    def client_factory():
        nonlocal factory_calls
        factory_calls += 1
        raise AssertionError("无 DB 报告时不得构造 LLM client")

    runner = _make_runner(
        store=store,
        model_port=LLMModelPort(client_factory=client_factory),
    )

    outcome = runner.run(
        WeeklyReportRunRequest(
            as_of_date=date(2026, 5, 18),
            source="db",
            save=False,
            week_label="2026-W20",
        )
    )

    assert isinstance(outcome, ReportRunFailure)
    assert outcome.error.error_code is ErrorCode.NO_SOURCE_REPORTS
    assert outcome.source == "db"
    assert factory_calls == 0


def test_monthly_db_zero_scanner_calls(tmp_path):
    from src.services.sqlite_store import SQLiteStore

    store = SQLiteStore(db_path=tmp_path / "monthly.sqlite3")
    store.save_report(
        DailyReportData(
            date="2026-05-05",
            completed_work="c",
            work_summary="w",
            next_plan="n",
        )
    )
    scanner = FakeScanner(_context())

    class RecordingClient:
        def generate_monthly_report(
            self,
            *,
            reports,
            file_context: str,
            year_month: str,
            missing_days: list[str],
            data_source: str,
        ) -> MonthlyReportData:
            assert [report.date for report in reports] == ["2026-05-05"]
            assert file_context == "无文件证据"
            assert year_month == "2026-05"
            assert missing_days == sorted(missing_days)
            assert "2026-05-04" in missing_days
            assert "2026-05-31" not in missing_days
            assert data_source == "db"
            return MonthlyReportData(
                year_month=year_month,
                overview="ov",
                completed_work="cw",
                work_summary="",
                next_plan="",
            )

    runner = _make_runner(
        scanner=scanner,
        store=store,
        model_port=LLMModelPort(client_factory=RecordingClient),
    )

    outcome = runner.run(
        MonthlyReportRunRequest(
            as_of_date=date(2026, 5, 20),
            source="db",
            save=False,
            year_month="2026-05",
        )
    )

    assert scanner.calls == []
    assert isinstance(outcome, ReportRunSuccess)
    assert outcome.period.start_date == date(2026, 5, 1)
    assert outcome.period.end_date == date(2026, 5, 31)


def test_monthly_scan_calls_scanner_once(tmp_path):
    from src.services.report_gen import ReportGenerator

    renderer = ReportGenerator(reports_dir=tmp_path / "reports")
    scanner = FakeScanner(_context())

    class RecordingClient:
        def generate_monthly_report(
            self,
            *,
            reports,
            file_context: str,
            year_month: str,
            missing_days: list[str],
            data_source: str,
        ) -> MonthlyReportData:
            assert reports == []
            assert missing_days == []
            assert file_context == "ctx\n\n---\n\n用户补充: 月补充"
            assert data_source == "scan"
            return MonthlyReportData(
                year_month=year_month,
                overview="ov",
                completed_work="cw",
                work_summary="",
                next_plan="",
            )

    runner = _make_runner(
        scanner=scanner,
        renderer=renderer,
        model_port=LLMModelPort(client_factory=RecordingClient),
    )

    outcome = runner.run(
        MonthlyReportRunRequest(
            as_of_date=date(2026, 5, 20),
            source="scan",
            save=False,
            year_month="2026-05",
            supplemental_input="月补充",
        )
    )

    assert isinstance(outcome, ReportRunSuccess)
    assert len(scanner.calls) == 1
    schedule = scanner.calls[0]
    assert schedule.start_date == date(2026, 5, 1)
    assert schedule.end_date == date(2026, 5, 31)
