use crate::ReportError;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// Model permissions and settings used to inspect retained archive evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveSettings {
    pub compress_extractions: bool,
    pub roots: Vec<PathBuf>,
    pub model_roots: Vec<PathBuf>,
    pub allow_work_dir_for_model: bool,
    pub model_ignored_prefixes: Vec<String>,
    /// These four parameters identify historical extractions; they start no workers.
    pub worker_memory_mib: u64,
    pub parse_seconds: u64,
    pub max_input_mib: u64,
    pub max_output_mib: u64,
    /// Keys are worksheet/table labels; values are exact header names.
    pub table_keys: BTreeMap<String, Vec<String>>,
}

impl Default for ArchiveSettings {
    fn default() -> Self {
        Self {
            compress_extractions: true,
            roots: vec![],
            model_roots: vec![],
            allow_work_dir_for_model: true,
            model_ignored_prefixes: vec![".tmp-".into()],
            worker_memory_mib: 512,
            parse_seconds: 300,
            max_input_mib: 512,
            max_output_mib: 256,
            table_keys: BTreeMap::new(),
        }
    }
}

impl ArchiveSettings {
    pub fn validate(&self) -> Result<(), ReportError> {
        if self.model_ignored_prefixes.iter().any(|prefix| {
            prefix.is_empty() || prefix.len() > 128 || prefix.contains(['/', '\\', '\r', '\n'])
        }) {
            return Err(super::error("ARCHIVE_CONFIG_INVALID"));
        }
        if self.worker_memory_mib != 512
            || !(1..=300).contains(&self.parse_seconds)
            || !(1..=512).contains(&self.max_input_mib)
            || !(1..=256).contains(&self.max_output_mib)
        {
            return Err(super::error("ARCHIVE_CONFIG_INVALID"));
        }
        Ok(())
    }
}
