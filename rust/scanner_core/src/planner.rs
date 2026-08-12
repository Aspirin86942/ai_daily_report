use crate::classifier::{classify_candidate, ClassificationError, ParserRoute};
use ai_daily_discovery::DiscoveredFileOut;
use ai_daily_scanner_contract::NormalizedScannerSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    Parse(ParserRoute),
    Reject(ClassificationError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub file: DiscoveredFileOut,
    pub action: PlanAction,
    pub timeout_ms: u64,
}

pub fn plan_candidates(
    mut files: Vec<DiscoveredFileOut>,
    profile: &NormalizedScannerSettings,
) -> Vec<PlannedFile> {
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
        .into_iter()
        .map(|file| {
            let timeout_ms = profile
                .execution
                .file_timeout_by_extension_ms
                .get(&file.extension)
                .copied()
                .unwrap_or(profile.execution.file_timeout_ms);
            let action = match classify_candidate(&file, profile) {
                Ok(route) => PlanAction::Parse(route),
                Err(error) => PlanAction::Reject(error),
            };
            PlannedFile {
                file,
                action,
                timeout_ms,
            }
        })
        .collect()
}
