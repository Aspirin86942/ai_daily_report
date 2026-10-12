from pathlib import Path
from threading import Barrier, Lock
from types import SimpleNamespace

import pytest

from scripts.backfill_daily_reports import BackfillConfig, BudgetedTransport, current_entry
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.evidence_daily import generate_model_batches


def test_parallel_batches_preserve_order_and_bound_inflight_work():
    barrier, lock = Barrier(3), Lock()
    active = peak = 0
    prepared = False

    def prepare():
        nonlocal prepared
        prepared = True

    def generate(run_id, index):
        nonlocal active, peak
        assert prepared and run_id == 'run'
        with lock:
            active += 1
            peak = max(peak, active)
        if index < 3:
            barrier.wait(timeout=5)
        with lock:
            active -= 1
        return index * 10

    client = SimpleNamespace(transport=SimpleNamespace(prepare=prepare), generate=generate)
    assert generate_model_batches(client, 'run', list(range(8)), workers=3) == list(range(0, 80, 10))
    assert peak == 3 and active == 0


def test_parallel_failure_waits_for_inflight_audits():
    barrier, lock = Barrier(3), Lock()
    started, finished = [], []

    def generate(run_id, index):
        with lock:
            started.append(index)
        if index < 3:
            barrier.wait(timeout=5)
        if index == 0:
            raise ReportRunError('MODEL_HTTP_401')
        with lock:
            finished.append(index)
        return index

    client = SimpleNamespace(transport=SimpleNamespace(prepare=lambda: None), generate=generate)
    with pytest.raises(ReportRunError, match='MODEL_HTTP_401'):
        generate_model_batches(client, 'run', list(range(8)), workers=3)
    assert 0 in started
    assert sorted(finished) == sorted(index for index in started if index != 0)


def test_backfill_budget_distinguishes_preparation_from_reserved_final_call():
    used, sends = 1, []
    budget = SimpleNamespace(limit=2, used=lambda: used)
    inner = SimpleNamespace(prepare=lambda: None, send=lambda request, timeout: sends.append(request) or {'ok': True})
    from src.services.report_runner.model_calls import LazyModelTransport
    transport = LazyModelTransport(lambda: BudgetedTransport(inner, budget))
    transport.prepare()
    used = 2
    assert transport.send({'last': True}, timeout=1) == {'ok': True}
    with pytest.raises(ReportRunError, match='BACKFILL_CALL_BUDGET_EXHAUSTED'):
        transport.prepare()
    used = 3
    with pytest.raises(ReportRunError, match='BACKFILL_CALL_BUDGET_EXHAUSTED'):
        transport.send({'extra': True}, timeout=1)
    assert sends == [{'last': True}]


def test_existing_backfill_output_is_preserved_and_external_edits_are_detected(tmp_path):
    import sqlite3
    from hashlib import sha256
    db_path = tmp_path / 'reports.sqlite3'
    with sqlite3.connect(db_path) as connection:
        connection.execute('CREATE TABLE daily_reports(date,schema_version,current_run_id,raw_json)')
        connection.execute('CREATE TABLE report_runs(run_id,quality,expected_sha256,status,phase)')
        connection.execute('INSERT INTO daily_reports VALUES (?,?,?,?)', ('2026-04-12',2,'r','{"sources":[],"summary_items":[]}'))
        connection.execute('INSERT INTO report_runs VALUES (?,?,?,?,?)', ('r','no_changes',sha256(b'original').hexdigest(),'succeeded','published'))
    output = tmp_path / '2026-04/2026-04-12.md'
    output.parent.mkdir()
    output.write_bytes(b'original')
    assert current_entry(db_path, tmp_path, '2026-04-12')['status'] == 'complete'
    assert output.read_bytes() == b'original'
    output.write_bytes(b'edited')
    with pytest.raises(ReportRunError, match='BACKFILL_EXISTING_OUTPUT_MODIFIED'):
        current_entry(db_path, tmp_path, '2026-04-12')
    assert output.read_bytes() == b'edited'
def test_backfill_output_override_is_local_and_respects_verified_capacity():
    from src.models.report_settings import ModelRequestSettings
    original = SimpleNamespace(llm_config={'max_tokens': 8192},
        model_request_settings=ModelRequestSettings(context_window_tokens=1000000, verified_max_output_tokens=393216))
    runtime = BackfillConfig(original, None, output_tokens=32768)
    assert runtime.llm_config['max_tokens'] == 32768
    assert original.llm_config['max_tokens'] == 8192
    with pytest.raises(ReportRunError, match='MODEL_OUTPUT_LIMIT_EXCEEDED'):
        BackfillConfig(original, None, output_tokens=393217)
