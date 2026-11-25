use super::{PersonalItem, Selection, MAX_BODY_CHARS};
use crate::{
    reports::{
        environment,
        personal::{bounded_body, compile_template},
    },
    snapshots::valid_hash,
    ReportError,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersonalPrompts {
    pub summary: String,
    pub compaction: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationRules {
    pub prompts: PersonalPrompts,
    pub template: String,
    pub evidence_link: String,
    pub labels: BTreeMap<String, String>,
    pub max_chars: usize,
}

impl PublicationRules {
    pub(super) fn valid_for(&self, date: &str, selection: &Selection) -> bool {
        self.max_chars == MAX_BODY_CHARS
            && !self.prompts.summary.trim().is_empty()
            && !self.prompts.compaction.trim().is_empty()
            && !self.template.trim().is_empty()
            && self
                .evidence_link
                .strip_prefix(&format!("attachments/{date}."))
                .and_then(|link| link.strip_suffix(".md"))
                .is_some_and(valid_hash)
            && selection
                .materials
                .iter()
                .all(|material| self.labels.contains_key(&material.source.source_id))
    }

    pub(super) fn body_chars(&self, items: &[PersonalItem]) -> Result<usize, ReportError> {
        let env = environment();
        Ok(bounded_body(
            &compile_template(&env, &self.template)?,
            items,
            &self.labels,
            &self.evidence_link,
            false,
        )?
        .chars()
        .count())
    }
}

fn retains_source_subset(kept: &[String], prior: &[String]) -> bool {
    if kept.is_empty() || kept.len() > prior.len() {
        return false;
    }
    let mut prior_iter = prior.iter();
    kept.iter()
        .all(|id| prior_iter.any(|candidate| candidate == id))
}

pub(super) fn keeps_content(item: &PersonalItem, prior: &PersonalItem) -> bool {
    retains_source_subset(&item.source_ids, &prior.source_ids)
        && item.title.trim() == prior.title.trim()
        && assertions(&item.text) == assertions(&prior.text)
}

fn assertions(text: &str) -> Vec<&str> {
    let mut assertions: Vec<_> = text
        .split_inclusive(['。', '！', '？', '\n'])
        .map(str::trim)
        .map(|sentence| {
            [
                "总体而言，",
                "综合来看，",
                "具体来说，",
                "如前所述，",
                "简言之，",
            ]
            .iter()
            .find_map(|prefix| sentence.strip_prefix(prefix))
            .unwrap_or(sentence)
            .trim()
        })
        .filter(|sentence| !sentence.is_empty())
        .collect();
    // Separated repetitions can belong to different conditions.
    assertions.dedup();
    assertions
}
