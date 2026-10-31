use crate::{
    reports::data::{CoverageGap, ReportSource},
    snapshots::reject_redirects,
    types::RunContext,
    ReportError,
};
use ai_daily_scanner_contract::EngineStatus;
use ai_daily_scanner_core::ContextResult;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
};
fn source_gap(source: &ReportSource, code: &str, stage: &str, message: &str) -> CoverageGap {
    CoverageGap::new(code, stage, Some(&source.source_id), message)
}
pub(super) fn scan_sources(
    scan: &ContextResult,
) -> Result<(Vec<ReportSource>, Vec<CoverageGap>), ReportError> {
    let mut sources = Vec::new();
    let mut gaps = Vec::new();
    for warning in &scan.envelope.warnings {
        let code = serde_json::to_value(warning.error_code)?
            .as_str()
            .unwrap()
            .to_owned();
        if code != "WORKER_RSS_UNAVAILABLE" {
            gaps.push(CoverageGap::new(
                &code,
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
    for raw in &scan.sources {
        let mut source = ReportSource::from_scan(raw)?;
        let mut audit = source.coverage_limits["evidence_audit"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut tags: BTreeSet<String> = audit
            .get("secondary_tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let has_hash = source.content_sha256.is_some();
        let status = if source.parse_status == "not_parsed" {
            "not_parsed"
        } else if source.parse_status != "success" || source.excerpt_chars > 0 && !has_hash {
            "parse_failed"
        } else if source.excerpt_chars == 0 || audit.get("usable_chars") == Some(&json!(0)) {
            "empty_excerpt"
        } else {
            "usable_excerpt"
        };
        audit.insert("primary_status".into(), json!(status));
        if status == "parse_failed" || status == "not_parsed" {
            tags.insert("SOURCE_UNAVAILABLE".into());
            gaps.push(source_gap(
                &source,
                "SOURCE_UNAVAILABLE",
                "parse",
                "文件未能成功读取或无法确认其稳定版本。",
            ));
        }
        if source.parse_truncated || source.extension.as_deref() == Some(".csv") {
            tags.insert("PARSE_BOUNDED".into());
            gaps.push(source_gap(
                &source,
                "PARSE_BOUNDED",
                "parse",
                "只提取了限定页数、行数或字符范围，结论限于已读取部分。",
            ));
        }
        if source.excerpt_truncated || status == "empty_excerpt" {
            tags.insert("EXCERPT_BOUNDED".into());
            gaps.push(source_gap(
                &source,
                "EXCERPT_BOUNDED",
                "excerpt",
                "此来源的摘录未覆盖全部内容，或没有取得有效文字。",
            ));
        }
        if source.unverified() {
            tags.insert("OCR_TEXT_UNVERIFIED".into());
            audit.insert("text_unverified".into(), json!(true));
            gaps.push(source_gap(
                &source,
                "OCR_TEXT_UNVERIFIED",
                "parse",
                "识别文字尚未核实，数字和表格对应关系需对照原文件确认。",
            ));
        }
        if let Some(history) = source.coverage_limits.get("history") {
            if history["prior_content_sha256"].is_null() {
                gaps.push(source_gap(
                    &source,
                    "HISTORICAL_CONTENT_UNKNOWN",
                    "parse",
                    "旧报告缺少原文件内容哈希，本次依据冻结时可取得的版本。",
                ));
            } else if history["current_version_changed"] == true {
                gaps.push(source_gap(
                    &source,
                    "HISTORICAL_CONTENT_CHANGED",
                    "parse",
                    "当前文件与旧报告记录的内容不同，无法据此还原历史版本。",
                ));
            }
        }
        if let Some(file) = native.get(source.relative_path.as_deref().unwrap_or("")) {
            if let Some(classification) = &file.pdf_classification.0 {
                let classification = serde_json::to_value(classification)?;
                if !classification["page_audit"].is_null() {
                    audit.insert(
                        "pdf_page_audit".into(),
                        classification["page_audit"].clone(),
                    );
                    if classification["page_audit"]["observations"]
                        .as_array()
                        .is_some_and(|pages| pages.iter().any(|p| p["has_image"] == true))
                    {
                        tags.insert("PDF_OBSERVED_IMAGE".into());
                    }
                    if classification["status"] == "no_text_in_parse_window" && !source.unverified()
                    {
                        audit.insert("primary_reason".into(), json!("pdf_window_no_text"));
                        let ranges:Vec<_>=classification["page_audit"]["checked_pages"].as_array().into_iter().flatten().filter_map(Value::as_u64).map(|page|json!({"unit":"pdf_page","start":page,"end":page+1,"label":"classification"})).collect();
                        audit.insert("read_ranges".into(), json!(ranges));
                    }
                }
            }
            if source.extension.as_deref() == Some(".pdf") {
                if let Some(diagnostic) = &file.final_diagnostic.0 {
                    let code = serde_json::to_value(diagnostic.error_code)?;
                    tags.insert(
                        if code == "PDF_PROTECTED" {
                            "PDF_PROTECTED"
                        } else if code == "PARSER_TIMEOUT" {
                            "PDF_PROBE_TIMEOUT"
                        } else {
                            "PDF_PARSE_FAILED"
                        }
                        .into(),
                    );
                }
            }
        }
        audit.insert("secondary_tags".into(), json!(tags));
        source.coverage_limits["evidence_audit"] = Value::Object(audit);
        sources.push(source);
    }
    sources.sort_by(|a, b| a.source_id.cmp(&b.source_id));
    gaps.sort_by(|a, b| {
        (&a.code, &a.source_id, &a.relative_path, &a.stage).cmp(&(
            &b.code,
            &b.source_id,
            &b.relative_path,
            &b.stage,
        ))
    });
    gaps.dedup_by(|a, b| {
        a.code == b.code
            && a.source_id == b.source_id
            && a.relative_path == b.relative_path
            && a.stage == b.stage
    });
    Ok((sources, gaps))
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
