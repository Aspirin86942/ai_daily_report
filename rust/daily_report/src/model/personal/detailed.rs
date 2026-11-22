use super::{
    request_source, summary_step, Material, PlannedStep, PublicationRules, Selection,
    BOUNDED_PLAN_VERSION, BOUNDED_PROMPT_VERSION, DETAILED_PROMPT_VERSION, SUMMARY_STEP,
};
use crate::{
    config::Llm,
    model::plan::request_too_large,
    reports::personal::MAX_REPORT_BYTES,
    snapshots::{canonical_json, valid_hash},
    types::RunContext,
    ReportError,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

/// Freeze the complete evidence blocks that actually fit one detailed summary request.
pub fn prepare_detailed_scan(
    llm: &Llm,
    context: &RunContext,
    full: &Selection,
) -> Result<PlannedStep, ReportError> {
    prepare_scan(llm, context, full, None)
}

pub(crate) fn prepare_scan(
    llm: &Llm,
    context: &RunContext,
    full: &Selection,
    publication: Option<&PublicationRules>,
) -> Result<PlannedStep, ReportError> {
    full.validate()?;
    let empty = summary_step(llm, context, &Selection::default(), publication)?;
    let remaining = empty
        .byte_limit(llm)?
        .checked_sub(empty.bytes()?.len())
        .ok_or_else(request_too_large)?;
    if full.materials.is_empty() {
        return summary_step(llm, context, full, publication);
    }

    let included = choose_blocks(&full.materials, remaining)?;
    if !included.contains(&true) {
        return Err(request_too_large());
    }

    // Restore scan order so neighboring rows from one file remain readable together.
    let mut summary = full.summary.clone();
    let mut materials = Vec::new();
    for (material, included) in full.materials.iter().zip(included) {
        if included {
            materials.push(material.clone());
        } else {
            summary.omit(
                "budget",
                material
                    .source
                    .relative_path
                    .as_deref()
                    .unwrap_or("user_input"),
            );
        }
    }
    summary.selected_chars = materials.iter().map(|m| m.source.excerpt_chars).sum();
    summary.selected_files = materials
        .iter()
        .filter_map(|m| m.source.relative_path.as_deref())
        .collect::<BTreeSet<_>>()
        .len() as u64;
    let sent = Selection {
        materials,
        summary,
        gaps: full.gaps.clone(),
    };
    let step = summary_step(llm, context, &sent, publication)?;
    step.check_capacity(llm)?;
    Ok(step)
}

fn choose_blocks(materials: &[Material], budget: usize) -> Result<Vec<bool>, ReportError> {
    let mut by_path = BTreeMap::<_, Vec<_>>::new();
    for (index, material) in materials.iter().enumerate() {
        if material.text.len() <= budget {
            let bytes = source_bytes(material)?;
            if bytes <= budget {
                by_path
                    .entry(material.source.relative_path.as_deref())
                    .or_default()
                    .push((index, bytes + 1));
                continue;
            }
        }
        // A request that cannot carry the operator's supplement must not silently
        // generate a report using files alone. The full input remains frozen.
        if material.source.source_id == "user_input" {
            return Err(request_too_large());
        }
    }
    // Charge a comma with each block and credit the absent first comma once.
    let mut spare = budget + 1;
    let mut groups: Vec<_> = by_path
        .into_iter()
        .map(|(path, blocks)| {
            let minimum = blocks.iter().map(|&(_, cost)| cost).min().unwrap();
            (path.is_some(), minimum, blocks)
        })
        .collect();
    // A file's large first block must not consume space that could represent other files.
    // Reserve one affordable block per source, giving user input its original priority.
    groups.sort_unstable_by_key(|(file, minimum, blocks)| (*file, *minimum, blocks[0].0));
    groups.retain(|(_, minimum, _)| {
        if *minimum > spare {
            return false;
        }
        spare -= minimum;
        true
    });
    groups.sort_unstable_by_key(|(_, _, blocks)| blocks[0].0);
    let mut included = vec![false; materials.len()];
    let mut later = Vec::new();
    for (_, minimum, mut blocks) in groups {
        let allowance = minimum + spare;
        // Prefer important blocks (for example totals) within this file's turn.
        // The stable sort preserves scan order among equally important blocks.
        blocks.sort_by_key(|&(index, _)| materials[index].priority);
        let first = blocks
            .iter()
            .position(|&(_, cost)| cost <= allowance)
            .unwrap();
        let (index, cost) = blocks.remove(first);
        included[index] = true;
        spare = allowance - cost;
        for (round, (index, cost)) in blocks.into_iter().enumerate() {
            later.push((round, cost, index));
        }
    }
    // Each remaining file gets one turn per round. A cheaper block wins a tight round.
    later.sort_unstable();
    for (_, cost, index) in later {
        if cost <= spare {
            included[index] = true;
            spare -= cost;
        }
    }
    Ok(included)
}

fn source_bytes(material: &Material) -> Result<usize, ReportError> {
    let fragment = canonical_json(&request_source(material, true))?;
    // The inner JSON already escaped control characters. Only its quotes and backslashes
    // expand when that fragment is embedded in the outer message's content string.
    let escapes = fragment
        .iter()
        .filter(|&&byte| matches!(byte, b'"' | b'\\'))
        .count();
    Ok(fragment.len() + escapes)
}

pub(super) fn validate_shape(
    step: &PlannedStep,
    selection: &Selection,
    compacting: bool,
) -> Result<Option<PublicationRules>, ReportError> {
    let bounded = step.version == BOUNDED_PLAN_VERSION;
    let prompt = if bounded {
        BOUNDED_PROMPT_VERSION
    } else {
        DETAILED_PROMPT_VERSION
    };
    let fields = 5 + usize::from(bounded) + usize::from(compacting);
    let invalid = || {
        ReportError::new(
            "MODEL_PLAN_INVALID",
            if bounded {
                "限长日报的冻结模型计划无效"
            } else {
                "详细日报的冻结模型计划无效"
            },
        )
    };
    let date = step.target["date"].as_str().ok_or_else(invalid)?;
    if step.target["summary_prompt_version"] != prompt
        || step
            .target
            .as_object()
            .is_none_or(|target| target.len() != fields)
        || ["window_start_utc", "window_end_utc"]
            .iter()
            .any(|key| !step.target[key].is_string())
        || if compacting {
            !step.target["summary_draft"].is_array()
                || !step.parent_sha256.as_deref().is_some_and(valid_hash)
        } else {
            step.logical_step != SUMMARY_STEP || step.parent_sha256.is_some()
        }
        || step.revision != 0
        || step.coverage != json!({})
        || step.output_capacity_bytes != MAX_REPORT_BYTES
    {
        return Err(invalid());
    }
    let publication = bounded
        .then(|| PublicationRules::deserialize(&step.target["publication"]))
        .transpose()
        .map_err(|_| invalid())?;
    if publication
        .as_ref()
        .is_some_and(|rules| !rules.valid_for(date, selection))
    {
        return Err(invalid());
    }
    Ok(publication)
}
