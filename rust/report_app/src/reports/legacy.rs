use super::data::{CoverageGap, ReportSource};
use crate::{
    model::legacy::{LegacyItem, LegacySummary},
    types::{parse_date, utc_window},
    ReportError,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub fn render(report: &Value, template: &str) -> Result<String, ReportError> {
    let error = || ReportError::new("REPORT_DATA_INVALID", "旧报告结构或来源引用无效");
    let day = parse_date(report["date"].as_str().ok_or_else(error)?)?;
    let (start, end) = utc_window(day)?;
    for (key, expected) in [("window_start_utc", start), ("window_end_utc", end)] {
        let stamp = DateTime::parse_from_rfc3339(report[key].as_str().ok_or_else(error)?)
            .map_err(|_| error())?;
        if stamp.offset().local_minus_utc() != 0 || stamp.with_timezone(&Utc) != expected {
            return Err(error());
        }
    }
    let sources: Vec<ReportSource> = serde_json::from_value(report["sources"].clone())?;
    let gaps: Vec<CoverageGap> = serde_json::from_value(report["gaps"].clone())?;
    let summary: LegacySummary = serde_json::from_value(
        json!({"summary_items":report["summary_items"],"verification_items":report["verification_items"]}),
    )?;
    let known: BTreeSet<_> = sources.iter().map(|s| s.source_id.as_str()).collect();
    if report["schema_version"] != 2
        || report["report_kind"] != "file_evidence"
        || report["scope_id"] != "personal_work"
        || report["timezone"] != "Asia/Shanghai"
        || known.len() != sources.len()
        || summary.summary_items.len() > 2048
        || summary.verification_items.len() > 4096
    {
        return Err(error());
    }
    for source in &sources {
        source.validate()?;
    }
    for item in summary
        .summary_items
        .iter()
        .chain(&summary.verification_items)
    {
        validate_item(item, &known)?;
    }
    let quality = report["quality"].as_str().ok_or_else(error)?;
    if quality == "no_changes"
        && (!sources.is_empty()
            || !gaps.is_empty()
            || !summary.summary_items.is_empty()
            || !summary.verification_items.is_empty())
        || quality == "complete"
            && (!gaps.is_empty()
                || sources.iter().any(|s| {
                    s.parse_status != "success"
                        || s.model_truncated
                        || s.excerpt_truncated
                        || s.parse_truncated
                }))
    {
        return Err(error());
    }
    let label = match quality {
        "complete" => "完整覆盖",
        "partial" => "部分覆盖",
        "no_changes" => "未发现目标日期内修改的文件",
        _ => return Err(error()),
    };
    let generated =
        DateTime::parse_from_rfc3339(report["generated_at"].as_str().ok_or_else(error)?)
            .map_err(|_| error())?;
    let labels: BTreeMap<_, _> = sources
        .iter()
        .enumerate()
        .map(|(i, s)| (&s.source_id, format!("S{:03}", i + 1)))
        .collect();
    let presented: BTreeSet<_> = summary
        .summary_items
        .iter()
        .chain(&summary.verification_items)
        .flat_map(|i| i.source_ids.iter())
        .collect();
    let files: Vec<_> = sources.iter().filter(|s| s.source_kind == "file").collect();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
    let mut reason_labels: BTreeMap<String, usize> = BTreeMap::new();
    let mut covered = 0;
    for source in &files {
        let audit = &source.coverage_limits["evidence_audit"];
        let reason = source.reason.as_deref().unwrap_or("");
        let status = if source.parse_status == "not_parsed" {
            "not_parsed"
        } else if source.parse_status != "success"
            || source.excerpt_chars > 0 && source.content_sha256.is_none()
        {
            "parse_failed"
        } else if source.excerpt_chars > 0 && audit["usable_chars"] != 0 {
            "usable_excerpt"
        } else {
            "empty_excerpt"
        };
        *counts.entry(status).or_default() += 1;
        if status == "usable_excerpt" && source.model_chars > 0 {
            covered += 1;
        }
        let primary = if status == "usable_excerpt" {
            None
        } else if let Some(primary) = audit["primary_reason"].as_str() {
            Some(primary)
        } else if status == "parse_failed" {
            Some(if source.parse_status == "timeout" {
                "parse_timeout"
            } else if reason.contains("unstable") {
                "source_unstable"
            } else {
                "parser_failed"
            })
        } else {
            Some(if status == "empty_excerpt" {
                "no_business_content"
            } else {
                "unknown"
            })
        };
        if let Some(primary) = primary {
            *reasons.entry(primary.into()).or_default() += 1;
            *reason_labels
                .entry(reason_label(primary).into())
                .or_default() += 1;
        }
    }
    let count = |key| counts.get(key).copied().unwrap_or(0);
    let coverage = json!({"registered_count":files.len(),"usable_excerpt_count":count("usable_excerpt"),"not_parsed_count":count("not_parsed"),"parse_failed_count":count("parse_failed"),"empty_excerpt_count":count("empty_excerpt"),"model_covered_count":covered,"presented_count":files.iter().filter(|s|presented.contains(&s.source_id)).count(),"user_statement_count":sources.len()-files.len(),"primary_reasons":reasons,"primary_reason_labels":reason_labels,"actual_ranges_known_count":files.iter().filter(|s|!s.coverage_limits["evidence_audit"]["read_ranges"].is_null()).count()});
    let mut view = report.clone();
    view["source_labels"] = json!(labels);
    view["quality_label"] = json!(label);
    view["is_historical_reconstruction"] = json!(
        generated
            .with_timezone(&crate::types::shanghai())
            .date_naive()
            > day + chrono::Duration::days(1)
    );
    view["coverage_summary"] = coverage;
    let body = super::render(template, &view, "")?;
    Ok(format!(
        "{}\n",
        body.replace("\r\n", "\n").replace('\r', "\n").trim_end()
    ))
}

pub fn validate_item(item: &LegacyItem, known: &BTreeSet<&str>) -> Result<(), ReportError> {
    let ids: BTreeSet<_> = item.source_ids.iter().map(String::as_str).collect();
    if item.text.trim().is_empty()
        || item.text.chars().count() > 4000
        || ids.is_empty()
        || ids.len() > 64
        || ids.len() != item.source_ids.len()
        || !ids.is_subset(known)
    {
        return Err(ReportError::new(
            "REPORT_SOURCE_REFERENCE_INVALID",
            "旧报告引用无效",
        ));
    }
    Ok(())
}
fn reason_label(reason: &str) -> &str {
    match reason {
        "candidate_file_budget" => "候选文件名额耗尽",
        "pdf_classification_page_quota_exhausted" => "PDF 分类页额耗尽",
        "pdf_text_extraction_quota_exhausted" => "PDF 正文提取名额耗尽",
        "global_context_budget_exceeded" => "上下文预算耗尽",
        "excerpt_character_budget" => "摘录字符预算耗尽",
        "file_too_large" => "文件体积超限",
        "unsupported_format" => "不支持的格式",
        "source_unstable" => "来源读取期间变化",
        "parse_timeout" => "解析超时",
        "parser_failed" => "解析失败",
        "pdf_window_no_text" => "PDF 已读窗口无字",
        "empty_document" => "有效空文档",
        "no_business_content" => "没有有效业务文字",
        _ => "原因未知",
    }
}
