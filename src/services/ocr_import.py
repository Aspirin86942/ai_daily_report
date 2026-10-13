"""Hash-bound, page-bound import of explicitly supplied local OCR text."""
from __future__ import annotations

import json
import re
from hashlib import sha256
from pathlib import Path

from src.models.content_audit import SourceRange
from src.models.scan_sources import ScanSource
from src.services.document_parser import _PreviewBuilder
from src.services.report_runner.errors import ReportRunError
from src.workers.ocr import OcrError, OcrLimits, pdf_identity


def load_ocr_manifest(path: Path | None, limits: OcrLimits | None = None) -> dict[str, dict]:
    if path is None:
        return {}
    limits = limits or OcrLimits()
    if path.stat().st_size > 4 * 1024 * 1024:
        raise ReportRunError("BACKFILL_OCR_MANIFEST_LIMIT")
    try:
        documents = json.loads(path.read_text(encoding="utf-8-sig"))
        if not isinstance(documents, list) or len(documents) > 1000:
            raise ValueError()
        result = {}
        for document in documents:
            source_id = document["source_id"]
            digest = document["content_sha256"]
            total = document["pages_total"]
            pages = document["pages"]
            if (source_id in result or not re.fullmatch(r"src_[a-f0-9]{64}", source_id)
                or not re.fullmatch(r"[a-f0-9]{64}", digest) or type(total) is not int or total < 1
                or not isinstance(pages, list) or not 1 <= len(pages) <= limits.max_pages_per_file):
                raise ValueError()
            numbers, copied = [], []
            for page in pages:
                number = page["page"]
                if type(number) is not int or not 1 <= number <= total or number in numbers:
                    raise ReportRunError("BACKFILL_OCR_PAGE_OUT_OF_RANGE")
                text_path = Path(page["text"])
                if not text_path.is_absolute():
                    text_path = path.parent / text_path
                if text_path.stat().st_size > limits.max_output_chars * 4 + 3:
                    raise ReportRunError("BACKFILL_OCR_OUTPUT_LIMIT")
                data = text_path.read_bytes()
                if not re.fullmatch(r"[a-f0-9]{64}", page.get("text_sha256", "")) or sha256(data).hexdigest() != page["text_sha256"]:
                    raise ReportRunError("BACKFILL_OCR_TEXT_CHANGED")
                text = data.decode("utf-8-sig", errors="strict")
                copied.append({"page": number, "text": text, "text_sha256": page["text_sha256"]})
                numbers.append(number)
            if sum(len(page["text"]) for page in copied) > limits.max_output_chars:
                raise ReportRunError("BACKFILL_OCR_OUTPUT_LIMIT")
            if not any(page["text"].strip() for page in copied):
                raise ReportRunError("BACKFILL_OCR_EMPTY")
            result[source_id] = {"content_sha256": digest, "pages_total": total, "pages": copied}
        return result
    except ReportRunError:
        raise
    except (KeyError, TypeError, ValueError, OSError):
        raise ReportRunError("BACKFILL_OCR_INVALID_MANIFEST") from None


def overlay_ocr_source(source: ScanSource, document: dict, work_root: Path,
                       limits: OcrLimits | None = None) -> ScanSource:
    limits = limits or OcrLimits()
    if source.extension != ".pdf":
        raise ReportRunError("BACKFILL_OCR_INVALID_SOURCE")
    path = work_root / source.relative_path
    try:
        path.absolute().relative_to(work_root.absolute())
        digest, count = pdf_identity(path, limits.max_file_size_bytes)
    except (OcrError, OSError, ValueError):
        raise ReportRunError("BACKFILL_OCR_SOURCE_CHANGED") from None
    if (digest != document["content_sha256"] or count != document["pages_total"]
        or source.size_bytes != path.stat().st_size
        or source.content_sha256 is not None and source.content_sha256 != digest):
        raise ReportRunError("BACKFILL_OCR_SOURCE_CHANGED")
    builder = _PreviewBuilder(limits.max_output_chars)
    for page in document["pages"]:
        range_ = SourceRange(unit="pdf_page", start=page["page"], end=page["page"] + 1, label="unverified_ocr")
        builder.read(range_)
        builder.add(f"## Page {page['page']}")
        for paragraph in page["text"].splitlines():
            builder.add(paragraph, ranges=[range_])
    excerpt = builder.build()
    audit = {**builder.audit("pdf").model_dump(mode="json"), "primary_status": "usable_excerpt",
        "primary_reason": None, "text_unverified": True, "parser_version": "local_ocr_import_v2",
        "secondary_tags": ["OCR_TEXT_UNVERIFIED"]}
    coverage = {**source.coverage_limits, "evidence_audit": audit, "ocr": {
        "engine": "local_import", "pages_read": len(document["pages"]), "pages_total": count,
        "pages": [page["page"] for page in document["pages"]], "text_unverified": True,
        "text_sha256": [page["text_sha256"] for page in document["pages"]]}}
    return ScanSource.model_validate({**source.model_dump(), "excerpt": excerpt,
        "excerpt_sha256": sha256(excerpt.encode("utf-8")).hexdigest(), "content_sha256": digest,
        "parser_backend": "local_ocr_import_v2", "worker_lane": "local_ocr_preflight", "parse_status": "success",
        "parse_truncated": len(document["pages"]) < count, "excerpt_truncated": builder.was_truncated,
        "coverage_limits": coverage, "reason": "local_ocr_text_requires_verification"})
