//! One-pass deterministic per-file and global context budgeting pipeline.

use std::collections::BTreeMap;

use ai_daily_scanner_contract::{ContextAction, ContextDecision, ParseStatus, ReportMode};

use crate::budget_model::{
    budget_model_mismatch, count_chars, OmittedCandidate, OmittedSummaryPlan, MAX_U64_DIGITS,
    SECTION_SEPARATOR_CHARS,
};
use crate::decision::{decide_files, BudgetProfile, ContextFileEvidence, DecidedFile};

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
    /// Rendered chars per file identity, used by the scheduler to enforce the
    /// `rendered_chars <= reserved_chars` budget-model invariant per file.
    pub rendered_by_identity: BTreeMap<String, u64>,
}

/// The fixed sections that exist before any admitted file (spec Part 1.3
/// `base_chars`). `success_count`/`failure_count` are rendered at their
/// worst-case digit length so the budget model prices the section before the
/// parse results are known; the real renderer never exceeds them.
pub fn fixed_context_sections(
    profile: &impl BudgetProfile,
    report_mode: ReportMode,
    source_file_count: u64,
) -> Vec<String> {
    vec![
        "# 文件证据上下文".to_string(),
        render_run_summary(
            report_mode,
            profile,
            source_file_count,
            &"9".repeat(MAX_U64_DIGITS as usize),
            &"9".repeat(MAX_U64_DIGITS as usize),
        ),
        render_notice(profile),
        "## 文件证据".to_string(),
    ]
}

pub fn build_context(
    evidence: Vec<ContextFileEvidence>,
    profile: &impl BudgetProfile,
    report_mode: ReportMode,
) -> Result<ContextBuildOutput, String> {
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
    let error_file_count = decided
        .iter()
        .filter(|item| item.evidence.parse_status == ParseStatus::Error)
        .count() as u64;
    let not_parsed_count = source_file_count
        .checked_sub(success_count)
        .and_then(|value| value.checked_sub(timeout_count))
        .and_then(|value| value.checked_sub(error_file_count))
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
            &success_count.to_string(),
            &timeout_count.saturating_add(error_file_count).to_string(),
        ),
        render_notice(profile),
        "## 文件证据".to_string(),
    ];
    let mut decisions = Vec::with_capacity(decided.len());
    let mut rendered_by_identity = BTreeMap::new();
    let mut parse_issues = Vec::new();
    let mut omitted_files = Vec::new();
    let mut included_file_count = 0_u64;
    let mut metadata_only_count = 0_u64;
    let mut compressed_file_count = 0_u64;

    let omitted_candidates: Vec<OmittedCandidate> = decided
        .iter()
        .map(|item| OmittedCandidate {
            file_identity: item.evidence.file_identity.clone(),
            relative_path: item.evidence.relative_path.clone(),
            extension: item.evidence.extension.clone(),
        })
        .collect();
    let omitted_plan = OmittedSummaryPlan::build(&omitted_candidates, profile.global_max_chars());

    for item in decided {
        let DecidedFile {
            evidence,
            mut decision,
        } = item;
        match decision.action {
            ContextAction::Omit => {
                decision.output_chars = 0;
                omitted_files.push(OmittedRow {
                    relative_path: decision.relative_path.clone(),
                    extension: evidence.extension.clone(),
                    reason: decision.reason.clone(),
                    input_chars: decision.input_chars,
                    in_detail_slot: omitted_plan
                        .detail_slots
                        .iter()
                        .any(|slot| slot.file_identity == evidence.file_identity),
                });
                decisions.push(BudgetedDecision {
                    file_identity: evidence.file_identity,
                    decision,
                });
                continue;
            }
            ContextAction::Error => {
                let error_code = evidence
                    .error
                    .as_ref()
                    .map(|error| enum_text(&error.error_code))
                    .unwrap_or_else(|| "UNKNOWN_ERROR".to_string());
                // spec Part 1.3: file_context renders ONLY the contract-bounded
                // error code, never the arbitrary Diagnostic message.
                let line = format!(
                    "- {} | reason={} | error={error_code}",
                    decision.relative_path, decision.reason
                );
                let rendered_chars = count_chars(&line) + 1; // trailing newline
                decision.output_chars = rendered_chars;
                parse_issues.push(line);
                rendered_by_identity.insert(evidence.file_identity.clone(), rendered_chars);
                decisions.push(BudgetedDecision {
                    file_identity: evidence.file_identity,
                    decision,
                });
                continue;
            }
            _ => {}
        }

        let candidate = render_file_section(&evidence, &mut decision, profile);
        let candidate_chars = count_chars(&candidate);
        if can_append_with_footer(&sections, &candidate, profile.global_max_chars()) {
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
        rendered_by_identity.insert(evidence.file_identity.clone(), candidate_chars);
        decisions.push(BudgetedDecision {
            file_identity: evidence.file_identity,
            decision,
        });
    }

    if included_file_count == 0 {
        append_if_fits(
            &mut sections,
            "无文件证据".to_string(),
            profile.global_max_chars(),
        );
    }
    append_omitted_summary(
        &mut sections,
        &omitted_files,
        omitted_plan,
        profile.global_max_chars(),
    )?;
    append_parse_issues(&mut sections, &parse_issues, profile.global_max_chars());

    let content = join_sections(&sections);
    if count_chars(&content) > profile.global_max_chars() {
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
    let omitted_file_count = not_parsed_count;

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
        rendered_by_identity,
    })
}

struct OmittedRow {
    relative_path: String,
    extension: String,
    reason: String,
    input_chars: u64,
    in_detail_slot: bool,
}

fn render_run_summary(
    report_mode: ReportMode,
    profile: &impl BudgetProfile,
    source_file_count: u64,
    success_text: &str,
    failure_text: &str,
) -> String {
    format!(
        "## 本轮摘要\n- 报告模式: {}\n- 压缩 profile: {}\n- 扫描文件数: {source_file_count}\n- 成功解析数: {success_text}\n- 失败解析数: {failure_text}\n- 全局上下文预算: {}\n- 单文件正文预算: {}\n- 压缩策略: {}",
        report_mode_text(report_mode),
        profile.profile_name(),
        profile.global_max_chars(),
        profile.per_file_max_chars(),
        profile.compression_policy_version(),
    )
}

fn render_notice(profile: &impl BudgetProfile) -> String {
    format!(
        "## 重要提示\n- 以下内容来自本地 scanner 输出；Rust context core 不重新读取文件、不调用 LLM。\n- 正文块受单文件预算 {} 字符限制；超出全局上下文预算的文件由确定性准入计划省略，并在省略摘要中汇总。",
        profile.per_file_max_chars()
    )
}

fn render_file_section(
    evidence: &ContextFileEvidence,
    decision: &mut ContextDecision,
    profile: &impl BudgetProfile,
) -> String {
    if decision.action == ContextAction::MetadataOnly {
        decision.output_chars = 0;
        return format!(
            "### {}\n- action: metadata_only\n- reason: {}\n- parser_backend: {}\n- worker_lane: {}\n- file_type: {}\n- size_bytes: {}\n- input_chars: ~{}\n- body: omitted_by_metadata_only_policy",
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
    let limit = profile.per_file_max_chars();
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

/// Renders the omitted summary from the pre-selected detail slots (spec
/// Part 1.3). Detail rows render only for files that are actually omitted AND
/// have a pre-selected slot (no backfill); then aggregate rows render in
/// `(reason, extension)` canonical order and overflow groups fold into the
/// single catch-all row. The whole section must stay inside the reservation.
fn append_omitted_summary(
    sections: &mut Vec<String>,
    omitted: &[OmittedRow],
    plan: OmittedSummaryPlan,
    global_budget: u64,
) -> Result<(), String> {
    if omitted.is_empty() {
        return Ok(());
    }
    let mut lines = vec![
        "## 省略文件摘要".to_string(),
        format!("- 省略文件数: {}", omitted.len()),
    ];
    let mut used = count_chars(&lines.join("\n")) + SECTION_SEPARATOR_CHARS;
    // Detail rows: only omitted files that were pre-selected as detail slots.
    for row in omitted {
        if !row.in_detail_slot {
            continue;
        }
        let line = format!(
            "- {} | action=omit | reason={} | input_chars=~{}",
            row.relative_path, row.reason, row.input_chars
        );
        let candidate_used = used.saturating_add(count_chars(&line) + 1);
        if candidate_used <= plan.reservation {
            lines.push(line);
            used = candidate_used;
        } else {
            return Err(budget_model_mismatch(
                "pre-selected omitted detail exceeds its reservation",
            ));
        }
    }
    // Aggregate rows in (reason, extension) canonical order.
    let mut groups: BTreeMap<(&str, &str), u64> = BTreeMap::new();
    for row in omitted {
        *groups
            .entry((row.reason.as_str(), row.extension.as_str()))
            .or_insert(0) += 1;
    }
    let mut other_count = 0_u64;
    for ((reason, extension), count) in groups {
        let line = format!("- {reason} | {extension} | action=omit | count={count}");
        let candidate_used = used.saturating_add(count_chars(&line) + 1);
        if candidate_used <= plan.reservation {
            lines.push(line);
            used = candidate_used;
        } else {
            other_count = other_count.saturating_add(count);
        }
    }
    if other_count > 0 {
        let line = format!("- 其他 | action=omit | count={other_count}");
        let candidate_used = used.saturating_add(count_chars(&line) + 1);
        if candidate_used <= plan.reservation {
            lines.push(line);
        } else {
            return Err(budget_model_mismatch(
                "mandatory omitted catch-all exceeds its reservation",
            ));
        }
    }
    let section = lines.join("\n");
    if count_chars(&section) > plan.reservation {
        return Err(budget_model_mismatch(
            "omitted summary exceeds its reservation",
        ));
    }
    if !can_append_with_footer(sections, &section, global_budget) {
        return Err(budget_model_mismatch(
            "omitted summary exceeds the global context budget",
        ));
    }
    sections.push(section);
    Ok(())
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
    use crate::budget_model::{max_omitted_row_chars, BUDGET_MODEL_MISMATCH_CODE};
    use ai_daily_scanner_contract::CacheStatus;
    use ai_daily_scanner_contract::ContextProfile;

    #[test]
    fn character_helpers_preserve_unicode_boundaries() {
        assert_eq!(take_prefix_chars("甲乙丙", 2), "甲乙");
        assert_eq!(take_suffix_chars("甲乙丙", 2), "乙丙");
    }

    #[test]
    fn cache_status_serialization_is_contract_text() {
        assert_eq!(enum_text(&CacheStatus::Fresh), "fresh");
    }

    #[test]
    fn fixed_sections_price_counts_at_max_digits() {
        let profile = ContextProfile {
            profile_name: "daily_balanced_v1".to_string(),
            global_max_chars: 50_000,
            per_file_max_chars: 8_000,
            small_file_max_bytes: 65_536,
            medium_file_max_bytes: 1_048_576,
            large_file_max_bytes: 10_485_760,
            priority_policy_version: "default_v1".to_string(),
            compression_policy_version: "markdown_context_v1".to_string(),
        };
        let worst = fixed_context_sections(&profile, ReportMode::Daily, 999);
        let worst_chars = worst
            .iter()
            .map(|section| count_chars(section) + SECTION_SEPARATOR_CHARS)
            .sum::<u64>();
        // The real render with concrete counts never exceeds the worst-case.
        let real = render_run_summary(
            ReportMode::Daily,
            &profile,
            999,
            &"3".to_string(),
            &"1".to_string(),
        );
        let real_chars = count_chars(&real) + SECTION_SEPARATOR_CHARS;
        assert!(real_chars <= worst_chars);
    }

    #[test]
    fn max_omitted_row_chars_is_used_by_the_renderer() {
        // The detail-row format matches `max_omitted_row_chars` so the
        // OmittedSummaryPlan reservation covers the actual render.
        let row = format!(
            "- {} | action=omit | reason={} | input_chars=~{}",
            "\\deep\\path\\file.md", "pdf_text_extraction_quota_exhausted", 9_u64,
        );
        assert!(
            count_chars(&row) <= max_omitted_row_chars("\\deep\\path\\file.md"),
            "rendered omitted row must fit the priced max"
        );
    }

    #[test]
    fn omitted_summary_fails_closed_when_preselected_detail_does_not_fit() {
        let mut sections = vec!["base".to_string()];
        let original = sections.clone();
        let omitted = vec![OmittedRow {
            relative_path: "too-long.md".to_string(),
            extension: ".md".to_string(),
            reason: "semantic_file_quota_exhausted".to_string(),
            input_chars: 1,
            in_detail_slot: true,
        }];

        let error = append_omitted_summary(
            &mut sections,
            &omitted,
            OmittedSummaryPlan {
                reservation: 1,
                detail_slots: Vec::new(),
            },
            10_000,
        )
        .expect_err("a pre-selected detail row must never disappear silently");

        assert!(error.contains(BUDGET_MODEL_MISMATCH_CODE));
        assert_eq!(sections, original);
    }

    #[test]
    fn omitted_summary_fails_closed_when_mandatory_catch_all_does_not_fit() {
        let mut sections = vec!["base".to_string()];
        let omitted = vec![OmittedRow {
            relative_path: "omitted.md".to_string(),
            extension: ".md".to_string(),
            reason: "semantic_file_quota_exhausted".to_string(),
            input_chars: 1,
            in_detail_slot: false,
        }];

        let error = append_omitted_summary(
            &mut sections,
            &omitted,
            OmittedSummaryPlan {
                reservation: 1,
                detail_slots: Vec::new(),
            },
            10_000,
        )
        .expect_err("the mandatory catch-all must never disappear silently");

        assert!(error.contains(BUDGET_MODEL_MISMATCH_CODE));
        assert!(error.contains("catch-all"));
    }

    #[test]
    fn omitted_summary_fails_closed_when_global_append_does_not_fit() {
        let mut sections = vec!["base".to_string()];
        let original = sections.clone();
        let omitted = vec![OmittedRow {
            relative_path: "omitted.md".to_string(),
            extension: ".md".to_string(),
            reason: "semantic_file_quota_exhausted".to_string(),
            input_chars: 1,
            in_detail_slot: false,
        }];

        let error = append_omitted_summary(
            &mut sections,
            &omitted,
            OmittedSummaryPlan {
                reservation: 1_000,
                detail_slots: Vec::new(),
            },
            1,
        )
        .expect_err("the complete omitted summary must fit the global budget");

        assert!(error.contains(BUDGET_MODEL_MISMATCH_CODE));
        assert_eq!(sections, original);
    }
}
