//! Single ordering implementation for the deterministic admission plans.
//!
//! `nominal_rank` is the ONLY sort-key producer for planning, quota allocation,
//! result merging, decisions and final presentation (spec Part 1.1). Parse
//! failures change status/action/reason but never the position, so the plan is
//! cache-independent: the same discovery snapshot + profile always yields the
//! same ordering.

pub const PRIORITY_POLICY_VERSION: &str = "budget_nominal_v2";

const OFFICE_OR_PDF_EXTENSIONS: &[&str] = &[
    ".doc", ".docx", ".pdf", ".ppt", ".pptx", ".xls", ".xlsm", ".xlsx",
];
const TEXT_EXTENSIONS: &[&str] = &[".md", ".txt"];

/// Returns `(priority, lower_path, path, file_identity)`.
///
/// `lower_path` is the Unicode-lowercased relative path used for the stable
/// tie-break; `path` is the caller's original relative path; `file_identity`
/// is unknown to this signature and therefore empty — use [`NominalKey::new`]
/// when the discovery identity is available (the admission plans do).
pub fn nominal_rank(relative_path: &str, extension: &str) -> (u64, String, String, String) {
    let lower = normalize_lower(relative_path);
    let priority = priority_for(&lower, extension);
    (
        priority,
        relative_path.to_lowercase(),
        relative_path.to_string(),
        String::new(),
    )
}

/// Full ordering key including the discovery file identity.
///
/// The four-tuple matches the frozen sort key
/// `(priority, relative_path.to_lowercase(), relative_path, file_identity)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NominalKey {
    pub priority: u64,
    pub lower_path: String,
    pub path: String,
    pub file_identity: String,
}

impl NominalKey {
    pub fn new(relative_path: &str, extension: &str, file_identity: &str) -> Self {
        let lower = normalize_lower(relative_path);
        NominalKey {
            priority: priority_for(&lower, extension),
            lower_path: relative_path.to_lowercase(),
            path: relative_path.to_string(),
            file_identity: file_identity.to_string(),
        }
    }
}

/// Normalizes the relative path for priority matching only: `/` -> `\`, trim
/// leading/trailing separators, Unicode lowercase.
fn normalize_lower(relative_path: &str) -> String {
    relative_path
        .replace('/', "\\")
        .trim_matches('\\')
        .to_lowercase()
}

/// Priority by the FIRST matching row of the spec Part 1.1 table. The
/// `path_key = "\\" + lower + "\\"` form matches segments anywhere in the path.
fn priority_for(lower_path: &str, extension: &str) -> u64 {
    let path_key = format!("\\{lower_path}\\");
    if path_key.contains("\\.pytest_cache\\") || path_key.contains("\\data\\benchmarks\\") {
        70
    } else if path_key.contains("\\logs\\") {
        60
    } else if OFFICE_OR_PDF_EXTENSIONS.contains(&extension) {
        20
    } else if TEXT_EXTENSIONS.contains(&extension) {
        30
    } else {
        50
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_uses_normalized_lower_for_priority_but_raw_lower_for_tie_break() {
        // `/` is normalized to `\` for the priority match (logs segment).
        assert_eq!(nominal_rank("a/logs/app.log", ".log").0, 60);
        // the tie-break element is literally relative_path.to_lowercase().
        assert_eq!(nominal_rank("a/B.md", ".md").1, "a/b.md");
    }
}
