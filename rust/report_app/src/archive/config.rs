use crate::ReportError;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveSettings {
    pub collection_interval_minutes: u32,
    pub compress_extractions: bool,
    pub roots: Vec<PathBuf>,
    pub model_roots: Vec<PathBuf>,
    pub allow_work_dir_for_model: bool,
    pub model_ignored_prefixes: Vec<String>,
    pub model_group_max_units: usize,
    pub model_group_max_utf8_bytes: usize,
    pub read_workers: usize,
    pub parse_workers: usize,
    pub memory_mib: u64,
    pub system_reserve_mib: u64,
    pub parent_memory_mib: u64,
    pub worker_memory_mib: u64,
    pub run_seconds: u64,
    pub copy_seconds: u64,
    pub parse_seconds: u64,
    pub max_input_mib: u64,
    pub max_output_mib: u64,
    pub warn_free_mib: u64,
    pub reserve_free_mib: u64,
    /// Keys are worksheet/table labels; values are exact header names.
    pub table_keys: BTreeMap<String, Vec<String>>,
    pub daily_source: String,
}

impl Default for ArchiveSettings {
    fn default() -> Self {
        Self {
            collection_interval_minutes: 30,
            compress_extractions: true,
            roots: vec![],
            model_roots: vec![],
            allow_work_dir_for_model: true,
            model_ignored_prefixes: vec![".tmp-".into()],
            model_group_max_units: 16,
            model_group_max_utf8_bytes: 8192,
            read_workers: 2,
            parse_workers: 5,
            memory_mib: 3072,
            system_reserve_mib: 2048,
            parent_memory_mib: 512,
            worker_memory_mib: 512,
            run_seconds: 1200,
            copy_seconds: 120,
            parse_seconds: 300,
            max_input_mib: 512,
            max_output_mib: 256,
            warn_free_mib: 20480,
            reserve_free_mib: 5120,
            table_keys: BTreeMap::new(),
            daily_source: "scan".into(),
        }
    }
}

impl ArchiveSettings {
    pub fn validate(&self) -> Result<(), ReportError> {
        if !(1..=64).contains(&self.model_group_max_units)
            || !(1024..=32768).contains(&self.model_group_max_utf8_bytes)
            || self.model_ignored_prefixes.iter().any(|prefix| {
                prefix.is_empty() || prefix.len() > 128 || prefix.contains(['/', '\\', '\r', '\n'])
            })
        {
            return Err(super::error("ARCHIVE_CONFIG_INVALID"));
        }
        if !(30..=1440).contains(&self.collection_interval_minutes)
            || 1440 % self.collection_interval_minutes != 0
            || !(1..=2).contains(&self.read_workers)
            || !(1..=5).contains(&self.parse_workers)
            || self.worker_memory_mib != 512
            || self.parent_memory_mib < 512
            || self.system_reserve_mib < 2048
            || self.memory_mib < self.parent_memory_mib + self.worker_memory_mib
            || self.memory_mib > 3072
            || !(1..=1200).contains(&self.run_seconds)
            || !(1..=120).contains(&self.copy_seconds)
            || !(1..=300).contains(&self.parse_seconds)
            || !(1..=512).contains(&self.max_input_mib)
            || !(1..=256).contains(&self.max_output_mib)
            || self.reserve_free_mib > self.warn_free_mib
            || !matches!(self.daily_source.as_str(), "archive" | "scan")
        {
            return Err(super::error("ARCHIVE_CONFIG_INVALID"));
        }
        Ok(())
    }
}
