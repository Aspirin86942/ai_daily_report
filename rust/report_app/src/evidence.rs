//! Evidence identities and sending permissions shared by personal reporting.
use crate::config::AppConfig;
use ai_daily_worker_contract::SourceRange;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRef {
    pub source_id: String,
    pub version_id: String,
    pub extraction_id: String,
    pub unit_id: String,
    pub side: String,
    pub raw_sha256: String,
    pub unit_sha256: String,
    pub quote: String,
    pub quote_sha256: String,
    pub unit_char_start: usize,
    pub input_start: usize,
    pub input_end: usize,
    pub ranges: Vec<SourceRange>,
}

fn comparable(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .trim_end_matches('/')
        .to_lowercase()
}
fn within(path: &Path, root: &Path) -> bool {
    let path = comparable(path);
    let root = comparable(root);
    path == root
        || path
            .strip_prefix(&root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
pub fn permitted(config: &AppConfig, path: &Path) -> bool {
    // Additional roots stay local until explicitly granted, including nested roots.
    let allowed = config
        .archive
        .model_roots
        .iter()
        .any(|root| within(path, root))
        || config.archive.allow_work_dir_for_model
            && within(path, &config.paths.work_dir)
            && !config.archive.roots.iter().any(|root| within(path, root));
    if !allowed {
        return false;
    }
    let Ok(profile) = ai_daily_scanner_core::config::normalize_scanner_settings(
        &config.scanner,
        ai_daily_scanner_contract::ReportMode::Daily,
    ) else {
        return false;
    };
    let excluded: Vec<_> = profile
        .discovery
        .excluded_dirs
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    ai_daily_discovery::archived_path_allowed(
        path,
        &config.paths.work_dir,
        &profile.discovery.allowed_extensions,
        &profile.discovery.ignored_patterns,
        &excluded,
    )
    .unwrap_or(false)
}
