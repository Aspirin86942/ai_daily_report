"""Convert native coverage into local report evidence and allow one unstable-file retry."""

from dataclasses import replace
from hashlib import sha256
from pathlib import Path

from ...models.evidence_report import CoverageGap
from ..native_scanner import ScanRequest, ScanResult
from .context import RunContext
from .model_calls import Deadline


INFORMATIONAL_WARNINGS = {"WORKER_RSS_UNAVAILABLE"}


def _unstable(source) -> bool:
    reason = (source.reason or "").lower()
    return "unstable" in reason or "source_changed" in reason or reason in {"source_guard_unavailable", "source_version_changed"}


def scan_with_retry(scanner, context: RunContext, deadline: Deadline) -> ScanResult:
    request = ScanRequest("daily", context.target_date, context.target_date,
                          window_start_utc=context.window_start_utc, window_end_utc=context.window_end_utc)
    deadline.remaining()
    first = scanner.build_context(request)
    if not any(_unstable(source) for source in first.sources):
        deadline.remaining()
        return first
    deadline.remaining()
    second = scanner.build_context(request)
    deadline.remaining()
    present = {source.source_id: source for source in second.sources}
    # A file can change mtime or disappear during the retry. Keep the original
    # candidate visible as unavailable rather than silently removing its gap.
    for source in first.sources:
        if source.source_id not in present:
            present[source.source_id] = source.model_copy(update={"parse_status": "error", "excerpt": "",
                "excerpt_sha256": sha256(b"").hexdigest(), "content_sha256": None,
                "reason": "source_unstable_disappeared_on_retry"})
    if second.envelope.status == "error":
        # The second scan cannot prove coverage even if an earlier excerpt existed.
        return replace(second, sources=tuple(present.values()))
    return replace(second, sources=tuple(sorted(present.values(), key=lambda source: source.source_id)))


def coverage_gaps(result: ScanResult, *, root: Path | None = None) -> list[CoverageGap]:
    gaps = []
    for warning in result.envelope.warnings:
        if warning.error_code in INFORMATIONAL_WARNINGS:
            continue
        relative = None
        raw_path = getattr(warning, "file_path", None)
        if raw_path and root:
            try:
                relative = Path(raw_path).relative_to(root).as_posix()
            except ValueError:
                pass
        gaps.append(CoverageGap(code=warning.error_code, message=f"扫描存在未覆盖范围，诊断代码：{warning.error_code}。",
                                stage="discovery", relative_path=relative))
    if result.envelope.status == "error":
        code = result.envelope.error.error_code if result.envelope.error else "SCAN_UNAVAILABLE"
        gaps.append(CoverageGap(code=code, message="文件扫描未能完成，文件内容覆盖无法确认。", stage="discovery"))
    elif result.envelope.status == "partial" and not gaps:
        gaps.append(CoverageGap(code="SCAN_PARTIAL", message="扫描器报告存在部分未完成的采集范围。", stage="discovery"))
    for source in result.sources:
        if source.coverage_limits.get("ocr", {}).get("text_unverified"):
            gaps.append(CoverageGap(code="OCR_TEXT_UNVERIFIED", stage="parse", source_id=source.source_id,
                                   message="扫描件文字由本地识别生成，错字、数值及表格对应关系需要对照原 PDF 核实。"))
        if source.parse_status != "success":
            message = "文件读取期间发生变化，稳定性重试后仍无法取得可靠内容。" if _unstable(source) else "文件未能成功解析。"
            gaps.append(CoverageGap(code="SOURCE_UNAVAILABLE", message=message, stage="parse", source_id=source.source_id))
        elif source.parse_truncated or source.extension == ".csv":
            gaps.append(CoverageGap(code="PARSE_BOUNDED", message="只提取了限定页数、行数或字符范围，结论限于已提取部分。",
                                    stage="parse", source_id=source.source_id))
        if source.excerpt_truncated or (source.parse_status == "success" and not source.excerpt):
            gaps.append(CoverageGap(code="EXCERPT_BOUNDED", message="此来源的摘录未覆盖全部可解析内容。",
                                    stage="excerpt", source_id=source.source_id))
    # Cache/runtime wording is deliberately excluded from stable gap identity.
    unique = {(gap.code, gap.stage, gap.source_id, gap.relative_path): gap for gap in gaps}
    return [unique[key] for key in sorted(unique, key=lambda key: tuple(part or "" for part in key))]
