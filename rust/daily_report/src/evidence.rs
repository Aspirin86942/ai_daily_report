//! Evidence identities and sending permissions shared by personal reporting.
use crate::{config::AppConfig, ReportError};
use daily_parsers::contract::SourceRange;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
    EvidencePermissions::new(config).is_some_and(|policy| policy.permits(path))
}

pub(crate) struct EvidencePermissions<'a> {
    config: &'a AppConfig,
    profile: daily_parsers::settings::DiscoveryProfile,
    excluded: Vec<PathBuf>,
}

impl<'a> EvidencePermissions<'a> {
    pub fn new(config: &'a AppConfig) -> Option<Self> {
        let profile = crate::scan::config::normalize_scanner_settings(
            &config.scanner,
            daily_parsers::settings::ReportMode::Daily,
        )
        .ok()?
        .discovery;
        let excluded = profile.excluded_dirs.iter().map(PathBuf::from).collect();
        Some(Self {
            config,
            profile,
            excluded,
        })
    }

    pub fn permits(&self, path: &Path) -> bool {
        let config = self.config;
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
        daily_parsers::discovery::archived_path_allowed(
            path,
            &config.paths.work_dir,
            &self.profile.allowed_extensions,
            &self.profile.ignored_patterns,
            &self.excluded,
        )
        .unwrap_or(false)
    }
}

pub(crate) fn permission_denied() -> ReportError {
    ReportError::new("ARCHIVE_MODEL_PERMISSION_DENIED", "来源当前未获使用许可")
}

pub(crate) fn denied_sources<'a>(
    config: &AppConfig,
    sources: impl Iterator<Item = &'a crate::reports::data::ReportSource>,
) -> Result<BTreeSet<String>, ReportError> {
    let mut files = sources.filter(|s| s.source_kind == "file").peekable();
    let mut denied = BTreeSet::new();
    if files.peek().is_none() {
        return Ok(denied);
    }
    let current = crate::config::AppConfig::load(&config.program_dir, Some(&config.config_dir))?;
    let initial = EvidencePermissions::new(config);
    let active = EvidencePermissions::new(&current);
    let mut checked = BTreeMap::new();
    for source in files {
        let path = source.coverage_limits["personal"]["absolute_path"]
            .as_str()
            .ok_or_else(|| {
                ReportError::new("ARCHIVE_MODEL_PERMISSION_DENIED", "缺少来源权限信息")
            })?;
        let allowed = checked.entry(path).or_insert_with(|| {
            [&initial, &active].into_iter().all(|policy| {
                policy
                    .as_ref()
                    .is_some_and(|policy| policy.permits(Path::new(path)))
            })
        });
        if !*allowed {
            denied.insert(source.source_id.clone());
        }
    }
    Ok(denied)
}
