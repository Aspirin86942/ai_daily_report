"""Backfill a reviewed date range with a resumable ledger and a job-wide call cap."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
from dataclasses import replace
from datetime import UTC, date, datetime, timedelta
from hashlib import sha256
import json
import os
from pathlib import Path
import re
import sqlite3
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from src.core.config import config
from src.models.scan_sources import ScanSource
from src.services.native_scanner import NativeScanner
from src.services.report_runner.context import RunContext, parse_report_date
from src.services.report_runner.errors import ReportRunError
from src.services.report_runner.evidence_daily import EvidenceDailyService
from src.services.report_runner.locking import ReportLock
from src.services.report_runner.model_plan import check_capabilities
from src.services.report_runner.snapshots import is_redirect
from src.services.scan_preview import ReportScannerConfig


def write_json(path: Path, value: dict) -> None:
    temporary = path.with_suffix('.json.tmp')
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n', encoding='utf-8', newline='\n')
    os.replace(temporary, path)


@contextmanager
def read_database(path: Path):
    connection = sqlite3.connect(path.resolve().as_uri() + '?mode=ro', uri=True)
    connection.row_factory = sqlite3.Row
    try:
        yield connection
    finally:
        connection.close()


class JobBudget:
    def __init__(self, db_path: Path, baseline_call_id: int, limit: int):
        self.db_path, self.baseline_call_id, self.limit = db_path, baseline_call_id, limit

    def used(self) -> int:
        with read_database(self.db_path) as connection:
            return connection.execute('SELECT count(*) FROM report_model_calls WHERE call_id>?', (self.baseline_call_id,)).fetchone()[0]


class BudgetedTransport:
    def __init__(self, inner, budget: JobBudget):
        self.inner, self.budget = inner, budget

    def prepare(self):
        if self.budget.used() >= self.budget.limit:
            raise ReportRunError('BACKFILL_CALL_BUDGET_EXHAUSTED')
        self.inner.prepare()

    def send(self, request: dict, *, timeout: float) -> dict:
        # The reservation has already been committed; exactly the final allowed call may proceed.
        if self.budget.used() > self.budget.limit:
            raise ReportRunError('BACKFILL_CALL_BUDGET_EXHAUSTED')
        return self.inner.send(request, timeout=timeout)


class BackfillConfig:
    def __init__(self, original, budget: JobBudget, *, output_tokens: int | None = None):
        self.original, self.budget = original, budget
        self.output_tokens = output_tokens
        if output_tokens is not None:
            if output_tokens < 1:
                raise ValueError('output tokens must be positive')
            check_capabilities(original.model_request_settings, message_bytes=0, max_tokens=output_tokens)

    def __getattr__(self, name):
        return getattr(self.original, name)

    @property
    def llm_config(self):
        values = dict(self.original.llm_config)
        if self.output_tokens is not None:
            values['max_tokens'] = self.output_tokens
        return values

    @property
    def recovery_settings(self):
        from zoneinfo import ZoneInfo
        day = datetime.now(ZoneInfo('Asia/Shanghai')).date().isoformat()
        with read_database(self.budget.db_path) as connection:
            used_today = connection.execute('SELECT count(*) FROM report_model_calls WHERE call_date=?', (day,)).fetchone()[0]
        remaining = max(0, self.budget.limit - self.budget.used())
        original = self.original.recovery_settings
        return original.model_copy(update={
            'max_calls_per_calendar_day': max(original.max_calls_per_run, used_today + remaining),
        })


class BackfillService(EvidenceDailyService):
    def __init__(self, runtime_config, *, budget: JobBudget, **kwargs):
        super().__init__(runtime_config, **kwargs)
        self.budget = budget

    def _transport(self):
        return BudgetedTransport(super()._transport(), self.budget)


class PdfOcrScanner:
    """Overlay locally recognized PDF text only when its original content hash still matches."""

    def __init__(self, inner, manifest: Path | None, *, work_root: Path | None = None):
        self.inner = inner
        self.work_root = Path(work_root or config.work_dir)
        self.documents = {}
        if manifest is not None:
            for document in json.loads(manifest.read_text(encoding='utf-8-sig')):
                parts = []
                for page in document['pages']:
                    data = Path(page['text']).read_bytes()
                    if page.get('text_sha256') and sha256(data).hexdigest() != page['text_sha256']:
                        raise ReportRunError('BACKFILL_OCR_TEXT_CHANGED')
                    text = data.decode('utf-8-sig')
                    text = re.sub(r'(?<=[\u4e00-\u9fff])[ \t]+(?=[\u4e00-\u9fff])', '', text)
                    parts.append(f"[第 {page['page']} 页]\n{text}")
                full_text = '\n\n'.join(parts)
                if not any(Path(page['text']).read_text(encoding='utf-8-sig').strip() for page in document['pages']):
                    raise ReportRunError('BACKFILL_OCR_EMPTY')
                self.documents[document['source_id']] = {**document, 'full_text': full_text}

    def build_context(self, request):
        result = self.inner.build_context(request)
        if result.envelope.status == 'error':
            return result
        sources = []
        for source in result.sources:
            supplement = self.documents.get(source.source_id)
            if supplement is None or source.excerpt:
                sources.append(source)
                continue
            digest = source.content_sha256
            if digest is None:
                path = self.work_root / source.relative_path
                try:
                    if any(is_redirect(component) for component in (path, *path.parents)):
                        raise ReportRunError('BACKFILL_OCR_SOURCE_CHANGED')
                    before = path.stat()
                    digest = sha256(path.read_bytes()).hexdigest()
                    after = path.stat()
                except OSError:
                    raise ReportRunError('BACKFILL_OCR_SOURCE_CHANGED') from None
                identity = lambda stat: (stat.st_dev, stat.st_ino, stat.st_size, stat.st_mtime_ns, stat.st_ctime_ns)
                if before.st_size != source.size_bytes or identity(before) != identity(after):
                    raise ReportRunError('BACKFILL_OCR_SOURCE_CHANGED')
            if digest != supplement['content_sha256']:
                raise ReportRunError('BACKFILL_OCR_SOURCE_CHANGED')
            full_text = supplement['full_text']
            excerpt = full_text[:6000]
            limits = {**source.coverage_limits, 'ocr': {'engine': 'windows_ocr', 'language': 'zh-Hans-CN',
                      'pages_read': len(supplement['pages']), 'pages_total': supplement['pages_total'], 'text_unverified': True}}
            sources.append(ScanSource.model_validate({**source.model_dump(),
                'excerpt': excerpt, 'excerpt_sha256': sha256(excerpt.encode('utf-8')).hexdigest(),
                'content_sha256': digest,
                'parser_backend': 'windows_ocr_v1', 'worker_lane': 'local_ocr_preflight',
                'parse_status': 'success', 'parse_truncated': len(supplement['pages']) < supplement['pages_total'],
                'excerpt_truncated': len(full_text) > len(excerpt), 'coverage_limits': limits,
                'reason': 'local_ocr_text_requires_verification',
            }))
        return replace(result, sources=tuple(sources))


def current_entry(db_path: Path, reports_dir: Path, target: str) -> dict | None:
    with read_database(db_path) as connection:
        row = connection.execute('''SELECT d.date,d.schema_version,d.current_run_id,d.raw_json,
            r.quality,r.expected_sha256,r.status,r.phase FROM daily_reports d
            LEFT JOIN report_runs r ON d.current_run_id=r.run_id WHERE d.date=?''', (target,)).fetchone()
    if row is None:
        return None
    path = reports_dir / target[:7] / f'{target}.md'
    if not path.is_file():
        raise ReportRunError('BACKFILL_EXISTING_OUTPUT_MISSING')
    digest = sha256(path.read_bytes()).hexdigest()
    if row['expected_sha256'] and digest != row['expected_sha256']:
        raise ReportRunError('BACKFILL_EXISTING_OUTPUT_MODIFIED')
    if row['current_run_id'] and (row['status'] != 'succeeded' or row['phase'] != 'published'):
        raise ReportRunError('BACKFILL_EXISTING_PUBLICATION_INCOMPLETE')
    report = json.loads(row['raw_json'])
    return {'date': target, 'status': 'complete', 'quality': row['quality'] or 'legacy',
            'run_id': row['current_run_id'], 'output_path': str(path), 'sha256': digest,
            'sources': len(report.get('sources', [])), 'summary_items': len(report.get('summary_items', []))}


def write_index(state: dict, reports_dir: Path) -> None:
    rows = state['dates']
    complete = sum(row.get('status') == 'complete' for row in rows.values())
    labels = {'no_changes': '未发现该日期的修改文件', 'partial': '部分覆盖', 'complete': '完整覆盖', 'legacy': '保留既有日报'}
    lines = [f"# 历史日报补录：{state['start_date']} 至 {state['end_date']}", '',
             f"覆盖 {state['total_dates']} 天，已保存并核验 {complete} 天。模型调用记录 {state['calls_used']} / {state['call_budget']} 次。", '',
             '历史补录依据当前文件的最后修改时间和现有内容，无法还原已被覆盖的旧版本。空日期记录只说明没有发现符合规则的文件。扫描件文字识别结果会单独标明，数字与表格对应关系需要核对原件。', '',
             '| 日期 | 状态 | 来源数 | 日报 |', '| --- | --- | ---: | --- |']
    start = date.fromisoformat(state['start_date'])
    for index in range(state['total_dates']):
        target = str(start + timedelta(days=index))
        row = rows.get(target, {})
        if row.get('status') == 'complete':
            label = labels.get(row['quality'], row['quality'])
            link = f"[打开]({target[:7]}/{target}.md)"
        else:
            label = row.get('error_code') or row.get('status', '待补录')
            link = '—'
        lines.append(f"| {target} | {label} | {row.get('sources', '—')} | {link} |")
    path = reports_dir / f"历史日报补录_{state['start_date']}_{state['end_date']}.md"
    temporary = path.with_suffix('.md.tmp')
    temporary.write_text('\n'.join(lines)+'\n', encoding='utf-8', newline='\n')
    os.replace(temporary, path)


def run(args) -> dict:
    start, end = parse_report_date(args.start_date), parse_report_date(args.end_date)
    RunContext.create(now=datetime.now(UTC), target_date=end)
    if start > end or args.call_budget < 0:
        raise ValueError('invalid backfill range or budget')
    directory = Path(config.data_dir) / 'backfill' / f'{start}_{end}'
    directory.mkdir(parents=True, exist_ok=True)
    state_path = directory / 'state.json'
    db_path, reports_dir = Path(config.db_dir) / 'reports.sqlite3', Path(config.reports_dir)
    if state_path.exists():
        state = json.loads(state_path.read_text(encoding='utf-8'))
        if state['call_budget'] != args.call_budget:
            raise ValueError('resume must retain the approved call budget')
    else:
        with ReportLock(db_path):
            with read_database(db_path) as connection:
                baseline = connection.execute('SELECT COALESCE(max(call_id),0) FROM report_model_calls').fetchone()[0]
                backup = directory / 'reports-before-backfill.sqlite3'
                if backup.exists():
                    raise ReportRunError('BACKFILL_BACKUP_ALREADY_EXISTS')
                with sqlite3.connect(backup) as target:
                    connection.backup(target)
        state = {'start_date': str(start), 'end_date': str(end), 'total_dates': (end-start).days+1,
                 'call_budget': args.call_budget, 'baseline_call_id': baseline, 'dates': {}, 'calls_used': 0,
                 'created_at': datetime.now(UTC).isoformat(), 'backup_sha256': sha256(backup.read_bytes()).hexdigest()}
        write_json(state_path, state)
    budget = JobBudget(db_path, state['baseline_call_id'], state['call_budget'])
    state.pop('stop_reason', None)
    runtime = BackfillConfig(config, budget, output_tokens=args.output_tokens)
    scanner = PdfOcrScanner(NativeScanner(ReportScannerConfig(runtime)), args.ocr_manifest,
                            work_root=Path(runtime.work_dir))
    service = BackfillService(runtime, budget=budget, scanner=scanner, model_workers=args.workers)
    preflight = json.loads(args.preflight.read_text(encoding='utf-8'))
    if preflight['date_start'] != str(start) or preflight['date_end'] != str(end) or preflight['completed_dates'] != state['total_dates']:
        raise ValueError('preflight does not cover the selected range')
    order = sorted(preflight['dates'], key=lambda row: (row['status'] not in {'no_changes','existing'}, row['date']))
    for planned in order:
        target = planned['date']
        if args.only_failed and state['dates'].get(target, {}).get('status') != 'failed':
            continue
        if args.only_no_model and planned['status'] not in {'no_changes','existing'}:
            continue
        try:
            entry = current_entry(db_path, reports_dir, target)
            if entry is not None:
                state['dates'][target] = {**entry, 'action': state['dates'].get(target, {}).get('action', 'preserved')}
            else:
                context = RunContext.create(now=datetime.now(UTC), target_date=date.fromisoformat(target))
                reason = '历史补录失败日期恢复：修复扫描一致性并调整本次输出容量。' if args.only_failed else None
                outcome = service.run(context, regenerate_reason=reason)
                entry = current_entry(db_path, reports_dir, target)
                if entry is None:
                    raise ReportRunError('BACKFILL_PUBLICATION_MISSING')
                state['dates'][target] = {**entry, 'action': outcome.action, 'run_model_calls': outcome.model_calls}
        except ReportRunError as error:
            state['dates'][target] = {'date': target, 'status': 'failed', 'error_code': error.code}
            if error.code in {'BACKFILL_CALL_BUDGET_EXHAUSTED', 'MODEL_DAY_BUDGET_EXHAUSTED', 'MODEL_HTTP_401', 'MODEL_HTTP_403', 'MODEL_CREDENTIAL_MISSING'}:
                state['stop_reason'] = error.code
        state['calls_used'] = budget.used()
        state['updated_at'] = datetime.now(UTC).isoformat()
        write_json(state_path, state)
        write_index(state, reports_dir)
        print(json.dumps({'date': target, 'status': state['dates'][target]['status'],
                          'quality': state['dates'][target].get('quality'),
                          'completed': sum(row['status']=='complete' for row in state['dates'].values()),
                          'total': state['total_dates'], 'calls_used': state['calls_used'],
                          'error_code': state['dates'][target].get('error_code')}, ensure_ascii=False), flush=True)
        if state.get('stop_reason'):
            break
    return state


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--start-date', required=True)
    parser.add_argument('--end-date', required=True)
    parser.add_argument('--preflight', required=True, type=Path)
    parser.add_argument('--call-budget', type=int, required=True)
    parser.add_argument('--workers', type=int, choices=range(1,5), default=1)
    parser.add_argument('--ocr-manifest', type=Path)
    parser.add_argument('--only-no-model', action='store_true')
    parser.add_argument('--only-failed', action='store_true')
    parser.add_argument('--output-tokens', type=int)
    args = parser.parse_args()
    if args.only_no_model:
        os.environ['AI_DAILY_TEST_FORBID_LLM'] = '1'
    state = run(args)
    complete = sum(row['status']=='complete' for row in state['dates'].values())
    return 0 if args.only_no_model or complete == state['total_dates'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
