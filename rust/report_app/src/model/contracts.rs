use crate::ReportError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_RESPONSE_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub topic: String,
    pub background: Option<String>,
    pub detail: String,
    pub conditions: Vec<String>,
    pub status: Option<String>,
    pub source_ids: Vec<String>,
    pub uncertainty: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SelectedFact {
    pub fact_id: String,
    pub pending: bool,
    pub fact: Fact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matter_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<crate::evidence::EvidenceRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CitedText {
    pub text: String,
    pub fact_ids: Vec<String>,
    pub source_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Matter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matter_id: Option<String>,
    pub title: String,
    pub background: Option<CitedText>,
    pub details: Vec<CitedText>,
    pub status: Option<CitedText>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposedReport {
    pub schema_version: u32,
    pub overview: Vec<CitedText>,
    pub matters: Vec<Matter>,
    pub pending: Vec<CitedText>,
}

pub fn invalid() -> ReportError {
    ReportError::new("MODEL_RESPONSE_INVALID", "模型响应结构、内容范围或引用无效")
}

fn bounded(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.chars().count() <= max
}
fn ids(ids: &[String], available: &BTreeSet<String>) -> bool {
    !ids.is_empty()
        && ids.len() <= 64
        && ids.iter().collect::<BTreeSet<_>>().len() == ids.len()
        && ids.iter().all(|id| available.contains(id))
}

impl ComposedReport {
    pub fn validate(&self, facts: &[SelectedFact]) -> Result<(), ReportError> {
        if !matches!(self.schema_version, 3 | 4) || self.overview.len() > 5 {
            return Err(invalid());
        }
        let by_id: BTreeMap<_, _> = facts
            .iter()
            .map(|fact| (fact.fact_id.clone(), fact))
            .collect();
        if by_id.len() != facts.len() {
            return Err(invalid());
        }
        let available: BTreeSet<_> = by_id.keys().cloned().collect();
        let mut detailed = BTreeSet::new();
        let mut pending_ids = BTreeSet::new();
        let check = |item: &CitedText| -> Result<(), ReportError> {
            if !bounded(&item.text, 6000) || !ids(&item.fact_ids, &available) {
                return Err(invalid());
            }
            let expected: BTreeSet<_> = item
                .fact_ids
                .iter()
                .flat_map(|id| by_id[id].fact.source_ids.iter().cloned())
                .collect();
            if item.source_ids.iter().cloned().collect::<BTreeSet<_>>() != expected
                || item.source_ids.len() != expected.len()
            {
                return Err(invalid());
            }
            Ok(())
        };
        for item in &self.overview {
            check(item)?;
            if item.fact_ids.iter().any(|id| by_id[id].pending) {
                return Err(invalid());
            }
        }
        if facts.iter().any(|f| !f.pending) && (self.overview.is_empty() || self.matters.is_empty())
        {
            return Err(invalid());
        }
        let mut titles = BTreeSet::new();
        for matter in &self.matters {
            if !bounded(&matter.title, 120)
                || !titles.insert(matter.matter_id.as_deref().unwrap_or(matter.title.trim()))
                || matter.details.is_empty()
            {
                return Err(invalid());
            }
            for item in matter
                .background
                .iter()
                .chain(&matter.details)
                .chain(&matter.status)
            {
                check(item)?;
                if self.schema_version == 4
                    && (matter.matter_id.is_none()
                        || item
                            .fact_ids
                            .iter()
                            .any(|id| by_id[id].matter_id != matter.matter_id))
                {
                    return Err(invalid());
                }
                if item.fact_ids.iter().any(|id| by_id[id].pending) {
                    return Err(invalid());
                }
                detailed.extend(item.fact_ids.iter().cloned());
            }
        }
        for item in &self.pending {
            check(item)?;
            pending_ids.extend(item.fact_ids.iter().cloned());
        }
        for fact in facts {
            if fact.pending && !pending_ids.contains(&fact.fact_id)
                || !fact.pending
                    && !detailed.contains(&fact.fact_id)
                    && !pending_ids.contains(&fact.fact_id)
            {
                return Err(ReportError::new(
                    "MODEL_COMPOSE_COVERAGE_INSUFFICIENT",
                    "成稿未完整覆盖本组事实，需保留全部事实继续拆分。",
                ));
            }
        }
        Ok(())
    }
}
