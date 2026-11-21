use crate::{
    reports::data::{CoverageGap, ReportSource},
    snapshots::reject_redirects,
    types::RunContext,
    ReportError,
};
use ai_daily_scanner_contract::{EngineStatus, ErrorCode, PdfClassificationStatus};
use ai_daily_scanner_core::ContextResult;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
};
pub(super) fn scan_sources(
    scan: ContextResult,
    gaps: &mut Vec<CoverageGap>,
) -> Result<Vec<(ReportSource, String)>, ReportError> {
    let mut sources = Vec::with_capacity(scan.sources.len());
    for warning in &scan.envelope.warnings {
        if warning.error_code != ErrorCode::WorkerRssUnavailable {
            let code = serde_json::to_value(warning.error_code)?;
            gaps.push(CoverageGap::new(
                code.as_str().unwrap(),
                "discovery",
                None,
                "扫描存在未覆盖的范围，请结合其他材料确认。",
            ));
        }
    }
    if scan.envelope.status == EngineStatus::Error {
        gaps.push(CoverageGap::new(
            "SCAN_UNAVAILABLE",
            "discovery",
            None,
            "扫描未能完成，文件内容覆盖无法确认。",
        ));
    } else if scan.envelope.status == EngineStatus::Partial && gaps.is_empty() {
        gaps.push(CoverageGap::new(
            "SCAN_PARTIAL",
            "discovery",
            None,
            "扫描存在未完成的采集范围。",
        ));
    }
    let native: BTreeMap<_, _> = scan
        .evidence
        .as_ref()
        .map(|e| {
            e.files
                .iter()
                .map(|f| (f.relative_path.replace('\\', "/"), f))
                .collect()
        })
        .unwrap_or_default();
    for raw in scan.sources {
        let mut source = ReportSource::from_scan(&raw)?;
        let unverified = source.unverified();
        let mut audit = match source.coverage_limits["evidence_audit"].take() {
            Value::Object(audit) => audit,
            _ => Default::default(),
        };
        let mut tags: BTreeSet<&str> = raw.coverage_limits["evidence_audit"]
            .get("secondary_tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let gap = |code: &str, stage: &str, message: &str| {
            CoverageGap::new(code, stage, Some(&source.source_id), message)
        };
        let mut tag_gap = |code: &'static str, stage: &str, message: &str| {
            tags.insert(code);
            gaps.push(gap(code, stage, message));
        };
        // Both fresh and replayed scans set this after checking the original file hash.
        let status = &raw.coverage_limits["evidence_audit"]["primary_status"];
        if status == "parse_failed" || status == "not_parsed" {
            let (code, message) = match audit.get("primary_reason").and_then(Value::as_str) {
                Some("office_content_missing") => (
                    "OFFICE_CONTENT_MISSING",
                    "Office 文件仅含包元数据，缺少可提取的文档正文。".to_owned(),
                ),
                Some("file_size_policy") => {
                    let limit = audit
                        .get("max_file_size_bytes")
                        .and_then(Value::as_u64)
                        .map(|n| format!(" {n} 字节"))
                        .unwrap_or_default();
                    (
                        "FILE_SIZE_LIMIT",
                        format!(
                            "文件为 {} 字节，超过本次扫描的单文件上限{limit}，未进入解析。",
                            source.size_bytes
                        ),
                    )
                }
                Some("pdf_image_ocr_file_quota_exhausted") => (
                    "OCR_BUDGET",
                    "已确认整份 PDF 为纯图，本次扫描的识字文件额度已用完。".to_owned(),
                ),
                Some("pdf_image_ocr_page_quota_exhausted") => (
                    "OCR_BUDGET",
                    "已确认整份 PDF 为纯图，本次扫描的识字页数额度已用完。".to_owned(),
                ),
                Some("pdf_image_ocr_timeout") => {
                    ("OCR_TIMEOUT", "纯图 PDF 的本地识字已到时限。".to_owned())
                }
                Some("pdf_image_ocr_failed") => (
                    "OCR_UNAVAILABLE",
                    "纯图 PDF 本地识字失败，详见扫描诊断。".to_owned(),
                ),
                _ => (
                    "SOURCE_UNAVAILABLE",
                    "文件未能成功读取或无法确认其稳定版本。".to_owned(),
                ),
            };
            tag_gap(code, "parse", &message);
        }
        if source.parse_truncated {
            tag_gap(
                "PARSE_BOUNDED",
                "parse",
                "只提取了限定页数、行数或字符范围，结论限于已读取部分。",
            );
        }
        if source.excerpt_truncated || status == "empty_excerpt" {
            tag_gap(
                "EXCERPT_BOUNDED",
                "excerpt",
                "此来源的摘录未覆盖全部内容，或没有取得有效文字。",
            );
        }
        if unverified {
            audit.insert("text_unverified".into(), json!(true));
            tag_gap(
                "OCR_TEXT_UNVERIFIED",
                "parse",
                "识别文字尚未核实，数字和表格对应关系需对照原文件确认。",
            );
        }
        if let Some(history) = source.coverage_limits.get("history") {
            if history["prior_content_sha256"].is_null() {
                gaps.push(gap(
                    "HISTORICAL_CONTENT_UNKNOWN",
                    "parse",
                    "旧报告缺少原文件内容哈希，本次依据冻结时可取得的版本。",
                ));
            } else if history["current_version_changed"] == true {
                gaps.push(gap(
                    "HISTORICAL_CONTENT_CHANGED",
                    "parse",
                    "当前文件与旧报告记录的内容不同，无法据此还原历史版本。",
                ));
            }
        }
        if let Some(file) = native.get(source.relative_path.as_deref().unwrap_or("")) {
            if let Some(classification) = &file.pdf_classification.0 {
                if let Some(pages) = &classification.page_audit {
                    audit.insert("pdf_page_audit".into(), serde_json::to_value(pages)?);
                    if pages
                        .observations
                        .iter()
                        .flatten()
                        .any(|p| p.has_image == Some(true))
                    {
                        tags.insert("PDF_OBSERVED_IMAGE");
                    }
                    if classification.status == PdfClassificationStatus::NoTextInParseWindow
                        && !unverified
                    {
                        audit
                            .entry("primary_reason")
                            .or_insert_with(|| json!("pdf_window_no_text"));
                        let ranges: Vec<_> = pages.checked_pages.iter().map(|page|
                            json!({"unit":"pdf_page","start":page,"end":page+1,"label":"classification"})
                        ).collect();
                        audit.insert("read_ranges".into(), json!(ranges));
                    }
                }
            }
            if source.extension.as_deref() == Some(".pdf") {
                if let Some(diagnostic) = &file.final_diagnostic.0 {
                    tags.insert(match diagnostic.error_code {
                        ErrorCode::PdfProtected => "PDF_PROTECTED",
                        ErrorCode::ParserTimeout => "PDF_PROBE_TIMEOUT",
                        _ => "PDF_PARSE_FAILED",
                    });
                }
            }
        }
        audit.insert("secondary_tags".into(), json!(tags));
        source.coverage_limits["evidence_audit"] = Value::Object(audit);
        sources.push((source, raw.excerpt));
    }
    sources.sort_by(|(a, _), (b, _)| a.source_id.cmp(&b.source_id));
    fn key(gap: &CoverageGap) -> (&str, Option<&str>, Option<&str>, &str) {
        (
            &gap.code,
            gap.source_id.as_deref(),
            gap.relative_path.as_deref(),
            &gap.stage,
        )
    }
    gaps.sort_by(|a, b| key(a).cmp(&key(b)));
    gaps.dedup_by(|a, b| key(a) == key(b));
    Ok(sources)
}
pub(crate) fn personal_notes(
    config: &crate::config::AppConfig,
    context: &RunContext,
    input: &str,
) -> Result<String, ReportError> {
    let path = config
        .report
        .notes_dir
        .join(format!("{}.md", context.target_date));
    reject_redirects(&path, "NOTES_PATH_REDIRECTED")?;
    let mut bytes = Vec::new();
    match File::open(&path) {
        Ok(file) => {
            file.take(8_004).read_to_end(&mut bytes)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if bytes.len() > 8_003 || input.chars().count() > 2_000 {
        return Err(ReportError::new(
            "USER_INPUT_TOO_LARGE",
            "单日补充最多 2,000 字，请缩短原文",
        ));
    }
    let file_text = String::from_utf8(bytes)
        .map_err(|_| ReportError::new("NOTES_ENCODING_INVALID", "补充须为 UTF-8"))?;
    let mut seen = BTreeSet::new();
    let mut paragraphs = Vec::new();
    for text in [input, file_text.trim_start_matches('\u{feff}')] {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for paragraph in normalized
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            if seen.insert(paragraph.to_owned()) {
                paragraphs.push(paragraph.to_owned());
            }
        }
    }
    let text = paragraphs.join("\n\n");
    if text.chars().count() > 2_000 {
        return Err(ReportError::new(
            "USER_INPUT_TOO_LARGE",
            "合并后的单日补充超过 2,000 字，请缩短原文",
        ));
    }
    Ok(text)
}
