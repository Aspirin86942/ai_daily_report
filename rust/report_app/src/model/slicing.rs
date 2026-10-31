//! Readable historical source data; model slicing and batch execution are retired.
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputUnit {
    pub start: usize,
    pub end: usize,
    pub kind: String,
    #[serde(default)]
    pub ranges: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceInput {
    pub source_id: String,
    pub source_kind: String,
    pub path: String,
    pub text: String,
    pub units: Vec<InputUnit>,
    pub history_priority: bool,
    pub text_unverified: bool,
}
