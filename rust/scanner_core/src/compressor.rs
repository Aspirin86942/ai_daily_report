//! One-pass deterministic per-file and global context budgeting pipeline.

use ai_daily_scanner_contract::{
    ContextAction, ContextDecision, ContextProfile, ParseStatus, ReportMode, Validate,
};

use crate::budget_model::{budget_model_mismatch, count_chars};
use crate::decision::{decide_files, ContextFileEvidence, DecidedFile};

const GLOBAL_BUDGET_REASON: &str = "global_budget_exceeded";
const PARSE_FOOTER: &str = "## 解析问题\n- 未发现解析问题。";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetedDecision {
    pub file_identity: String,
    pub decision: ContextDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBuildOutput {
    pub content: String,
    pub source_file_count: u64,
    pub success_count: u64,
    pub timeout_count: u64,
    pub included_file_count: u64,
    pub omitted_file_count: u64,
    pub error_file_count: u64,
    pub input_chars: u64,
    pub output_chars: u64,
    pub metadata_only_count: u64,
    pub compressed_file_count: u64,
    pub truncated_file_count: u64,
    pub decisions: Vec<BudgetedDecision>,
}

pub fn build_context(
    evidence: Vec<ContextFileEvidence>,
    profile: &ContextProfile,
    report_mode: ReportMode,
) -> Result<ContextBuildOutput, String> {
    profile.validate()?;
    let decided = decide_files(evidence, profile)?;
    let source_file_count = decided.len() as u64;
    let success_count = decided
        .iter()
        .filter(|item| item.evidence.parse_status == ParseStatus::Success)
        .count() as u64;
    let timeout_count = decided
        .iter()
        .filter(|item| item.evidence.parse_status == ParseStatus::Timeout)
        .count() as u64;
    let error_file_count = source_file_count
        .checked_sub(success_count)
        .and_then(|value| value.checked_sub(timeout_count))
        .ok_or_else(|| "context status counts overflow".to_string())?;
    let input_chars = decided.iter().try_fold(0_u64, |total, item| {
        total
            .checked_add(item.decision.input_chars)
            .ok_or_else(|| "context input characters overflow".to_string())
    })?;

    let mut sections = vec![
        "# 文件证据上下文".to_string(),
        render_run_summary(
            report_mode,
            profile,
            source_file_count,
            success_count,
            timeout_count.saturating_add(error_file_count),
        ),
        render_notice(profile),
        "## 文件证据".to_string(),
    ];
    let mut decisions = Vec::with_capacity(decided.len());
    let omitted = Vec::new();
    let mut parse_issues = Vec::new();
    let mut included_file_count = 0_u64;
    let omitted_file_count = 0_u64;
    let mut metadata_only_count = 0_u64;
    let mut compressed_file_count = 0_u64;

    for item in decided {
        let DecidedFile {
            evidence,
            mut decision,
        } = item;
        if decision.action == ContextAction::Error {
            decision.output_chars = 0;
            let error_message = evidence
                .error
                .as_ref()
                .map_or("unknown_error", |error| error.message.as_str());
            parse_issues.push(format!(
                "- {} | reason={} | error={error_message}",
                decision.relative_path, decision.reason
            ));
            decisions.push(BudgetedDecision {
                file_identity: evidence.file_identity,
                decision,
            });
            continue;
        }

        let candidate = render_file_section(&evidence, &mut decision, profile);
        if can_append_with_footer(&sections, &candidate, profile.global_max_chars) {
            sections.push(candidate);
            included_file_count += 1;
            match decision.action {
                ContextAction::MetadataOnly => metadata_only_count += 1,
                ContextAction::Compress => compressed_file_count += 1,
                _ => {}
            }
        } else {
            // spec Part 2.2: 成功后因全局预算改 Omit 的兼容分支已删除。
            // 被准入文件的真实渲染必须满足 rendered <= reserved；违反是
            // 非重试 BUDGET_MODEL_MISMATCH 内部 Error，不 panic、不静默 Omit。
            return Err(budget_model_mismatch(
                "admitted file section exceeds the global context budget",
            ));
        }
        decisions.push(BudgetedDecision {
            file_identity: evidence.file_identity,
            decision,
        });
    }

    if included_file_count == 0 {
        append_if_fits(
            &mut sections,
            "无文件证据".to_string(),
            profile.global_max_chars,
        );
    }
    append_omitted_summary(&mut sections, &omitted, profile.global_max_chars);
    append_parse_issues(&mut sections, &parse_issues, profile.global_max_chars);

    let content = join_sections(&sections);
    if count_chars(&content) > profile.global_max_chars {
        return Err(budget_model_mismatch(
            "rendered context exceeds the global budget",
        ));
    }
    if content.is_empty() {
        return Err("context budget produced an empty result".to_string());
    }
    let output_chars = count_chars(&content);
    let truncated_file_count = decisions
        .iter()
        .filter(|record| record.decision.truncated)
        .count() as u64;

    Ok(ContextBuildOutput {
        content,
        source_file_count,
        success_count,
        timeout_count,
        included_file_count,
        omitted_file_count,
        error_file_count,
        input_chars,
        output_chars,
        metadata_only_count,
        compressed_file_count,
        truncated_file_count,
        decisions,
    })
}

fn render_run_summary(
    report_mode: ReportMode,
    profile: &ContextProfile,
    source_file_count: u64,
    success_count: u64,
    failure_count: u64,
) -> String {
    format!(
        "## 本轮摘要\n- 报告模式: {}\n- 压缩 profile: {}\n- 扫描文件数: {source_file_count}\n- 成功解析数: {success_count}\n- 失败解析数: {failure_count}\n- 全局上下文预算: {}\n- 单文件正文预算: {}\n- 压缩策略: {}",
        report_mode_text(report_mode),
        profile.profile_name,
        profile.global_max_chars,
        profile.per_file_max_chars,
        profile.compression_policy_version,
    )
}

fn render_notice(profile: &ContextProfile) -> String {
    format!(
        "## 重要提示\n- 以下内容来自本地 scanner 输出；Rust context core 不重新读取文件、不调用 LLM。\n- 正文块受单文件预算 {} 字符限制；超出全局预算的文件只保留审计摘要。",
        profile.per_file_max_chars
    )
}

fn render_file_section(
    evidence: &ContextFileEvidence,
    decision: &mut ContextDecision,
    profile: &ContextProfile,
) -> String {
    if decision.action == ContextAction::MetadataOnly {
        decision.output_chars = 0;
        return format!(
            "### {}\n- action: metadata_only\n- reason: {}\n- parser_backend: {}\n- worker_lane: {}\n- file_type: {}\n- size_bytes: {}\n- input_chars: {}\n- body: omitted_by_metadata_only_policy",
            decision.relative_path,
            decision.reason,
            evidence.parser_backend,
            enum_text(&evidence.worker_lane),
            evidence.extension,
            evidence.size_bytes.unwrap_or(0),
            decision.input_chars,
        );
    }

    let input_count = count_chars(&evidence.content);
    let limit = profile.per_file_max_chars;
    let body = if input_count > limit {
        decision.action = ContextAction::Compress;
        decision.truncated = true;
        if evidence.extension == ".log" {
            take_suffix_chars(&evidence.content, limit as usize)
        } else {
            take_prefix_chars(&evidence.content, limit as usize)
        }
    } else {
        evidence.content.clone()
    };
    decision.output_chars = count_chars(&body);
    let truncated_note = if decision.truncated {
        "\n- 内容已按单文件预算或解析预算截断"
    } else {
        ""
    };
    format!(
        "### {}\n- action: {}\n- reason: {}\n- parser_backend: {}\n- worker_lane: {}\n- input_chars: {}\n- output_chars: {}{}\n```text\n{}\n```",
        decision.relative_path,
        enum_text(&decision.action),
        decision.reason,
        evidence.parser_backend,
        enum_text(&evidence.worker_lane),
        decision.input_chars,
        decision.output_chars,
        truncated_note,
        body,
    )
}

fn append_omitted_summary(
    sections: &mut Vec<String>,
    omitted: &[(String, u64)],
    global_budget: u64,
) {
    if omitted.is_empty() {
        return;
    }
    let mut lines = vec![
        "## 省略文件摘要".to_string(),
        format!("- 省略文件数: {}", omitted.len()),
    ];
    for (path, input_chars) in omitted {
        let line = format!(
            "- {path} | action=omit | reason={GLOBAL_BUDGET_REASON} | input_chars={input_chars}"
        );
        let candidate = lines
            .iter()
            .chain(std::iter::once(&line))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        if can_append_with_footer(sections, &candidate, global_budget) {
            lines.push(line);
        } else {
            break;
        }
    }
    let section = lines.join("\n");
    if can_append_with_footer(sections, &section, global_budget) {
        sections.push(section);
    }
}

fn append_parse_issues(sections: &mut Vec<String>, issues: &[String], global_budget: u64) {
    let section = if issues.is_empty() {
        PARSE_FOOTER.to_string()
    } else {
        format!("## 解析问题\n{}", issues.join("\n"))
    };
    append_if_fits(sections, section, global_budget);
}

fn append_if_fits(sections: &mut Vec<String>, candidate: String, global_budget: u64) {
    let projected = join_sections(
        &sections
            .iter()
            .cloned()
            .chain(std::iter::once(candidate.clone()))
            .collect::<Vec<_>>(),
    );
    if count_chars(&projected) <= global_budget {
        sections.push(candidate);
    }
}

fn can_append_with_footer(sections: &[String], candidate: &str, global_budget: u64) -> bool {
    let projected = join_sections(
        &sections
            .iter()
            .cloned()
            .chain([candidate.to_string(), PARSE_FOOTER.to_string()])
            .collect::<Vec<_>>(),
    );
    count_chars(&projected) <= global_budget
}

fn join_sections(sections: &[String]) -> String {
    let body = sections
        .iter()
        .map(|section| section.trim_end())
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{}\n", body.trim_end())
}

fn take_prefix_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

fn take_suffix_chars(value: &str, count: usize) -> String {
    let total = count_chars(value) as usize;
    value.chars().skip(total.saturating_sub(count)).collect()
}

fn report_mode_text(report_mode: ReportMode) -> &'static str {
    match report_mode {
        ReportMode::Daily => "daily",
        ReportMode::Weekly => "weekly",
        ReportMode::Monthly => "monthly",
    }
}

fn enum_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .expect("contract enum must serialize to text")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_daily_scanner_contract::CacheStatus;

    #[test]
    fn character_helpers_preserve_unicode_boundaries() {
        assert_eq!(take_prefix_chars("甲乙丙", 2), "甲乙");
        assert_eq!(take_suffix_chars("甲乙丙", 2), "乙丙");
    }

    #[test]
    fn cache_status_serialization_is_contract_text() {
        assert_eq!(enum_text(&CacheStatus::Fresh), "fresh");
    }
}
