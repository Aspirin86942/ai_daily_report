use super::plan::{payload, PlannedStep, StepKind};
use crate::{
    config::Llm,
    reports::personal::{
        body_chars, validate_items, PersonalItem, Selection, StatementKind, MAX_BODY_CHARS,
        MAX_ITEMS,
    },
    snapshots::{canonical_json, digest},
    types::RunContext,
    ReportError,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const PROMPT: &str = include_str!("../../../../templates/personal_daily_prompt_v5.md");

pub fn schema() -> Value {
    json!({"schema_version":5,"items":[{"title":"简短标题","text":"事项正文",
        "kind":"progress|attention|next_step","statement_kind":"observed|user_statement|uncertain|suggestion","source_ids":["输入来源编号"]}]})
}

pub fn prepare(
    llm: &Llm,
    context: &RunContext,
    selection: &mut Selection,
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
                "window_end_utc":context.window_end_utc,"selection":selection}),
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
    step.prompt_sha256 = digest(PROMPT.as_bytes());
    step.schema_sha256 = digest(&canonical_json(&schema())?);
    let sources: Vec<_> = selection.materials.iter().map(|m| json!({
        "source_id":m.source.source_id,"path":m.source.relative_path,"source_kind":m.source.source_kind,
        "text_unverified":m.source.unverified(),"text":m.text,
        "parts":m.evidence.iter().map(|e| json!({"side":e.side,"start":e.input_start,"end":e.input_end,"ranges":e.ranges})).collect::<Vec<_>>()
    })).collect();
    step.request = payload(
        llm,
        format!("{PROMPT}\n{}", schema()),
        json!({"date":step.target["date"],"sources":sources}),
        llm.max_tokens
            .min(llm.verified_max_output_tokens.unwrap_or(llm.max_tokens)),
    )?;
    Ok(())
}

#[derive(Debug)]
pub struct PersonalOutput {
    pub items: Vec<PersonalItem>,
    pub omitted: usize,
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
    while items.len() > MAX_ITEMS || body_chars(&items) > MAX_BODY_CHARS {
        items.pop();
    }
    Ok(PersonalOutput {
        omitted: original - items.len(),
        items,
    })
}
