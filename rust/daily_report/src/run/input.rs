use crate::{
    reports::data::{CoverageGap, ReportSource},
    scan::{
        sources::{ScannedFile, SourceProfile},
        ScanResult,
    },
    snapshots::reject_redirects,
    types::RunContext,
    ReportError,
};
use daily_parsers::settings::{EngineStatus, ErrorCode, ParseStatus};
use serde_json::Value;
use std::{collections::BTreeSet, fs::File, io::Read};

pub(super) fn scan_gaps(scan: &ScanResult, gaps: &mut Vec<CoverageGap>) -> Result<(), ReportError> {
    for warning in scan
        .warnings
        .iter()
        .filter(|warning| warning.error_code != ErrorCode::WorkerRssUnavailable)
    {
        let code = serde_json::to_value(warning.error_code)?;
        gaps.push(CoverageGap::new(
            code.as_str().unwrap(),
            "discovery",
            None,
            "扫描存在未覆盖的范围，请结合其他材料确认。",
        ));
    }
    let (code, message) = match scan.status {
        EngineStatus::Error => ("SCAN_UNAVAILABLE", "扫描未能完成，文件内容覆盖无法确认。"),
        EngineStatus::Partial if gaps.is_empty() => ("SCAN_PARTIAL", "扫描存在未完成的采集范围。"),
        _ => return Ok(()),
    };
    gaps.push(CoverageGap::new(code, "discovery", None, message));
    Ok(())
}

pub(super) fn file_gap(file: &ScannedFile, code: &str, stage: &str, message: &str) -> CoverageGap {
    let mut gap = CoverageGap::new(code, stage, Some(&file.source_id), message);
    gap.relative_path = Some(file.relative_path.clone());
    gap
}

pub(super) fn file_gaps<'a>(
    file: &'a ScannedFile,
    gaps: &mut Vec<CoverageGap>,
) -> BTreeSet<&'a str> {
    let mut tags: BTreeSet<_> = file.ocr_tag.as_deref().into_iter().collect();
    let mut tag_gap = |code: &'static str, stage: &str, message: &str| {
        tags.insert(code);
        gaps.push(file_gap(file, code, stage, message));
    };
    if file.parse_status != ParseStatus::Success {
        let size_message;
        let (code, message) = match file.reason {
            Some("office_content_missing") => (
                "OFFICE_CONTENT_MISSING",
                "Office 文件仅含包元数据，缺少可提取的文档正文。",
            ),
            Some("file_size_policy") => {
                let limit = file
                    .max_file_size_bytes
                    .map(|n| format!(" {n} 字节"))
                    .unwrap_or_default();
                size_message = format!(
                    "文件为 {} 字节，超过本次扫描的单文件上限{limit}，未进入解析。",
                    file.size_bytes
                );
                ("FILE_SIZE_LIMIT", size_message.as_str())
            }
            Some("pdf_image_ocr_file_quota_exhausted") => (
                "OCR_BUDGET",
                "已确认整份 PDF 为纯图，本次扫描的识字文件额度已用完。",
            ),
            Some("pdf_image_ocr_page_quota_exhausted") => (
                "OCR_BUDGET",
                "已确认整份 PDF 为纯图，本次扫描的识字页数额度已用完。",
            ),
            Some("pdf_image_ocr_timeout") => ("OCR_TIMEOUT", "纯图 PDF 的本地识字已到时限。"),
            Some("pdf_image_ocr_failed") => {
                ("OCR_UNAVAILABLE", "纯图 PDF 本地识字失败，详见扫描诊断。")
            }
            _ => (
                "SOURCE_UNAVAILABLE",
                "文件未能成功读取或无法确认其稳定版本。",
            ),
        };
        tag_gap(code, "parse", message);
    }
    if file.parse_truncated {
        tag_gap(
            "PARSE_BOUNDED",
            "parse",
            "只提取了限定页数、行数或字符范围，结论限于已读取部分。",
        );
    }
    if file.excerpt.truncated
        || (file.parse_status == ParseStatus::Success && file.excerpt.text.is_empty())
    {
        tag_gap(
            "EXCERPT_BOUNDED",
            "excerpt",
            "此来源的摘录未覆盖全部内容，或没有取得有效文字。",
        );
    }
    if file.unverified {
        tag_gap(
            "OCR_TEXT_UNVERIFIED",
            "parse",
            "识别文字尚未核实，数字和表格对应关系需对照原文件确认。",
        );
    }
    if file.pdf_pages().is_some_and(|pages| {
        pages
            .observations
            .iter()
            .flatten()
            .any(|page| page.has_image == Some(true))
    }) {
        tags.insert("PDF_OBSERVED_IMAGE");
    }
    if file.extension == ".pdf" {
        if let Some(code) = file.pdf_error_code {
            tags.insert(match code {
                ErrorCode::PdfProtected => "PDF_PROTECTED",
                ErrorCode::ParserTimeout => "PDF_PROBE_TIMEOUT",
                _ => "PDF_PARSE_FAILED",
            });
        }
    }
    tags
}

pub(super) fn sort_gaps(gaps: &mut Vec<CoverageGap>) {
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
}

pub(super) fn report_source(
    file: &ScannedFile,
    profile: &SourceProfile,
) -> Result<ReportSource, ReportError> {
    let source = ReportSource {
        source_id: file.source_id.clone(),
        source_kind: "file".into(),
        relative_path: Some(file.relative_path.clone()),
        extension: Some(file.extension.clone()),
        size_bytes: file.size_bytes,
        modified_at_utc: Some(file.modified_at_utc.clone()),
        observed_at_utc: profile.observed_at_utc.clone(),
        content_sha256: file.raw_sha256.clone(),
        excerpt_sha256: file.excerpt_sha256.clone(),
        excerpt_chars: file.excerpt.chars,
        model_chars: 0,
        parser_backend: Some(file.parser_backend.to_owned()),
        worker_lane: Some(file.worker_lane.into()),
        parse_status: file.parse_status.as_str().into(),
        parse_truncated: file.parse_truncated,
        excerpt_truncated: file.excerpt.truncated,
        model_truncated: false,
        coverage_limits: Value::Null,
        reason: file.reason.map(str::to_owned),
        snapshot_ref: None,
    };
    source.validate()?;
    Ok(source)
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
