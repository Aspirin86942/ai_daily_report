"""确定性文件证据上下文压缩服务。"""

from __future__ import annotations

from dataclasses import dataclass, replace
from typing import Any

from ..models.schemas import FileContext, ScanResult

ACTION_KEEP = "keep"
ACTION_COMPRESS = "compress"
ACTION_METADATA_ONLY = "metadata_only"
ACTION_OMIT = "omit"
ACTION_ERROR = "error"

_GLOBAL_BUDGET_REASON = "global_budget_exceeded"
_WARNING_PARSE_ISSUE_BUDGET = "预算不足，解析问题明细未展开。"
_WARNING_FINAL_BUDGET = "预算不足，已压缩最终上下文到全局预算。"
_WARNING_OMITTED_SUMMARY_BUDGET = "预算不足，省略文件摘要已截断。"


@dataclass(frozen=True, slots=True)
class ContextProfile:
    """定义上下文压缩 profile，字段名保持 scheduler 计划合同稳定。"""

    report_mode: str
    compression_profile: str
    global_context_max_chars: int
    per_file_max_chars: int
    small_file_max_bytes: int = 64 * 1024
    medium_file_max_bytes: int = 1024 * 1024
    large_file_max_bytes: int = 10 * 1024 * 1024
    version: str = "context_scheduler_v1"
    priority_policy: str = "default_v1"
    compression_policy: str = "markdown_context_v1"

    @classmethod
    def for_report_mode(cls, report_mode: str) -> "ContextProfile":
        """按报告模式生成固定默认预算，避免后续任务重复散落常量。"""
        normalized_mode = report_mode.strip().lower()
        defaults = {
            "daily": ("daily_balanced_v1", 50_000, 8_000),
            "weekly": ("weekly_balanced_v1", 50_000, 5_000),
            "monthly": ("monthly_balanced_v1", 60_000, 4_000),
        }
        if normalized_mode not in defaults:
            raise ValueError(f"unsupported report_mode: {report_mode!r}")

        compression_profile, global_budget, per_file_budget = defaults[normalized_mode]
        return cls(
            report_mode=normalized_mode,
            compression_profile=compression_profile,
            global_context_max_chars=global_budget,
            per_file_max_chars=per_file_budget,
        )

    def with_budget(
        self,
        global_context_max_chars: int,
        per_file_max_chars: int,
    ) -> "ContextProfile":
        """返回预算覆盖后的新 profile；非正数回退到当前预算，避免无效预算传播。"""
        return replace(
            self,
            global_context_max_chars=_positive_int(
                global_context_max_chars,
                self.global_context_max_chars,
            ),
            per_file_max_chars=_positive_int(
                per_file_max_chars,
                self.per_file_max_chars,
            ),
        )

    def to_profile_dict(self) -> dict[str, Any]:
        """输出完整可序列化 profile，供日志、cache key 和审计摘要复用。"""
        return {
            "version": self.version,
            "report_mode": self.report_mode,
            "compression_profile": self.compression_profile,
            "global_context_max_chars": self.global_context_max_chars,
            "per_file_max_chars": self.per_file_max_chars,
            "small_file_max_bytes": self.small_file_max_bytes,
            "medium_file_max_bytes": self.medium_file_max_bytes,
            "large_file_max_bytes": self.large_file_max_bytes,
            "priority_policy": self.priority_policy,
            "compression_policy": self.compression_policy,
        }


@dataclass(slots=True)
class ContextDecision:
    """单个文件在上下文压缩阶段的可审计决策。"""

    file_path: str
    extension: str
    size_bytes: int
    parser_backend: str | None
    worker_lane: str
    cache_status: str
    action: str
    reason: str
    priority: int
    input_chars: int
    output_chars: int
    truncated: bool
    error: str | None


@dataclass(slots=True)
class CompressedContext:
    """压缩后的上下文正文和统计结果，字段保持计划合同稳定。"""

    content: str
    source_file_count: int
    included_file_count: int
    omitted_file_count: int
    metadata_only_count: int
    compressed_file_count: int
    error_file_count: int
    truncated_file_count: int
    input_chars: int
    output_chars: int
    warnings: list[str]
    decisions: list[ContextDecision]

    @classmethod
    def empty(cls, error: str | None = None) -> "CompressedContext":
        """构造无文件证据时仍可审计的上下文，不依赖外部 profile。"""
        warning_lines = [f"- {error}"] if error else ["- 未发现解析问题。"]
        content = _join_sections(
            [
                "# 文件证据上下文",
                "## 本轮摘要\n- 扫描文件数: 0\n- 纳入文件数: 0\n- 省略文件数: 0",
                "## 重要提示\n- 无文件证据：本轮扫描未提供可用于报告生成的文件内容。",
                "## 文件证据\n无文件证据",
                "## 解析问题\n" + "\n".join(warning_lines),
            ]
        )
        warnings = [error] if error else []
        return cls(
            content=content,
            source_file_count=0,
            included_file_count=0,
            omitted_file_count=0,
            metadata_only_count=0,
            compressed_file_count=0,
            error_file_count=1 if error else 0,
            truncated_file_count=0,
            input_chars=0,
            output_chars=len(content),
            warnings=warnings,
            decisions=[],
        )

    def to_summary(self) -> dict[str, Any]:
        """输出轻量摘要，避免调用方解析 Markdown 正文拿统计。"""
        return {
            "source_file_count": self.source_file_count,
            "included_file_count": self.included_file_count,
            "omitted_file_count": self.omitted_file_count,
            "metadata_only_count": self.metadata_only_count,
            "compressed_file_count": self.compressed_file_count,
            "error_file_count": self.error_file_count,
            "truncated_file_count": self.truncated_file_count,
            "input_chars": self.input_chars,
            "output_chars": self.output_chars,
            "compression_ratio": self._compression_ratio(),
            "warnings": list(self.warnings),
        }

    def _compression_ratio(self) -> float:
        if self.input_chars <= 0:
            return 0.0
        return self.output_chars / self.input_chars


@dataclass(slots=True)
class _CompressionStats:
    source_file_count: int
    included_file_count: int = 0
    omitted_file_count: int = 0
    metadata_only_count: int = 0
    compressed_file_count: int = 0
    error_file_count: int = 0
    input_chars: int = 0


class ContextCompressor:
    """把 scanner 输出压缩为可直接喂给报告生成的确定性上下文。"""

    def compress(
        self,
        *,
        scan_result: ScanResult,
        decisions: list[ContextDecision],
        profile: ContextProfile,
    ) -> CompressedContext:
        """按 decision 顺序和预算渲染 Markdown-like 文件证据上下文。"""
        if not scan_result.contexts and not decisions:
            return CompressedContext.empty()

        # ContextCompressor 是纯压缩层，不能把全局预算或一致性修正泄漏回 scheduler/store。
        internal_decisions = [replace(decision) for decision in decisions]
        context_by_path = {context.file_path: context for context in scan_result.contexts}
        processed_paths: set[str] = set()
        ordered_decisions: list[ContextDecision] = []
        omitted_decisions: list[ContextDecision] = []
        parse_issue_lines: list[str] = []
        warnings: list[str] = []
        stats = _CompressionStats(source_file_count=scan_result.total_files)

        sections = [
            "# 文件证据上下文",
            self._render_run_summary(scan_result, profile),
            self._render_notice(profile),
            "## 文件证据",
        ]

        for decision in internal_decisions:
            context = context_by_path.get(decision.file_path)
            ordered_decisions.append(decision)
            if context is None:
                self._process_decision_without_context(
                    decision,
                    parse_issue_lines,
                    warnings,
                    stats,
                )
                continue

            processed_paths.add(context.file_path)
            self._process_context_decision(
                sections=sections,
                context=context,
                decision=decision,
                profile=profile,
                omitted_decisions=omitted_decisions,
                parse_issue_lines=parse_issue_lines,
                stats=stats,
            )

        for context in scan_result.contexts:
            if context.file_path in processed_paths:
                continue
            decision = self._default_decision(context)
            ordered_decisions.append(decision)
            self._process_context_decision(
                sections=sections,
                context=context,
                decision=decision,
                profile=profile,
                omitted_decisions=omitted_decisions,
                parse_issue_lines=parse_issue_lines,
                stats=stats,
            )

        if stats.included_file_count == 0:
            sections.append("无文件证据")

        if omitted_decisions:
            self._append_omitted_summary(
                sections,
                omitted_decisions,
                profile,
                warnings,
            )

        self._append_parse_issues(sections, parse_issue_lines, profile, warnings)
        content = self._fit_content_to_budget(sections, profile, warnings)
        truncated_file_count = self._count_truncated_files(ordered_decisions)

        return CompressedContext(
            content=content,
            source_file_count=stats.source_file_count,
            included_file_count=stats.included_file_count,
            omitted_file_count=stats.omitted_file_count,
            metadata_only_count=stats.metadata_only_count,
            compressed_file_count=stats.compressed_file_count,
            error_file_count=stats.error_file_count,
            truncated_file_count=truncated_file_count,
            input_chars=stats.input_chars,
            output_chars=len(content),
            warnings=warnings,
            decisions=ordered_decisions,
        )

    def _process_decision_without_context(
        self,
        decision: ContextDecision,
        parse_issue_lines: list[str],
        warnings: list[str],
        stats: _CompressionStats,
    ) -> None:
        stats.input_chars += decision.input_chars
        warning = f"missing context for {decision.file_path}"
        if warning not in warnings:
            warnings.append(warning)
        decision.action = ACTION_ERROR
        decision.reason = "missing_context"
        decision.output_chars = 0
        decision.truncated = False
        decision.error = warning
        stats.error_file_count += 1
        parse_issue_lines.append(
            f"- {decision.file_path} | reason=missing_context | error={warning}"
        )

    def _process_context_decision(
        self,
        *,
        sections: list[str],
        context: FileContext,
        decision: ContextDecision,
        profile: ContextProfile,
        omitted_decisions: list[ContextDecision],
        parse_issue_lines: list[str],
        stats: _CompressionStats,
    ) -> None:
        stats.input_chars += decision.input_chars if decision.input_chars else len(context.content)
        # truncated 是解析层事实，后续即使被 metadata/omit 策略省略，也要保留审计口径。
        decision.truncated = decision.truncated or context.truncated

        if context.error or decision.action == ACTION_ERROR:
            decision.action = ACTION_ERROR
            decision.reason = decision.reason or "parse_error"
            decision.error = decision.error or context.error
            decision.output_chars = 0
            stats.error_file_count += 1
            parse_issue_lines.append(self._render_parse_issue(context, decision))
            return

        if decision.action == ACTION_OMIT:
            decision.output_chars = 0
            omitted_decisions.append(decision)
            stats.omitted_file_count += 1
            return

        candidate = self._render_file_section(context, decision, profile)
        if not self._can_append_with_footer(sections, candidate, profile):
            self._mutate_to_global_omit(decision)
            omitted_decisions.append(decision)
            stats.omitted_file_count += 1
            return

        sections.append(candidate)
        stats.included_file_count += 1
        if decision.action == ACTION_COMPRESS:
            stats.compressed_file_count += 1
        elif decision.action == ACTION_METADATA_ONLY:
            stats.metadata_only_count += 1

    def _render_run_summary(
        self,
        scan_result: ScanResult,
        profile: ContextProfile,
    ) -> str:
        return "\n".join(
            [
                "## 本轮摘要",
                f"- 报告模式: {profile.report_mode}",
                f"- 压缩 profile: {profile.compression_profile}",
                f"- 扫描文件数: {scan_result.total_files}",
                f"- 成功解析数: {scan_result.success_count}",
                f"- 失败解析数: {scan_result.error_count}",
                f"- 全局上下文预算: {profile.global_context_max_chars}",
                f"- 单文件正文预算: {profile.per_file_max_chars}",
                f"- 压缩策略: {profile.compression_policy}",
            ]
        )

    def _render_notice(self, profile: ContextProfile) -> str:
        return "\n".join(
            [
                "## 重要提示",
                "- 以下内容来自本地 scanner 输出，context compressor 不重新读取文件、不调用 LLM、不写入 SQLite。",
                f"- 正文块受单文件预算 {profile.per_file_max_chars} 字符限制；超出全局预算的文件只保留审计摘要。",
            ]
        )

    def _render_file_section(
        self,
        context: FileContext,
        decision: ContextDecision,
        profile: ContextProfile,
    ) -> str:
        if decision.action == ACTION_METADATA_ONLY:
            decision.output_chars = 0
            return self._render_metadata_only(context, decision)

        body = context.content
        if len(body) > profile.per_file_max_chars:
            body = body[: profile.per_file_max_chars]
            decision.action = ACTION_COMPRESS
            decision.truncated = True
            decision.reason = decision.reason or "per_file_budget_exceeded"

        decision.output_chars = len(body)
        lines = [
            f"### {context.file_path}",
            f"- action: {decision.action}",
            f"- reason: {decision.reason}",
            f"- parser_backend: {context.parser_backend or decision.parser_backend or 'unknown'}",
            f"- input_chars: {decision.input_chars}",
            f"- output_chars: {decision.output_chars}",
        ]
        if decision.truncated:
            lines.append("- 内容已按单文件预算截断")
        lines.extend(["```text", body, "```"])
        return "\n".join(lines)

    def _render_metadata_only(
        self,
        context: FileContext,
        decision: ContextDecision,
    ) -> str:
        return "\n".join(
            [
                f"### {context.file_path}",
                f"- action: {ACTION_METADATA_ONLY}",
                f"- reason: {decision.reason}",
                f"- parser_backend: {context.parser_backend or decision.parser_backend or 'unknown'}",
                f"- file_type: {context.file_type}",
                f"- size_bytes: {decision.size_bytes}",
                f"- input_chars: {decision.input_chars}",
                "- body: omitted_by_metadata_only_policy",
            ]
        )

    def _append_omitted_summary(
        self,
        sections: list[str],
        omitted_decisions: list[ContextDecision],
        profile: ContextProfile,
        warnings: list[str],
    ) -> None:
        footer = "## 解析问题\n- 未发现解析问题。"

        def can_fit(lines: list[str]) -> bool:
            candidate = "\n".join(lines)
            # 省略摘要是审计信息，不应挤掉后面的解析问题入口；
            # 因此这里保守预留一个最小 footer，避免最终预算收口再整体截断正文。
            projected = _join_sections([*sections, candidate, footer])
            return len(projected) <= profile.global_context_max_chars

        lines = ["## 省略文件摘要", f"- 省略文件数: {len(omitted_decisions)}"]
        if not can_fit(lines):
            self._append_warning(warnings, _WARNING_OMITTED_SUMMARY_BUDGET)
            compact_lines = [
                "## 省略文件摘要",
                f"- 省略文件数: {len(omitted_decisions)}",
                f"- {_WARNING_OMITTED_SUMMARY_BUDGET}",
            ]
            if self._can_append_with_footer(sections, "\n".join(compact_lines), profile):
                sections.append("\n".join(compact_lines))
            return

        summary_truncated = False
        for decision in omitted_decisions:
            line = (
                "- "
                f"{decision.file_path} | action={decision.action} | "
                f"reason={decision.reason} | input_chars={decision.input_chars}"
            )
            if can_fit([*lines, line]):
                lines.append(line)
                continue
            summary_truncated = True
            break

        if summary_truncated:
            self._append_warning(warnings, _WARNING_OMITTED_SUMMARY_BUDGET)
            warning_line = (
                f"- {_WARNING_OMITTED_SUMMARY_BUDGET} "
                "完整逐文件决策请查看 context_decisions。"
            )
            if can_fit([*lines, warning_line]):
                lines.append(warning_line)

        sections.append("\n".join(lines))

    def _append_parse_issues(
        self,
        sections: list[str],
        parse_issue_lines: list[str],
        profile: ContextProfile,
        warnings: list[str],
    ) -> None:
        issue_body = parse_issue_lines or ["- 未发现解析问题。"]
        section = "## 解析问题\n" + "\n".join(issue_body)
        if self._can_append_exact(sections, section, profile):
            sections.append(section)
            return

        if _WARNING_PARSE_ISSUE_BUDGET not in warnings:
            self._append_warning(warnings, _WARNING_PARSE_ISSUE_BUDGET)
        warning_section = f"## 解析问题\n- {_WARNING_PARSE_ISSUE_BUDGET}"
        if self._can_append_exact(sections, warning_section, profile):
            sections.append(warning_section)

    def _render_parse_issue(
        self,
        context: FileContext,
        decision: ContextDecision,
    ) -> str:
        error = decision.error or context.error or "unknown_error"
        return f"- {context.file_path} | reason={decision.reason} | error={error}"

    def _can_append_with_footer(
        self,
        sections: list[str],
        candidate: str,
        profile: ContextProfile,
    ) -> bool:
        footer = "## 解析问题\n- 未发现解析问题。"
        projected = _join_sections([*sections, candidate, footer])
        return len(projected) <= profile.global_context_max_chars

    def _can_append_exact(
        self,
        sections: list[str],
        candidate: str,
        profile: ContextProfile,
    ) -> bool:
        projected = _join_sections([*sections, candidate])
        return len(projected) <= profile.global_context_max_chars

    def _fit_content_to_budget(
        self,
        sections: list[str],
        profile: ContextProfile,
        warnings: list[str],
    ) -> str:
        content = _join_sections(sections)
        if len(content) <= profile.global_context_max_chars:
            return content

        if _WARNING_FINAL_BUDGET not in warnings:
            self._append_warning(warnings, _WARNING_FINAL_BUDGET)

        return _truncate_content_to_budget(
            content,
            profile.global_context_max_chars,
        )

    def _mutate_to_global_omit(self, decision: ContextDecision) -> None:
        decision.action = ACTION_OMIT
        decision.reason = _GLOBAL_BUDGET_REASON
        decision.output_chars = 0

    def _count_truncated_files(self, decisions: list[ContextDecision]) -> int:
        """按文件去重统计 truncated 事实，避免同一文件多次决策时重复计数。"""
        return len({decision.file_path for decision in decisions if decision.truncated})

    def _append_warning(self, warnings: list[str], warning: str) -> None:
        if warning not in warnings:
            warnings.append(warning)

    def _default_decision(self, context: FileContext) -> ContextDecision:
        return ContextDecision(
            file_path=context.file_path,
            extension=context.file_type,
            size_bytes=0,
            parser_backend=context.parser_backend,
            worker_lane="unknown",
            cache_status="unknown",
            action=ACTION_KEEP,
            reason="implicit_keep",
            priority=100,
            input_chars=len(context.content),
            output_chars=0,
            truncated=context.truncated,
            error=context.error,
        )


def _positive_int(value: int, default: int) -> int:
    return value if value > 0 else default


def _join_sections(sections: list[str]) -> str:
    return "\n\n".join(section.rstrip() for section in sections).rstrip() + "\n"


def _truncate_content_to_budget(content: str, budget: int) -> str:
    if budget <= 0:
        return ""

    note = (
        "\n\n## 预算提示\n"
        "- 已按全局上下文预算截断尾部内容，完整逐文件决策请查看 context_decisions。\n"
    )
    if budget <= len(note) + 20:
        return content[:budget]

    keep_chars = budget - len(note)
    prefix = content[:keep_chars].rstrip()
    newline_index = prefix.rfind("\n")
    if newline_index >= keep_chars - 200:
        prefix = prefix[:newline_index].rstrip()

    # 最终兜底只截断尾部低价值审计内容，不能把已进入正文预算的文件证据整体替换掉。
    return (prefix + note)[:budget]


__all__ = [
    "ACTION_COMPRESS",
    "ACTION_ERROR",
    "ACTION_KEEP",
    "ACTION_METADATA_ONLY",
    "ACTION_OMIT",
    "CompressedContext",
    "ContextCompressor",
    "ContextDecision",
    "ContextProfile",
]
