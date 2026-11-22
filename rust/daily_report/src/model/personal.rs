mod bounded;
mod detailed;
use super::{
    contracts::invalid,
    plan::{payload, PlannedStep, StepKind, BOUNDED_PLAN_VERSION, DETAILED_PLAN_VERSION},
};
use crate::{
    config::Llm,
    reports::personal::{
        body_chars, source_map, validate_item_sources, ItemKind, Material, PersonalItem, Selection,
        StatementKind, MAX_BODY_CHARS, MAX_ITEMS, MAX_OVERVIEW_CHARS, MAX_REPORT_BYTES,
        MAX_SUMMARY_CHARS, MAX_SUMMARY_CHARS_EXTENDED, MAX_SUMMARY_ITEMS,
    },
    snapshots::{canonical_json, digest},
    types::RunContext,
    ReportError,
};
pub(crate) use bounded::{PersonalPrompts, PublicationRules};
pub use detailed::prepare_detailed_scan;
pub(crate) use detailed::prepare_scan;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

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
pub const SCAN_PROMPT_DETAILED: &str =
    include_str!("../../../../templates/personal_daily_scan_prompt_v8.md");
pub const SCAN_PROMPT_V9: &str = include_str!("../../../../templates/daily_prompt.md");
pub const COMPACT_PROMPT_V8: &str = include_str!("../../../../templates/daily_compact_prompt.md");
const DETAILED_PROMPT_VERSION: u32 = 8;
const BOUNDED_PROMPT_VERSION: u32 = 9;
const SUMMARY_STEP: &str = "personal_summary";
const COMPACTION_STEP: &str = "personal_summary.compact";

fn historical_prompt<'a>(template: &'a str, request: &Value) -> Cow<'a, str> {
    let frozen_crlf = request["messages"][0]["content"]
        .as_str()
        .is_some_and(|system| system.contains("\r\n"));
    if frozen_crlf {
        Cow::Owned(template.replace("\r\n", "\n").replace('\n', "\r\n"))
    } else if template.contains("\r\n") {
        Cow::Owned(template.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(template)
    }
}

pub fn schema() -> Value {
    json!({"schema_version":5,"items":[{"title":"简短标题","text":"事项正文",
        "kind":"progress|attention|next_step","statement_kind":"observed|user_statement|uncertain|suggestion","source_ids":["输入来源编号"]}]})
}

pub(crate) fn legacy_compaction_prompt(plan: &PlannedStep) -> Option<u64> {
    match plan.target["summary_prompt_version"].as_u64() {
        Some(4) => Some(5),
        Some(6) => Some(7),
        _ => None,
    }
}

fn summary_step(
    llm: &Llm,
    context: &RunContext,
    selection: &Selection,
    publication: Option<&PublicationRules>,
) -> Result<PlannedStep, ReportError> {
    let (version, prompt_version) = if publication.is_some() {
        (BOUNDED_PLAN_VERSION, BOUNDED_PROMPT_VERSION)
    } else {
        (DETAILED_PLAN_VERSION, DETAILED_PROMPT_VERSION)
    };
    let mut target = json!({"date":context.target_date,"window_start_utc":context.window_start_utc,
        "window_end_utc":context.window_end_utc,"selection":selection,
        "summary_prompt_version":prompt_version});
    if let Some(publication) = publication {
        target["publication"] = serde_json::to_value(publication)?;
    }
    let mut step = PlannedStep {
        version,
        kind: StepKind::PersonalSummary,
        logical_step: SUMMARY_STEP.into(),
        revision: 0,
        parent_sha256: None,
        prompt_sha256: String::new(),
        schema_sha256: String::new(),
        target,
        entries: vec![],
        facts: vec![],
        source_map: serde_json::to_value(source_map(selection.sources()))?,
        coverage: json!({}),
        output_capacity_bytes: MAX_REPORT_BYTES,
        request: Value::Null,
    };
    let output_tokens = llm
        .max_tokens
        .min(llm.verified_max_output_tokens.unwrap_or(llm.max_tokens));
    freeze_request(&mut step, llm, output_tokens)?;
    Ok(step)
}

fn request_source(material: &Material, detailed: bool) -> Value {
    let source = &material.source;
    let mut value = json!({
        "source_id":source.source_id,"path":source.relative_path,"source_kind":source.source_kind,
        "text_unverified":source.unverified(),"text":material.text,
        "parts":material.evidence.iter().map(|e| json!({"side":e.side,"start":e.input_start,
            "end":e.input_end,"ranges":e.ranges})).collect::<Vec<_>>()
    });
    if detailed {
        if let Some(context) = source.coverage_limits["personal"].get("context") {
            value["context"] = context.clone();
        }
    }
    value
}

fn freeze_request(
    step: &mut PlannedStep,
    llm: &Llm,
    output_tokens: u64,
) -> Result<(), ReportError> {
    (step.prompt_sha256, step.schema_sha256, step.request) =
        FrozenStep::read(step)?.request(llm, output_tokens)?;
    Ok(())
}

/// Decode the same evidence and draft for request identity, response validation and replay.
pub(super) struct FrozenStep<'a> {
    pub(super) plan: &'a PlannedStep,
    selection: Selection,
    publication: Option<PublicationRules>,
    draft: Option<Vec<PersonalItem>>,
}

impl<'a> FrozenStep<'a> {
    pub(super) fn read(plan: &'a PlannedStep) -> Result<Self, ReportError> {
        let selection = Selection::deserialize(&plan.target["selection"])?;
        selection.validate()?;
        let sources = source_map(selection.sources());
        if plan.source_map != serde_json::to_value(&sources)?
            || !plan.entries.is_empty()
            || !plan.facts.is_empty()
        {
            return Err(ReportError::new(
                "MODEL_PLAN_IDENTITY_MISMATCH",
                "个人日报来源与冻结材料不一致",
            ));
        }
        let compacting = match plan.version {
            6 => matches!(plan.target["summary_prompt_version"].as_u64(), Some(5 | 7)),
            BOUNDED_PLAN_VERSION => plan.logical_step == COMPACTION_STEP,
            _ => false,
        };
        let publication = if plan.version == 6 {
            None
        } else {
            detailed::validate_shape(plan, &selection, compacting)?
        };
        let draft = if compacting {
            let items = Vec::<PersonalItem>::deserialize(&plan.target["summary_draft"])?;
            validate_item_sources(&items, &sources)?;
            if items.is_empty() || publication.is_none() && items.len() > MAX_SUMMARY_ITEMS {
                return Err(invalid());
            }
            Some(items)
        } else {
            None
        };
        Ok(Self {
            plan,
            selection,
            publication,
            draft,
        })
    }

    fn prompt_version(&self) -> Option<u64> {
        self.plan.target["summary_prompt_version"].as_u64()
    }

    fn cap(&self) -> Option<usize> {
        match self.prompt_version() {
            Some(4 | 5) => Some(MAX_SUMMARY_CHARS),
            Some(6 | 7) => Some(MAX_SUMMARY_CHARS_EXTENDED),
            _ => self.publication.as_ref().map(|rules| rules.max_chars),
        }
    }

    fn measure(&self, items: &[PersonalItem]) -> Result<usize, ReportError> {
        self.publication
            .as_ref()
            .map_or_else(|| Ok(body_chars(items)), |rules| rules.body_chars(items))
    }

    fn prompt(&self) -> Result<Cow<'_, str>, ReportError> {
        if let Some(rules) = &self.publication {
            return Ok(Cow::Borrowed(if self.draft.is_some() {
                &rules.prompts.compaction
            } else {
                &rules.prompts.summary
            }));
        }
        let template = match (self.plan.version, self.prompt_version()) {
            (DETAILED_PLAN_VERSION, _) => SCAN_PROMPT_DETAILED,
            (_, None) if self.plan.target.get("summary_prompt_version").is_none() => LEGACY_PROMPT,
            (_, Some(2)) => PROMPT,
            (_, Some(3)) => SCAN_PROMPT,
            (_, Some(4)) => SCAN_COMPACT_PROMPT,
            (_, Some(5)) => COMPACT_PROMPT,
            (_, Some(6)) => SCAN_PROMPT_EXTENDED,
            (_, Some(7)) => COMPACT_PROMPT_EXTENDED,
            _ => {
                return Err(ReportError::new(
                    "MODEL_PLAN_INVALID",
                    "个人日报提示词版本无效",
                ))
            }
        };
        Ok(historical_prompt(template, &self.plan.request))
    }

    pub(super) fn request(
        &self,
        llm: &Llm,
        output_tokens: u64,
    ) -> Result<(String, String, Value), ReportError> {
        let prompt = self.prompt()?;
        let mut response_schema = schema();
        let mut input = json!({"date":self.plan.target["date"]});
        if let Some(draft) = &self.draft {
            response_schema["items"][0]["item_id"] = json!(1);
            let mut identified = serde_json::to_value(draft)?;
            for (index, item) in identified.as_array_mut().unwrap().iter_mut().enumerate() {
                item["item_id"] = json!(index + 1);
            }
            let draft_chars = self.measure(draft)?;
            let cap = self.cap().ok_or_else(invalid)?;
            if self.publication.is_some() && draft_chars <= cap {
                return Err(invalid());
            }
            input["draft_items"] = identified;
            input["draft_chars"] = json!(draft_chars);
            input["max_chars"] = json!(cap);
            if self.publication.is_none() {
                input["target_chars"] = json!(560);
            }
        } else {
            input["sources"] = Value::Array(
                self.selection
                    .materials
                    .iter()
                    .map(|material| request_source(material, self.plan.version != 6))
                    .collect(),
            );
            if let Some(rules) = &self.publication {
                input["max_chars"] = json!(rules.max_chars);
            }
        }
        Ok((
            digest(prompt.as_bytes()),
            digest(&canonical_json(&response_schema)?),
            payload(
                llm,
                format!("{prompt}\n{response_schema}"),
                input,
                output_tokens,
            )?,
        ))
    }
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
    let frozen = plan.checked(runner.llm)?;
    let mut output = runner.execute_frozen(&frozen)?.output;
    let Some(cap) = frozen.cap().filter(|_| frozen.draft.is_none()) else {
        return Ok(output);
    };
    if output.items.is_empty() || frozen.measure(&output.items)? <= cap {
        return Ok(output);
    }
    let compacted = (|| {
        let mut compact = plan.clone();
        compact.logical_step = format!("{}.compact", plan.logical_step);
        compact.parent_sha256 = Some(plan.identity()?);
        if let Some(prompt) = legacy_compaction_prompt(plan) {
            compact.target["summary_prompt_version"] = json!(prompt);
        }
        compact.target["summary_draft"] = serde_json::to_value(&output.items)?;
        // Derive the same child request on recovery. execute checks live limits before sending.
        freeze_request(&mut compact, runner.llm, plan.frozen_output_tokens()?)?;
        Ok::<_, ReportError>(runner.execute(&compact)?.output)
    })();
    let error = match compacted {
        Ok(result) => return Ok(result),
        Err(error) => error,
    };
    let priorities = source_priorities(&frozen.selection);
    while frozen.measure(&output.items)? > cap {
        if output.items.len() <= 1 || !omit_lowest_priority(&mut output.items, &priorities) {
            if frozen.publication.is_some() {
                return Err(ReportError::new(
                    "PERSONAL_REPORT_BODY_TOO_LONG",
                    format!(
                        "精简未能形成 1,500 字以内的完整事项和下一步（{}）",
                        error.code
                    ),
                ));
            }
            return Err(error);
        }
        output.omitted += 1;
    }
    output.compaction_error = Some(error.code);
    Ok(output)
}

pub(crate) fn source_priorities(selection: &Selection) -> BTreeMap<&str, u8> {
    selection
        .materials
        .iter()
        .map(|m| (m.source.source_id.as_str(), m.priority))
        .collect()
}

fn priority(item: &PersonalItem, priorities: &BTreeMap<&str, u8>) -> u8 {
    item.source_ids
        .iter()
        .filter_map(|id| priorities.get(id.as_str()).copied())
        .min()
        .unwrap_or(255)
}

/// Preserve action items and the original display order during last-resort omission.
pub(crate) fn omit_lowest_priority(
    items: &mut Vec<PersonalItem>,
    priorities: &BTreeMap<&str, u8>,
) -> bool {
    let candidate = items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.kind != ItemKind::NextStep)
        .max_by_key(|(index, item)| (priority(item, priorities), *index))
        .map(|(index, _)| index);
    if let Some(index) = candidate {
        items.remove(index);
        true
    } else {
        false
    }
}

pub(super) fn validate_response(
    step: &FrozenStep<'_>,
    mut value: Value,
) -> Result<PersonalOutput, ReportError> {
    // The frozen personal request fixes the response format. Some providers omit
    // its version tag; explicit versions and all content still validate below.
    if let Some(object) = value.as_object_mut() {
        object.entry("schema_version").or_insert(json!(5));
    }
    if step.draft.is_some() {
        let items = value
            .get_mut("items")
            .and_then(Value::as_array_mut)
            .ok_or_else(invalid)?;
        for (index, item) in items.iter_mut().enumerate() {
            if item.as_object_mut().and_then(|item| item.remove("item_id"))
                != Some(json!(index + 1))
            {
                return Err(invalid());
            }
        }
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Response {
        schema_version: u32,
        items: Vec<PersonalItem>,
    }
    let response: Response = serde_json::from_value(value).map_err(|_| invalid())?;
    let mut items = response.items;
    let legacy = step.plan.version == 6;
    if response.schema_version != 5 || (legacy && items.is_empty()) {
        return Err(invalid());
    }
    let sources = source_map(step.selection.sources());
    for item in &mut items {
        let mut cited = item.source_ids.iter().filter_map(|id| {
            sources
                .get(id.as_str())
                .filter(|source| source.unverified())
        });
        let needs_confirmation = match item.statement_kind {
            StatementKind::Observed => cited.next().is_some(),
            // A user note cannot confirm a separate image transcription. Older frozen
            // inputs predate this backend and keep their original response validation.
            StatementKind::UserStatement => cited.any(|source| {
                source.coverage_limits["evidence_audit"]["parser_version"].as_str()
                    == Some(daily_parsers::settings::OCR_BACKEND)
            }),
            StatementKind::Uncertain | StatementKind::Suggestion => false,
        };
        if needs_confirmation {
            item.statement_kind = StatementKind::Uncertain;
        }
    }
    validate_item_sources(&items, &sources).map_err(|_| invalid())?;
    let original = items.len();
    if !legacy {
        if items
            .iter()
            .any(|item| item.title.chars().count() > MAX_OVERVIEW_CHARS)
        {
            return Err(invalid());
        }
    } else {
        let limits = &step.plan.target["summary_limits"];
        let compaction_cap = step.cap();
        let limited = compaction_cap.is_some() || !limits.is_null();
        let (max_items, max_chars) = if limited {
            (
                MAX_SUMMARY_ITEMS,
                compaction_cap.unwrap_or(MAX_SUMMARY_CHARS),
            )
        } else {
            (MAX_ITEMS, MAX_BODY_CHARS)
        };
        if limited && *limits != json!({"max_chars":max_chars,"max_items":max_items})
            || compaction_cap.is_some() && items.len() > max_items
        {
            return Err(invalid());
        }
        if compaction_cap.is_none()
            && (!limited || items.len() > max_items || body_chars(&items) > max_chars)
        {
            let priorities = source_priorities(&step.selection);
            items.sort_by_key(|item| priority(item, &priorities));
            while items.len() > max_items || body_chars(&items) > max_chars {
                items.pop();
            }
            if limited && items.is_empty() {
                return Err(invalid());
            }
        }
    }
    if let Some(draft) = &step.draft {
        validate_compaction(step, draft, &items)?;
    }
    Ok(PersonalOutput {
        omitted: original - items.len(),
        items,
        compaction_error: None,
    })
}

fn validate_compaction(
    step: &FrozenStep<'_>,
    draft: &[PersonalItem],
    items: &[PersonalItem],
) -> Result<(), ReportError> {
    let legacy = step.publication.is_none();
    if items.len() != draft.len()
        || items.iter().zip(draft).any(|(item, prior)| {
            item.kind != prior.kind
                || item.statement_kind != prior.statement_kind
                || if legacy {
                    item.source_ids.iter().collect::<BTreeSet<_>>()
                        != prior.source_ids.iter().collect::<BTreeSet<_>>()
                } else {
                    !bounded::keeps_content(item, prior)
                }
        })
    {
        return Err(invalid());
    }
    if step.measure(items)? > step.cap().ok_or_else(invalid)? {
        return Err(if legacy {
            invalid()
        } else {
            ReportError::new(
                "MODEL_COMPACTION_TOO_LONG",
                "精简后的正文超过 1,500 个 Unicode 字符",
            )
        });
    }
    Ok(())
}
