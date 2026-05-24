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

_PROFILE_VERSION = "context_compressor_v1"
_GLOBAL_BUDGET_REASON = "global_budget_exceeded"


@dataclass(frozen=True, slots=True)
class ContextProfile:
    """定义本轮报告上下文预算，供 scheduler 和审计侧稳定复用。"""

    report_mode: str
    global_context_max_chars: int
    per_file_max_chars: int
    profile_version: str = _PROFILE_VERSION

    @classmethod
    def for_report_mode(cls, report_mode: str) -> "ContextProfile":
        """按报告模式生成默认预算，避免调用方散落硬编码。"""
        normalized_mode = report_mode.strip().lower()
        defaults = {
            "daily": (12_000, 3_000),
            "weekly": (30_000, 5_000),
            "monthly": (45_000, 6_000),
        }
        if normalized_mode not in defaults:
            raise ValueError(f"unsupported report_mode: {report_mode!r}")

        global_budget, per_file_budget = defaults[normalized_mode]
        return cls(
            report_mode=normalized_mode,
            global_context_max_chars=global_budget,
            per_file_max_chars=per_file_budget,
        )

    def with_budget(
        self,
        *,
        global_context_max_chars: int | None = None,
        per_file_max_chars: int | None = None,
    ) -> "ContextProfile":
        """返回预算覆盖后的新 profile，保持原 profile 不被隐式改写。"""
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
        """输出可序列化 profile，供日志、cache key 或审计摘要使用。"""
        return {
            "report_mode": self.report_mode,
            "global_context_max_chars": self.global_context_max_chars,
            "per_file_max_chars": self.per_file_max_chars,
            "profile_version": self.profile_version,
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
    """压缩后的上下文正文和统计结果。"""

    content: str
    decisions: list[ContextDecision]
    profile: ContextProfile
    source_file_count: int
    included_file_count: int
    compressed_file_count: int
    metadata_only_count: int
    omitted_file_count: int
    error_file_count: int
    output_chars: int

    @classmethod
    def empty(cls, profile: ContextProfile | None = None) -> "CompressedContext":
        """构造无文件证据时仍可审计的上下文。"""
        resolved_profile = profile or ContextProfile.for_report_mode("daily")
        content = _join_sections(
            [
                "# 文件证据上下文",
                "## 本轮摘要\n- 扫描文件数: 0\n- 纳入文件数: 0\n- 省略文件数: 0",
                "## 重要提示\n- 无文件证据：本轮扫描未提供可用于报告生成的文件内容。",
                "## 文件证据\n无文件证据",
                "## 解析问题\n- 未发现解析问题。",
            ]
        )
        return cls(
            content=content,
            decisions=[],
            profile=resolved_profile,
            source_file_count=0,
            included_file_count=0,
            compressed_file_count=0,
            metadata_only_count=0,
            omitted_file_count=0,
            error_file_count=0,
            output_chars=len(content),
        )

    def to_summary(self) -> dict[str, Any]:
        """输出轻量摘要，避免调用方解析 Markdown 正文拿统计。"""
        return {
            "source_file_count": self.source_file_count,
            "included_file_count": self.included_file_count,
            "compressed_file_count": self.compressed_file_count,
            "metadata_only_count": self.metadata_only_count,
            "omitted_file_count": self.omitted_file_count,
            "error_file_count": self.error_file_count,
            "output_chars": self.output_chars,
            "profile": self.profile.to_profile_dict(),
        }


class ContextCompressor:
    """把 scanner 输出压缩为可直接喂给报告生成的确定性上下文。"""

    def compress(
        self,
        *,
        scan_result: ScanResult,
        decisions: list[ContextDecision],
        profile: ContextProfile,
    ) -> CompressedContext:
        """按已给决策和预算渲染 Markdown-like 文件证据上下文。"""
        if not scan_result.contexts:
            return CompressedContext.empty(profile)

        decision_by_path = {decision.file_path: decision for decision in decisions}
        ordered_decisions: list[ContextDecision] = []
        omitted_decisions: list[ContextDecision] = []
        parse_issue_lines: list[str] = []

        included_file_count = 0
        compressed_file_count = 0
        metadata_only_count = 0
        omitted_file_count = 0
        error_file_count = 0

        sections = [
            "# 文件证据上下文",
            self._render_run_summary(scan_result, profile),
            self._render_notice(profile),
            "## 文件证据",
        ]

        for context in scan_result.contexts:
            decision = decision_by_path.get(context.file_path)
            if decision is None:
                decision = self._default_decision(context)
            ordered_decisions.append(decision)

            if context.error or decision.action == ACTION_ERROR:
                decision.action = ACTION_ERROR
                decision.reason = decision.reason or "parse_error"
                decision.error = decision.error or context.error
                decision.output_chars = 0
                error_file_count += 1
                parse_issue_lines.append(self._render_parse_issue(context, decision))
                continue

            candidate = self._render_file_section(context, decision, profile)
            if not self._can_append_with_footer(sections, candidate, profile):
                self._mutate_to_global_omit(decision)
                omitted_decisions.append(decision)
                omitted_file_count += 1
                continue

            sections.append(candidate)
            included_file_count += 1
            if decision.action == ACTION_COMPRESS:
                compressed_file_count += 1
            elif decision.action == ACTION_METADATA_ONLY:
                metadata_only_count += 1

        for decision in decisions:
            if decision.file_path not in {item.file_path for item in ordered_decisions}:
                ordered_decisions.append(decision)
                if decision.action == ACTION_OMIT:
                    omitted_decisions.append(decision)
                    omitted_file_count += 1

        if included_file_count == 0:
            sections.append("无文件证据")

        if omitted_decisions:
            sections.append(self._render_omitted_summary(omitted_decisions))

        self._append_parse_issues(sections, parse_issue_lines, profile)
        content = _join_sections(sections)

        return CompressedContext(
            content=content,
            decisions=ordered_decisions,
            profile=profile,
            source_file_count=scan_result.total_files,
            included_file_count=included_file_count,
            compressed_file_count=compressed_file_count,
            metadata_only_count=metadata_only_count,
            omitted_file_count=omitted_file_count,
            error_file_count=error_file_count,
            output_chars=len(content),
        )

    def _render_run_summary(
        self,
        scan_result: ScanResult,
        profile: ContextProfile,
    ) -> str:
        return "\n".join(
            [
                "## 本轮摘要",
                f"- 报告模式: {profile.report_mode}",
                f"- 扫描文件数: {scan_result.total_files}",
                f"- 成功解析数: {scan_result.success_count}",
                f"- 失败解析数: {scan_result.error_count}",
                f"- 全局上下文预算: {profile.global_context_max_chars}",
                f"- 单文件正文预算: {profile.per_file_max_chars}",
                f"- 压缩 profile: {profile.profile_version}",
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

    def _render_omitted_summary(self, omitted_decisions: list[ContextDecision]) -> str:
        lines = ["## 省略文件摘要"]
        for decision in omitted_decisions:
            lines.append(
                "- "
                f"{decision.file_path} | action={decision.action} | "
                f"reason={decision.reason} | input_chars={decision.input_chars}"
            )
        return "\n".join(lines)

    def _append_parse_issues(
        self,
        sections: list[str],
        parse_issue_lines: list[str],
        profile: ContextProfile,
    ) -> None:
        issue_body = parse_issue_lines or ["- 未发现解析问题。"]
        section = "## 解析问题\n" + "\n".join(issue_body)
        if self._can_append_exact(sections, section, profile):
            sections.append(section)
            return

        warning = "## 解析问题\n- 全局预算不足，解析问题明细未展开。"
        sections.append(warning)

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

    def _mutate_to_global_omit(self, decision: ContextDecision) -> None:
        decision.action = ACTION_OMIT
        decision.reason = _GLOBAL_BUDGET_REASON
        decision.output_chars = 0
        decision.truncated = False

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


def _positive_int(value: int | None, default: int) -> int:
    if value is None:
        return default
    return value if value > 0 else default


def _join_sections(sections: list[str]) -> str:
    return "\n\n".join(section.rstrip() for section in sections).rstrip() + "\n"


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
