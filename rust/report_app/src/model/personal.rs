use super::plan::{payload, PlannedStep, StepKind};
use crate::{
    config::Llm,
    reports::personal::{
        body_chars, validate_items, ItemKind, PersonalItem, Selection, StatementKind,
        MAX_BODY_CHARS, MAX_ITEMS, MAX_SUMMARY_CHARS, MAX_SUMMARY_CHARS_EXTENDED,
        MAX_SUMMARY_ITEMS,
    },
    snapshots::{canonical_json, digest},
    types::RunContext,
    ReportError,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const LEGACY_PROMPT: &str = include_str!("../../../../templates/personal_daily_prompt_v5.md");
pub const PROMPT: &str = include_str!("../../../../templates/personal_daily_prompt_v5_compact.md");
pub const SCAN_PROMPT: &str =
    include_str!("../../../../templates/personal_daily_scan_prompt_v5.md");
pub const SCAN_COMPACT_PROMPT: &str =
    include_str!("../../../../templates/personal_daily_scan_prompt_v6.md");
pub const COMPACT_PROMPT: &str =
    include_str!("../../../../templates/personal_daily_compact_prompt_v6.md");
/// Prompt versions 6/7 keep the 420–560 target but allow the 1,500-character ceiling.
pub const SCAN_PROMPT_EXTENDED: &str =
    include_str!("../../../../templates/personal_daily_scan_prompt_v7.md");
pub const COMPACT_PROMPT_EXTENDED: &str =
    include_str!("../../../../templates/personal_daily_compact_prompt_v7.md");

pub fn schema() -> Value {
    json!({"schema_version":5,"items":[{"title":"简短标题","text":"事项正文",
        "kind":"progress|attention|next_step","statement_kind":"observed|user_statement|uncertain|suggestion","source_ids":["输入来源编号"]}]})
}

pub fn prepare(
    llm: &Llm,
    context: &RunContext,
    selection: &mut Selection,
) -> Result<PlannedStep, ReportError> {
    prepare_version(llm, context, selection, 2)
}

pub fn prepare_scan(
    llm: &Llm,
    context: &RunContext,
    selection: &mut Selection,
) -> Result<PlannedStep, ReportError> {
    prepare_version(llm, context, selection, 6)
}

fn summary_limit(prompt_version: u32) -> usize {
    if prompt_version >= 6 {
        MAX_SUMMARY_CHARS_EXTENDED
    } else {
        MAX_SUMMARY_CHARS
    }
}

/// Character ceiling enforced by a response version. Versions 4/5 keep the
/// historical 600-character limits; versions 6/7 allow the 1,500-character body.
fn summary_cap(prompt_version: Option<u64>) -> Option<usize> {
    match prompt_version {
        Some(4 | 5) => Some(MAX_SUMMARY_CHARS),
        Some(6 | 7) => Some(MAX_SUMMARY_CHARS_EXTENDED),
        _ => None,
    }
}

fn prepare_version(
    llm: &Llm,
    context: &RunContext,
    selection: &mut Selection,
    prompt_version: u32,
) -> Result<PlannedStep, ReportError> {
    loop {
        selection.validate()?;
        let mut step = PlannedStep {
            version: 6,
            kind: StepKind::PersonalSummary,
            logical_step: "personal_summary".into(),
            revision: 0,
            parent_sha256: None,
            prompt_sha256: String::new(),
            schema_sha256: String::new(),
            target: json!({"date":context.target_date,"window_start_utc":context.window_start_utc,
                "window_end_utc":context.window_end_utc,"selection":selection,
                "summary_prompt_version":prompt_version,
                "summary_limits":{"max_chars":summary_limit(prompt_version),"max_items":MAX_SUMMARY_ITEMS}}),
            entries: vec![],
            facts: vec![],
            source_map: json!(selection
                .materials
                .iter()
                .map(|m| (m.source.source_id.clone(), m.source.clone()))
                .collect::<BTreeMap<_, _>>()),
            coverage: json!({}),
            output_capacity_bytes: 100_000,
            request: Value::Null,
        };
        rebuild(&mut step, llm)?;
        match step.validate(llm) {
            Ok(()) => return Ok(step),
            Err(error) if error.code == "MODEL_REQUEST_TOO_LARGE" => {
                let remove = selection
                    .materials
                    .iter()
                    .rposition(|m| m.source.source_id != "user_input")
                    .ok_or(error)?;
                let material = selection.materials.remove(remove);
                selection.summary.selected_chars -= material.text.chars().count();
                selection.summary.omit(
                    "budget",
                    material.source.relative_path.as_deref().unwrap_or(""),
                );
                selection.summary.selected_files = selection
                    .materials
                    .iter()
                    .filter_map(|m| m.source.relative_path.as_ref())
                    .collect::<std::collections::BTreeSet<_>>()
                    .len() as u64;
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) fn rebuild(step: &mut PlannedStep, llm: &Llm) -> Result<(), ReportError> {
    let selection: Selection = serde_json::from_value(step.target["selection"].clone())?;
    selection.validate()?;
    let sources: BTreeMap<_, _> = selection
        .materials
        .iter()
        .map(|m| (m.source.source_id.clone(), m.source.clone()))
        .collect();
    if step.source_map != serde_json::to_value(sources)?
        || !step.entries.is_empty()
        || !step.facts.is_empty()
    {
        return Err(ReportError::new(
            "MODEL_PLAN_IDENTITY_MISMATCH",
            "个人日报来源与冻结材料不一致",
        ));
    }
    let prompt = match step.target.get("summary_prompt_version") {
        None => LEGACY_PROMPT,
        Some(version) if version == &json!(2) => PROMPT,
        Some(version) if version == &json!(3) => SCAN_PROMPT,
        Some(version) if version == &json!(4) => SCAN_COMPACT_PROMPT,
        Some(version) if version == &json!(5) => COMPACT_PROMPT,
        Some(version) if version == &json!(6) => SCAN_PROMPT_EXTENDED,
        Some(version) if version == &json!(7) => COMPACT_PROMPT_EXTENDED,
        _ => {
            return Err(ReportError::new(
                "MODEL_PLAN_INVALID",
                "个人日报提示词版本无效",
            ))
        }
    };
    step.prompt_sha256 = digest(prompt.as_bytes());
    let mut response_schema = schema();
    let prompt_version = step.target["summary_prompt_version"].as_u64();
    if matches!(prompt_version, Some(5 | 7)) {
        response_schema["items"][0]["item_id"] = json!(1);
    }
    step.schema_sha256 = digest(&canonical_json(&response_schema)?);
    let sources: Vec<_> = selection.materials.iter().map(|m| json!({
        "source_id":m.source.source_id,"path":m.source.relative_path,"source_kind":m.source.source_kind,
        "text_unverified":m.source.unverified(),"text":m.text,
        "parts":m.evidence.iter().map(|e| json!({"side":e.side,"start":e.input_start,"end":e.input_end,"ranges":e.ranges})).collect::<Vec<_>>()
    })).collect();
    let input = if matches!(prompt_version, Some(5 | 7)) {
        let draft: Vec<PersonalItem> =
            serde_json::from_value(step.target["summary_draft"].clone())?;
        validate_items(
            &draft,
            &selection
                .materials
                .iter()
                .map(|m| m.source.clone())
                .collect::<Vec<_>>(),
        )?;
        if draft.is_empty() || draft.len() > MAX_SUMMARY_ITEMS {
            return Err(super::contracts::invalid());
        }
        let mut identified = serde_json::to_value(&draft)?;
        for (index, item) in identified.as_array_mut().unwrap().iter_mut().enumerate() {
            item["item_id"] = json!(index + 1);
        }
        json!({"date":step.target["date"],"draft_items":identified,
            "draft_chars":body_chars(&draft),
            "max_chars":summary_cap(prompt_version).unwrap_or(MAX_SUMMARY_CHARS),
            "target_chars":560})
    } else {
        json!({"date":step.target["date"],"sources":sources})
    };
    step.request = payload(
        llm,
        format!("{prompt}\n{response_schema}"),
        input,
        llm.max_tokens
            .min(llm.verified_max_output_tokens.unwrap_or(llm.max_tokens)),
    )?;
    Ok(())
}

#[derive(Debug)]
pub struct PersonalOutput {
    pub items: Vec<PersonalItem>,
    pub omitted: usize,
    pub compaction_error: Option<&'static str>,
}

/// Keep the validated draft until a separately audited compaction succeeds.
pub fn generate(
    runner: &super::ModelRunner<'_>,
    plan: &PlannedStep,
) -> Result<PersonalOutput, ReportError> {
    let super::StepOutput::Personal(mut output) = runner.execute(plan)?.output else {
        return Err(super::contracts::invalid());
    };
    let Some(cap) = summary_cap(plan.target["summary_prompt_version"].as_u64()) else {
        return Ok(output);
    };
    if !matches!(plan.target["summary_prompt_version"].as_u64(), Some(4 | 6))
        || body_chars(&output.items) <= cap
    {
        return Ok(output);
    }
    let compact_version = if plan.target["summary_prompt_version"] == json!(6) {
        7
    } else {
        5
    };
    let compacted = (|| {
        let mut compact = plan.clone();
        compact.logical_step = format!("{}.compact", plan.logical_step);
        compact.parent_sha256 = Some(plan.identity()?);
        compact.target["summary_prompt_version"] = json!(compact_version);
        compact.target["summary_draft"] = serde_json::to_value(&output.items)?;
        rebuild(&mut compact, runner.llm)?;
        compact.validate(runner.llm)?;
        let super::StepOutput::Personal(result) = runner.execute(&compact)?.output else {
            return Err(super::contracts::invalid());
        };
        Ok(result)
    })();
    match compacted {
        Ok(result) => Ok(result),
        Err(error) => {
            let selection: Selection = serde_json::from_value(plan.target["selection"].clone())?;
            while body_chars(&output.items) > cap {
                if output.items.len() <= 1 || !omit_lowest_priority(&mut output.items, &selection) {
                    return Err(error);
                }
                output.omitted += 1;
            }
            // A whole draft that already fits the ceiling stays complete: no gap is recorded.
            if output.omitted > 0 {
                output.compaction_error = Some(error.code);
            }
            Ok(output)
        }
    }
}

/// Preserve action items and the original display order during last-resort omission.
pub(crate) fn omit_lowest_priority(items: &mut Vec<PersonalItem>, selection: &Selection) -> bool {
    let priorities: BTreeMap<_, _> = selection
        .materials
        .iter()
        .map(|m| (&m.source.source_id, m.priority))
        .collect();
    let candidate = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.kind != ItemKind::NextStep)
        .max_by_key(|(index, item)| {
            (
                item.source_ids
                    .iter()
                    .filter_map(|id| priorities.get(id))
                    .min()
                    .copied()
                    .unwrap_or(255),
                *index,
            )
        })
        .map(|(index, _)| index);
    if let Some(index) = candidate {
        items.remove(index);
        true
    } else {
        false
    }
}

pub(super) fn validate_response(
    step: &PlannedStep,
    mut value: Value,
) -> Result<PersonalOutput, ReportError> {
    if step.kind != StepKind::PersonalSummary || step.version != 6 {
        return Err(super::contracts::invalid());
    }
    // The frozen personal request fixes the response format. Some providers omit
    // its version tag; explicit versions and all content still validate below.
    if let Some(object) = value.as_object_mut() {
        object.entry("schema_version").or_insert(json!(5));
    }
    let prompt_version = step.target["summary_prompt_version"].as_u64();
    if matches!(prompt_version, Some(5 | 7)) {
        let items = value["items"]
            .as_array_mut()
            .ok_or_else(super::contracts::invalid)?;
        for (index, item) in items.iter_mut().enumerate() {
            if item.as_object_mut().and_then(|item| item.remove("item_id"))
                != Some(json!(index + 1))
            {
                return Err(super::contracts::invalid());
            }
        }
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Response {
        schema_version: u32,
        items: Vec<PersonalItem>,
    }
    let mut response: Response =
        serde_json::from_value(value).map_err(|_| super::contracts::invalid())?;
    if response.schema_version != 5 || response.items.is_empty() {
        return Err(super::contracts::invalid());
    }
    let selection: Selection = serde_json::from_value(step.target["selection"].clone())?;
    let sources: Vec<_> = selection
        .materials
        .iter()
        .map(|m| m.source.clone())
        .collect();
    let unverified: BTreeSet<_> = sources
        .iter()
        .filter(|source| source.unverified())
        .map(|source| source.source_id.as_str())
        .collect();
    for item in &mut response.items {
        if item.statement_kind == StatementKind::Observed
            && item
                .source_ids
                .iter()
                .any(|id| unverified.contains(id.as_str()))
        {
            // Source certainty bounds the model's claim; the final report still
            // enforces every source identity and displays the uncertainty label.
            item.statement_kind = StatementKind::Uncertain;
        }
    }
    validate_items(&response.items, &sources).map_err(|_| super::contracts::invalid())?;
    if let Some(cap) = summary_cap(prompt_version) {
        if step.target["summary_limits"] != json!({"max_chars":cap,"max_items":MAX_SUMMARY_ITEMS})
            || response.items.len() > MAX_SUMMARY_ITEMS
        {
            return Err(super::contracts::invalid());
        }
        if matches!(prompt_version, Some(5 | 7)) {
            let draft: Vec<PersonalItem> =
                serde_json::from_value(step.target["summary_draft"].clone())?;
            if body_chars(&response.items) > cap
                || response.items.len() != draft.len()
                || response.items.iter().zip(&draft).any(|(item, prior)| {
                    item.kind != prior.kind
                        || item.statement_kind != prior.statement_kind
                        || item.source_ids.iter().collect::<BTreeSet<_>>()
                            != prior.source_ids.iter().collect::<BTreeSet<_>>()
                })
            {
                return Err(super::contracts::invalid());
            }
        }
        return Ok(PersonalOutput {
            items: response.items,
            omitted: 0,
            compaction_error: None,
        });
    }
    let (max_items, max_chars) = if !step.target["summary_limits"].is_null() {
        if step.target["summary_limits"]
            != json!({"max_chars":MAX_SUMMARY_CHARS,"max_items":MAX_SUMMARY_ITEMS})
        {
            return Err(super::contracts::invalid());
        }
        if response.items.len() <= MAX_SUMMARY_ITEMS
            && body_chars(&response.items) <= MAX_SUMMARY_CHARS
        {
            return Ok(PersonalOutput {
                items: response.items,
                omitted: 0,
                compaction_error: None,
            });
        }
        (MAX_SUMMARY_ITEMS, MAX_SUMMARY_CHARS)
    } else {
        (MAX_ITEMS, MAX_BODY_CHARS)
    };
    let priorities: BTreeMap<_, _> = selection
        .materials
        .iter()
        .map(|m| (&m.source.source_id, m.priority))
        .collect();
    let mut items = response.items;
    let original = items.len();
    items.sort_by_key(|item| {
        item.source_ids
            .iter()
            .filter_map(|id| priorities.get(id))
            .min()
            .copied()
            .unwrap_or(255)
    });
    // Keep complete statements, including their amounts, conditions and uncertainty.
    // Validate every source before omission, and retain the original model response.
    while items.len() > max_items || body_chars(&items) > max_chars {
        items.pop();
    }
    if items.is_empty() && max_chars == MAX_SUMMARY_CHARS {
        return Err(super::contracts::invalid());
    }
    Ok(PersonalOutput {
        omitted: original - items.len(),
        items,
        compaction_error: None,
    })
}
