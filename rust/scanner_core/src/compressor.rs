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
            take_log_tail(&evidence.content, limit as usize)
        } else {
            take_head_and_tail(&evidence.content, limit as usize)
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

const OMITTED_MARKER_RESERVE: usize = 64;
const HEAD_RATIO_PER_MILLE: usize = 400;

fn omitted_marker(prefix: &str, omitted: u64) -> String {
    format!("…（已省略{prefix}约 {omitted} 字符）…")
}

fn newline_boundary_at_or_before(content: &str, position: usize) -> usize {
    content
        .chars()
        .take(position)
        .enumerate()
        .filter(|(_, character)| *character == '\n')
        .map(|(index, _)| index + 1)
        .last()
        .unwrap_or(position)
}

fn newline_boundary_at_or_after(content: &str, position: usize) -> usize {
    match content.chars().enumerate().skip(position).find(|(_, character)| *character == '\n') {
        Some((index, _)) => index + 1,
        None => position,
    }
}

/// 头+尾逐字保留：头 40% + 尾 60%，切点回退/前进到行边界（区域内无换行时
/// 按字符截断），中缝插入省略标记。边界移动只会缩短头/尾，因此
/// `count_chars(body) <= limit` 结构性成立（marker ≤ 64 预留）。
fn take_head_and_tail(content: &str, limit: usize) -> String {
    let total = count_chars(content);
    let available = limit.saturating_sub(OMITTED_MARKER_RESERVE).max(1);
    let head_budget = available * HEAD_RATIO_PER_MILLE / 1000;
    let tail_budget = available - head_budget;
    let head_end = newline_boundary_at_or_before(content, head_budget);
    let tail_start = newline_boundary_at_or_after(content, total as usize - tail_budget);
    let head = content.chars().take(head_end).collect::<String>();
    let tail = content.chars().skip(tail_start).collect::<String>();
    let omitted = total - count_chars(&head) - count_chars(&tail);
    format!("{head}{}{tail}", omitted_marker("中部", omitted))
}

/// `.log` 尾部优先：保留最后 `limit - 64` 字符（逐字后缀），前缀头部省略标记。
fn take_log_tail(content: &str, limit: usize) -> String {
    let total = count_chars(content);
    let available = limit.saturating_sub(OMITTED_MARKER_RESERVE).max(1);
    let tail_start = (total as usize).saturating_sub(available);
    let tail = content.chars().skip(tail_start).collect::<String>();
    let omitted = total - count_chars(&tail);
    format!("{}{tail}", omitted_marker("头部", omitted))
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
    use ai_daily_scanner_contract::{AuditWorkerLane, CacheStatus, ContextProfile, ParseStatus};

    fn head_tail_evidence(path: &str, extension: &str, content: &str) -> ContextFileEvidence {
        ContextFileEvidence {
            file_identity: format!("fixture:{path}"),
            absolute_path: format!("C:\\fixture\\{}", path.replace('/', "\\")),
            relative_path: path.replace('/', "\\"),
            extension: extension.to_string(),
            size_bytes: Some(content.len() as u64),
            content: content.to_string(),
            parser_backend: "light_text_v1".to_string(),
            worker_lane: AuditWorkerLane::RustCore,
            cache_status: CacheStatus::Miss,
            parse_status: ParseStatus::Success,
            truncated: false,
            error: None,
            reason: None,
        }
    }

    fn head_tail_profile(global_max_chars: u64, per_file_max_chars: u64) -> ContextProfile {
        ContextProfile {
            profile_name: "daily_balanced_v1".to_string(),
            global_max_chars,
            per_file_max_chars,
            small_file_max_bytes: 65_536,
            medium_file_max_bytes: 1_048_576,
            large_file_max_bytes: 10_485_760,
            priority_policy_version: "default_v1".to_string(),
            compression_policy_version: "markdown_context_v1".to_string(),
        }
    }

    #[test]
    fn boundary_helpers_respect_unicode_char_positions() {
        // 位置: 0甲 1\n 2乙 3丙 4\n 5丁
        let content = "甲\n乙丙\n丁";
        assert_eq!(newline_boundary_at_or_before(content, 6), 5);
        assert_eq!(newline_boundary_at_or_before(content, 2), 2);
        assert_eq!(newline_boundary_at_or_before(content, 0), 0);
        assert_eq!(newline_boundary_at_or_after(content, 0), 2);
        assert_eq!(newline_boundary_at_or_after(content, 3), 5);
        assert_eq!(newline_boundary_at_or_after(content, 5), 5);
    }

    #[test]
    fn head_tail_cuts_at_line_boundaries_and_counts_marker_chars() {
        // 8 行 × 50 字符/行 + 7 个换行 = 407 字符
        let content = (0..8)
            .map(|i| format!("第{i}行") + &"字".repeat(47))
            .collect::<Vec<_>>()
            .join("\n");
        let limit = 300_usize;
        let body = take_head_and_tail(&content, limit);

        assert!(count_chars(&body) <= limit as u64);
        // head = 第0行(50) + '\n' = 51 字符；tail = 第6行(50)+'\n'+第7行(50) = 101 字符
        assert!(body.starts_with(&format!("第0行{}\n", "字".repeat(47))));
        assert!(body.ends_with(&format!("第7行{}", "字".repeat(47))));
        // 省略 407 - 51 - 101 = 255
        assert!(body.contains("省略中部约 255 字符"));
        // 头部必须结束于行边界（标记紧随换行）
        let marker_index = body.find("…（已省略中部").expect("marker must exist");
        assert!(body[..marker_index].ends_with('\n'));
    }

    #[test]
    fn head_tail_keeps_partial_first_line_when_no_boundary_in_head_budget() {
        let content = format!("{}尾行", "字".repeat(300)); // 303 字符，第一行无换行
        let limit = 200_usize;
        let body = take_head_and_tail(&content, limit);
        assert!(count_chars(&body) <= limit as u64);
        assert!(body.starts_with(&"字".repeat(54))); // head_budget = (200-64)*40% = 54
        assert!(body.ends_with("尾行"));
    }

    #[test]
    fn log_tail_keeps_recent_content_with_head_marker() {
        let content = format!("{}{}RECENT_TAIL", "old-".repeat(199), "old"); // 810 字符（头部 799 + RECENT_TAIL 11）
        let limit = 300_usize;
        let body = take_log_tail(&content, limit);
        assert!(count_chars(&body) <= limit as u64);
        assert!(body.ends_with("RECENT_TAIL"));
        assert!(body.contains("省略头部约 574 字符")); // 810 - 236 = 574
        assert!(!body.contains(&"old-".repeat(60))); // 240 字符 > 236 尾部预算
    }

    #[test]
    fn build_context_renders_head_and_tail_for_long_file() {
        let content = (0..8)
            .map(|i| format!("第{i}行") + &"字".repeat(47))
            .collect::<Vec<_>>()
            .join("\n");
        let result = build_context(
            vec![head_tail_evidence("notes/long.md", ".md", &content)],
            &head_tail_profile(100_000, 300),
            ReportMode::Daily,
        )
        .expect("long file context");
        assert_eq!(result.decisions[0].decision.action, ContextAction::Compress);
        assert!(result.decisions[0].decision.truncated);
        assert!(result.decisions[0].decision.output_chars <= 300);
        assert!(result.content.contains("第0行"));
        assert!(result.content.contains("第7行"));
        assert!(result.content.contains("省略中部"));
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
        let real = render_run_summary(ReportMode::Daily, &profile, 999, "3", "1");
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
