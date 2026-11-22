use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineStatus {
    Ok,
    Partial,
    Error,
}

/// The per-round record stored in v4 scan snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefillRoundAudit {
    pub round: u64,
    pub planned_source_count: u64,
    pub plan_sha256: String,
    pub candidate_slots_used: u64,
    pub classification_pages_held: u64,
    pub extraction_slots_used: u64,
    pub reserved_chars: u64,
    pub refunded_chars: u64,
    pub refunded_classification_pages: u64,
}
