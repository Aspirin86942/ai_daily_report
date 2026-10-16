use crate::{snapshots::canonical_json, ReportError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

pub const FORMAT_VERSION: u32 = 3;
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactsResponse {
    pub schema_version: u32,
    pub facts: Vec<Fact>,
    pub pending: Vec<Fact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SelectedFact {
    pub fact_id: String,
    pub pending: bool,
    pub fact: Fact,
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
fn optional(text: &Option<String>, max: usize) -> bool {
    text.as_ref().is_none_or(|s| bounded(s, max))
}
fn ids(ids: &[String], available: &BTreeSet<String>) -> bool {
    !ids.is_empty()
        && ids.len() <= 64
        && ids.iter().collect::<BTreeSet<_>>().len() == ids.len()
        && ids.iter().all(|id| available.contains(id))
}

impl FactsResponse {
    pub fn select(
        self,
        step: &str,
        sources: &BTreeSet<String>,
        unverified: &BTreeSet<String>,
        capacity: usize,
    ) -> Result<Vec<SelectedFact>, ReportError> {
        if self.schema_version != FORMAT_VERSION {
            return Err(invalid());
        }
        if self.facts.len() + self.pending.len() > 64 {
            return Err(ReportError::new(
                "MODEL_FACT_COUNT_EXCEEDED",
                "本批事实数量超过单次响应容量",
            ));
        }
        let mut selected = Vec::new();
        for (pending, facts) in [(false, self.facts), (true, self.pending)] {
            for fact in facts {
                if !bounded(&fact.topic, 120)
                    || !bounded(&fact.detail, 4000)
                    || !optional(&fact.background, 2000)
                    || !optional(&fact.status, 1000)
                    || !optional(&fact.uncertainty, 2000)
                    || fact.conditions.len() > 20
                    || fact.conditions.iter().any(|s| !bounded(s, 1000))
                    || !ids(&fact.source_ids, sources)
                    || (!pending
                        && (fact.uncertainty.is_some()
                            || fact.source_ids.iter().any(|id| unverified.contains(id))))
                {
                    return Err(invalid());
                }
                selected.push(SelectedFact {
                    fact_id: format!("{step}:{:03}", selected.len()),
                    pending,
                    fact,
                });
            }
        }
        if canonical_json(&serde_json::to_value(&selected)?)?.len() > capacity {
            return Err(ReportError::new(
                "MODEL_FACT_CAPACITY_EXCEEDED",
                "事实输出超过单次响应字节容量",
            ));
        }
        Ok(selected)
    }
}

impl ComposedReport {
    pub fn validate_part(&self, facts: &[SelectedFact]) -> Result<(), ReportError> {
        if self.matters.len() > 64
            || self.pending.len() > 64
            || self.matters.iter().any(|matter| matter.details.len() > 64)
        {
            return Err(invalid());
        }
        self.validate(facts)
    }

    pub fn merge(parts: Vec<Self>, facts: &[SelectedFact]) -> Result<Self, ReportError> {
        let mut merged = Self {
            schema_version: FORMAT_VERSION,
            overview: vec![],
            matters: vec![],
            pending: vec![],
        };
        for part in parts {
            // Overview is a short navigation aid. Every fact remains covered in
            // the full body or pending section, checked again after merging.
            merged
                .overview
                .extend(part.overview.into_iter().take(5 - merged.overview.len()));
            for mut matter in part.matters {
                if let Some(existing) = merged
                    .matters
                    .iter_mut()
                    .find(|item| item.title.trim() == matter.title.trim())
                {
                    existing.details.extend(matter.background.take());
                    existing.details.append(&mut matter.details);
                    existing.details.extend(matter.status.take());
                } else {
                    merged.matters.push(matter);
                }
            }
            merged.pending.extend(part.pending);
        }
        merged.validate(facts)?;
        Ok(merged)
    }

    pub fn validate(&self, facts: &[SelectedFact]) -> Result<(), ReportError> {
        if self.schema_version != FORMAT_VERSION || self.overview.len() > 5 {
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
                || !titles.insert(matter.title.trim())
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
                return Err(invalid());
            }
        }
        Ok(())
    }
}

/// This versioned schema is included in request identity and in the prompt.
pub fn response_schema(compose: bool) -> Value {
    let text = json!({"type":"string","minLength":1});
    let array = |items: Value| json!({"type":"array","items":items});
    let nullable = |value: Value| json!({"anyOf":[value,{"type":"null"}]});
    let object = |properties: Value, required: Value| json!({"type":"object","additionalProperties":false,"properties":properties,"required":required});
    if compose {
        let cited = object(
            json!({"text":text,"fact_ids":array(text.clone()),"source_ids":array(text.clone())}),
            json!(["text", "fact_ids", "source_ids"]),
        );
        let matter = object(
            json!({"title":text,"background":nullable(cited.clone()),"details":array(cited.clone()),"status":nullable(cited.clone())}),
            json!(["title", "background", "details", "status"]),
        );
        object(
            json!({"schema_version":{"const":3},"overview":array(cited.clone()),"matters":array(matter),"pending":array(cited)}),
            json!(["schema_version", "overview", "matters", "pending"]),
        )
    } else {
        let fact = object(
            json!({"topic":text,"background":nullable(text.clone()),"detail":text,"conditions":array(text.clone()),"status":nullable(text.clone()),"source_ids":array(text.clone()),"uncertainty":nullable(text)}),
            json!([
                "topic",
                "background",
                "detail",
                "conditions",
                "status",
                "source_ids",
                "uncertainty"
            ]),
        );
        object(
            json!({"schema_version":{"const":3},"facts":array(fact.clone()),"pending":array(fact)}),
            json!(["schema_version", "facts", "pending"]),
        )
    }
}
