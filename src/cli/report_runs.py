"""Inspect metadata and repair exact persisted output without scanning or a model."""

from pathlib import Path
import sqlite3

from src.core.config import config
from src.services.report_runner.context import parse_report_date
from src.services.report_runner.evidence_daily import EvidenceDailyService


def repair_output(args, *, console) -> bool:
    target = parse_report_date(args.target_date).isoformat()
    result = EvidenceDailyService(config).repair(target)
    console.print(f"输出已核验：{result.output_path}；扫描 0 次，模型调用 0 次。")
    return True


def list_runs(args, *, console) -> None:
    target = parse_report_date(args.target_date).isoformat() if args.target_date else None
    path = Path(config.db_dir) / "reports.sqlite3"
    if not path.exists():
        console.print("暂无运行记录。")
        return
    connection = sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    try:
        if not connection.execute("SELECT 1 FROM sqlite_master WHERE name='report_runs'").fetchone():
            console.print("报告库尚未启用运行记录。")
            return
        rows = connection.execute("""SELECT r.run_id,r.target_date,r.run_kind,r.phase,r.status,r.quality,r.error_code,r.output_path,
            (SELECT COUNT(*) FROM report_model_calls c WHERE c.run_id=r.run_id) AS calls
            FROM report_runs r WHERE (? IS NULL OR r.target_date=?) ORDER BY r.created_at DESC,r.run_id LIMIT ?""",
            (target, target, min(max(args.limit, 1), 500))).fetchall()
        for row in rows:
            console.print(f"{row['target_date']} {row['run_id']} {row['run_kind']} 阶段={row['phase']} "
                f"状态={row['status']} 质量={row['quality'] or '-'} 调用={row['calls']} 错误={row['error_code'] or '-'}",
                markup=False)
            if row["output_path"]:
                console.print(f"  产物：{row['output_path']}", markup=False)
        if not rows:
            console.print("暂无匹配的运行记录。")
    finally:
        connection.close()
