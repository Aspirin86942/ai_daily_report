"""daily 子命令。"""

from __future__ import annotations

from argparse import Namespace
from datetime import UTC, datetime
import os
from zoneinfo import ZoneInfo

from src.core.config import config
from src.services.report_runner.context import RunContext, parse_report_date
from src.services.report_runner.requests import DailyReportRunRequest

from .common import (
    ConsolePort,
    MarkdownFactory,
    RunnerFactory,
    build_default_report_runner,
    present_report_outcome,
)


def generate_daily_report(
    args: Namespace,
    *,
    console: ConsolePort,
    runner_factory: RunnerFactory | None = None,
    markdown: MarkdownFactory | None = None,
) -> bool:
    """映射 daily 参数并展示 ReportRunner outcome。"""
    console.print("\n[bold green]===== 文件证据日报 =====[/bold green]\n")
    scheduled = getattr(args, "scheduled", False)
    regenerate = getattr(args, "regenerate", False)
    reason = (getattr(args, "reason", None) or "").strip()
    if scheduled and any((getattr(args, "no_save", False), getattr(args, "dry_run", False), regenerate)):
        raise ValueError("--scheduled 与 --no-save、--dry-run、--regenerate 不能同时使用")
    if bool(regenerate) != bool(reason):
        raise ValueError("--regenerate 与非空 --reason 必须同时提供")
    if regenerate and getattr(args, "dry_run", False):
        raise ValueError("--regenerate 与 --dry-run 不能同时使用")
    date_alias = getattr(args, "date", None)
    target_value = getattr(args, "target_date", None)
    if date_alias and target_value:
        raise ValueError("--date 和 --target-date 不能同时使用")
    if date_alias:
        console.print("[yellow]--date 已弃用；日期同时控制扫描与报告，请改用 --target-date。[/yellow]")
    now = datetime.now(UTC)
    settings = config.report_settings
    timezone = getattr(args, "timezone", None) or settings.timezone
    context = RunContext.create(
        now=now,
        scope_id=settings.scope_id,
        timezone=timezone,
        target_date=parse_report_date(target_value or date_alias) if target_value or date_alias else None,
        scheduled=getattr(args, "scheduled", False),
        scheduled_time=config.schedule_settings.local_time,
    )
    if getattr(args, "dry_run", False):
        if getattr(args, "no_save", False):
            raise ValueError("--dry-run 与 --no-save 不能同时使用")
        from src.services.scan_preview import run_preview

        preview = run_preview(context, config)
        console.print(f"本地预检：{context.target_date}，来源 {preview.source_count}，可用 {preview.usable_count}，状态 {preview.quality}。")
        console.print(f"模型调用 0 次；预检清单：{preview.manifest_path}")
        return preview.quality != "unavailable"
    if runner_factory is None:
        from src.services.report_runner.evidence_daily import EvidenceDailyService
        from src.core.logger import setup_logger

        if scheduled:
            key_env = "DEEPSEEK_API_KEY" if config.llm_provider == "deepseek" else "OPENAI_API_KEY"
            setup_logger().info("scheduled_start id=%s date=%s pid=%d credential_env_present=%s",
                                context.run_id, context.target_date, os.getpid(), bool(os.environ.get(key_env)))
        outcome = EvidenceDailyService(config).run(context, user_input=args.input or "",
            save=not args.no_save, scheduled=scheduled, regenerate_reason=reason if regenerate else None)
        console.print(f"日期 {outcome.target_date}；质量 {outcome.quality}；模型调用记录 {outcome.model_calls} 次。")
        if outcome.output_path:
            console.print(f"日报文件：{outcome.output_path}")
        if not scheduled:
            if markdown is None:
                from rich.markdown import Markdown
                markdown = Markdown
            console.print(markdown(outcome.markdown))
        setup_logger().info("report_run id=%s date=%s quality=%s action=%s calls=%d", outcome.run_id,
                            outcome.target_date, outcome.quality, outcome.action, outcome.model_calls)
        return True
    if markdown is None:
        from rich.markdown import Markdown

        markdown = Markdown
    runner = runner_factory() if runner_factory else build_default_report_runner(console)
    outcome = runner.run(
        DailyReportRunRequest(
            as_of_date=now.astimezone(ZoneInfo(timezone)).date(),
            save=not args.no_save,
            user_input=args.input,
            report_date_override=date_alias,
            context=context,
        )
    )
    return present_report_outcome(
        outcome,
        label="日报",
        console=console,
        markdown=markdown,
    )
