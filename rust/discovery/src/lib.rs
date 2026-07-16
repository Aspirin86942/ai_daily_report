use chrono::{Local, NaiveDate, TimeZone};
use glob::Pattern;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

#[derive(Debug, Deserialize)]
pub struct DiscoveryRequest {
    pub work_dir: PathBuf,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub allowed_extensions: Vec<String>,
    pub ignored_patterns: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_optional_path_list")]
    pub excluded_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiscoveredFileOut {
    pub file_identity: String,
    pub path: String,
    pub extension: String,
    pub modified_at: String,
    pub size_bytes: u64,
    pub source_version: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryIssueKind {
    Walk,
    Metadata,
    ModifiedTime,
    Canonicalize,
    SourceVersion,
    AbsolutePath,
    Alias,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiscoveryIssue {
    pub kind: DiscoveryIssueKind,
    pub path: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiscoveryReport {
    pub files: Vec<DiscoveredFileOut>,
    pub issues: Vec<DiscoveryIssue>,
}

pub fn discover_files(request: &DiscoveryRequest) -> io::Result<Vec<DiscoveredFileOut>> {
    let report = discover_files_report(request, false, None)?;
    for issue in &report.issues {
        eprintln!("warning: {}", issue.message);
    }
    Ok(report.files)
}

pub fn discover_files_with_diagnostics(request: &DiscoveryRequest) -> io::Result<DiscoveryReport> {
    discover_files_report(request, true, Some(&request.work_dir))
}

fn discover_files_report(
    request: &DiscoveryRequest,
    deduplicate_aliases: bool,
    relative_exclusion_root: Option<&Path>,
) -> io::Result<DiscoveryReport> {
    validate_work_dir(&request.work_dir)?;

    let ignored_patterns = compile_patterns(&request.ignored_patterns)?;
    let excluded_dirs = resolve_excluded_dirs(&request.excluded_dirs, relative_exclusion_root);
    let mut files = Vec::new();
    let mut issues = Vec::new();

    let walker = WalkDir::new(&request.work_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() {
                return true;
            }
            let entry_path = entry.path();
            !is_excluded_dir(entry_path, &excluded_dirs)
        });

    for entry_result in walker {
        let entry = match entry_result {
            Ok(value) => value,
            Err(error) => {
                issues.push(DiscoveryIssue {
                    kind: DiscoveryIssueKind::Walk,
                    path: error.path().map(contract_path_string),
                    message: format!("cannot walk entry: {error}"),
                });
                continue;
            }
        };
        let file_name = entry.file_name().to_string_lossy().to_string();
        let file_name_lower = file_name.to_lowercase();
        if !has_allowed_extension(&file_name_lower, &request.allowed_extensions) {
            continue;
        }
        if matches_ignored_pattern(&file_name_lower, &ignored_patterns) {
            continue;
        }

        let resolved_path = match fs::canonicalize(entry.path()) {
            Ok(value) => value,
            Err(error) => {
                issues.push(path_issue(
                    DiscoveryIssueKind::Canonicalize,
                    entry.path(),
                    format!("cannot canonicalize {}: {error}", entry.path().display()),
                ));
                continue;
            }
        };
        let metadata = match read_candidate_metadata(&resolved_path, entry.path()) {
            Ok(value) => value,
            Err(issue) => {
                issues.push(issue);
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let modified_local = match metadata_modified_local(&metadata) {
            Ok(value) => value,
            Err(error) => {
                issues.push(path_issue(
                    DiscoveryIssueKind::ModifiedTime,
                    entry.path(),
                    format!(
                        "cannot read modified time {}: {error}",
                        entry.path().display()
                    ),
                ));
                continue;
            }
        };
        let modified_naive = modified_local.naive_local();
        if !is_within_date_range(modified_naive, request.start_date, request.end_date)? {
            continue;
        }

        let size_bytes = metadata.len();
        let mtime_ns = match metadata_mtime_ns(&metadata) {
            Ok(value) => value,
            Err(error) => {
                issues.push(path_issue(
                    DiscoveryIssueKind::SourceVersion,
                    entry.path(),
                    format!(
                        "cannot read modified nanoseconds {}: {error}",
                        entry.path().display()
                    ),
                ));
                continue;
            }
        };
        let discovered_path = match absolute_discovered_path(entry.path()) {
            Ok(value) => value,
            Err(error) => {
                issues.push(path_issue(
                    DiscoveryIssueKind::AbsolutePath,
                    entry.path(),
                    format!(
                        "cannot build absolute discovered path {}: {error}",
                        entry.path().display()
                    ),
                ));
                continue;
            }
        };
        let resolved_path_text = contract_path_string(&resolved_path);
        let discovered_path_text = contract_path_string(&discovered_path);
        files.push(DiscoveredFileOut {
            file_identity: bootstrap_file_identity_from_text(&resolved_path_text),
            path: discovered_path_text,
            extension: lower_extension(&discovered_path),
            modified_at: modified_naive.format("%Y-%m-%dT%H:%M:%S%.6f").to_string(),
            size_bytes,
            source_version: build_source_version(mtime_ns, size_bytes),
        });
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    if deduplicate_aliases {
        deduplicate_resolved_aliases(&mut files, &mut issues);
    }
    issues.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.kind.cmp(&right.kind))
            .then(left.message.cmp(&right.message))
    });
    Ok(DiscoveryReport { files, issues })
}

fn path_issue(kind: DiscoveryIssueKind, path: &Path, message: String) -> DiscoveryIssue {
    DiscoveryIssue {
        kind,
        path: Some(contract_path_string(path)),
        message,
    }
}

fn read_candidate_metadata(
    resolved_path: &Path,
    display_path: &Path,
) -> Result<Metadata, DiscoveryIssue> {
    fs::metadata(resolved_path).map_err(|error| {
        path_issue(
            DiscoveryIssueKind::Metadata,
            display_path,
            format!("cannot stat {}: {error}", display_path.display()),
        )
    })
}

fn deduplicate_resolved_aliases(
    files: &mut Vec<DiscoveredFileOut>,
    issues: &mut Vec<DiscoveryIssue>,
) {
    let mut chosen_paths = BTreeMap::new();
    let mut unique_files = Vec::with_capacity(files.len());
    for file in std::mem::take(files) {
        if let Some(chosen_path) = chosen_paths.get(&file.file_identity) {
            issues.push(DiscoveryIssue {
                kind: DiscoveryIssueKind::Alias,
                path: Some(file.path.clone()),
                message: format!(
                    "alias {} resolves to the same file identity as {chosen_path}",
                    file.path
                ),
            });
            continue;
        }
        chosen_paths.insert(file.file_identity.clone(), file.path.clone());
        unique_files.push(file);
    }
    *files = unique_files;
}

fn validate_work_dir(work_dir: &Path) -> io::Result<()> {
    let metadata = fs::metadata(work_dir).map_err(|error| {
        let message = if error.kind() == io::ErrorKind::NotFound {
            format!("work_dir does not exist: {}", work_dir.display())
        } else {
            format!(
                "work_dir is not accessible: {}: {error}",
                work_dir.display()
            )
        };
        io::Error::new(error.kind(), message)
    })?;

    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("work_dir is not a directory: {}", work_dir.display()),
        ));
    }

    Ok(())
}

fn resolve_excluded_dirs(paths: &[PathBuf], relative_root: Option<&Path>) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| {
            let rooted = if path.is_absolute() {
                path.clone()
            } else if let Some(root) = relative_root {
                root.join(path)
            } else {
                path.clone()
            };
            fs::canonicalize(&rooted).unwrap_or(rooted)
        })
        .collect()
}

fn deserialize_optional_path_list<'de, D>(deserializer: D) -> Result<Vec<PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<Vec<PathBuf>>::deserialize(deserializer)?.unwrap_or_default())
}

fn is_within_date_range(
    modified_at: chrono::NaiveDateTime,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> io::Result<bool> {
    Ok(modified_at.date() >= start_date && modified_at.date() <= end_date)
}

fn absolute_discovered_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

fn contract_path_string(path: &Path) -> String {
    #[cfg(windows)]
    {
        normalize_contract_path_text(path.to_string_lossy().as_ref())
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().into_owned()
    }
}

pub fn normalize_contract_path_text(value: &str) -> String {
    if let Some(stripped) = value.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{stripped}");
    }
    if let Some(stripped) = value.strip_prefix(r"\\?\") {
        return stripped.to_string();
    }
    value.to_string()
}

pub fn bootstrap_file_identity(resolved_path: &Path) -> String {
    bootstrap_file_identity_from_text(&contract_path_string(resolved_path))
}

fn bootstrap_file_identity_from_text(resolved_path: &str) -> String {
    format!("bootstrap:{}", resolved_path.to_lowercase())
}

fn has_allowed_extension(file_name_lower: &str, allowed_extensions: &[String]) -> bool {
    let file_name_lower = file_name_lower.to_lowercase();
    allowed_extensions
        .iter()
        .any(|extension| file_name_lower.ends_with(&extension.to_lowercase()))
}

fn compile_patterns(patterns: &[String]) -> io::Result<Vec<Pattern>> {
    patterns
        .iter()
        .map(|pattern| {
            Pattern::new(&pattern.to_lowercase()).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid ignored pattern {pattern}: {error}"),
                )
            })
        })
        .collect()
}

fn matches_ignored_pattern(file_name_lower: &str, patterns: &[Pattern]) -> bool {
    patterns
        .iter()
        .any(|pattern| pattern.matches(file_name_lower))
}

fn is_excluded_dir(path: &Path, excluded_dirs: &[PathBuf]) -> bool {
    let comparable = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    excluded_dirs
        .iter()
        .any(|excluded| comparable == *excluded || comparable.starts_with(excluded))
}

fn lower_extension(path: &Path) -> String {
    path.extension()
        .map(|value| format!(".{}", value.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

fn metadata_modified_local(metadata: &Metadata) -> io::Result<chrono::DateTime<Local>> {
    let modified = metadata.modified()?;
    let duration = modified.duration_since(UNIX_EPOCH).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("modified time is before unix epoch: {error}"),
        )
    })?;
    Local
        .timestamp_opt(duration.as_secs() as i64, duration.subsec_nanos())
        .single()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ambiguous local timestamp"))
}

#[cfg(unix)]
fn metadata_mtime_ns(metadata: &Metadata) -> io::Result<u128> {
    use std::os::unix::fs::MetadataExt;

    Ok((metadata.mtime() as u128) * 1_000_000_000 + metadata.mtime_nsec() as u128)
}

#[cfg(not(unix))]
fn metadata_mtime_ns(metadata: &Metadata) -> io::Result<u128> {
    let modified = metadata.modified()?;
    let duration = modified.duration_since(UNIX_EPOCH).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("modified time is before unix epoch: {error}"),
        )
    })?;
    Ok(duration.as_nanos())
}

pub fn build_source_version(mtime_ns: u128, size_bytes: u64) -> String {
    format!("mtime_ns={mtime_ns}:size={size_bytes}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[allow(dead_code)]
    struct TempFixture {
        root: PathBuf,
    }

    #[cfg(unix)]
    #[allow(dead_code)]
    impl TempFixture {
        fn new(name: &str) -> Self {
            let unique = format!(
                "ai_daily_discovery_{name}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let root = std::env::temp_dir().join(unique);
            std::fs::create_dir(&root).unwrap();
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    #[cfg(unix)]
    impl Drop for TempFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn allowed_extension_is_case_insensitive() {
        assert!(has_allowed_extension("REPORT.MD", &[".md".to_string()]));
        assert!(!has_allowed_extension("REPORT.tmp", &[".md".to_string()]));
    }

    #[test]
    fn ignored_patterns_match_file_name_only() {
        let patterns = compile_patterns(&["~$*".to_string(), "*.tmp".to_string()]).unwrap();

        assert!(matches_ignored_pattern("~$draft.md", &patterns));
        assert!(matches_ignored_pattern("scratch.tmp", &patterns));
        assert!(!matches_ignored_pattern("report.md", &patterns));
    }

    #[test]
    fn excluded_dir_matches_directory_and_children() {
        let root = PathBuf::from("/work/skip");

        assert!(is_excluded_dir(
            Path::new("/work/skip"),
            std::slice::from_ref(&root)
        ));
        assert!(is_excluded_dir(Path::new("/work/skip/nested"), &[root]));
        assert!(!is_excluded_dir(
            Path::new("/work/keep"),
            &[PathBuf::from("/work/skip")]
        ));
    }

    #[test]
    fn discovery_request_defaults_missing_or_null_excluded_dirs() {
        let base = serde_json::json!({
            "work_dir": "/work",
            "start_date": "2026-05-25",
            "end_date": "2026-05-25",
            "allowed_extensions": [".md"],
            "ignored_patterns": []
        });

        let missing: DiscoveryRequest = serde_json::from_value(base.clone()).unwrap();
        let mut with_null = base;
        with_null["excluded_dirs"] = serde_json::Value::Null;
        let null_value: DiscoveryRequest = serde_json::from_value(with_null).unwrap();

        assert!(missing.excluded_dirs.is_empty());
        assert!(null_value.excluded_dirs.is_empty());
    }

    #[test]
    fn discover_files_rejects_missing_work_dir() {
        let today = Local::now().date_naive();
        let missing = std::env::temp_dir().join(format!(
            "ai_daily_discovery_missing_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        if missing.exists() {
            std::fs::remove_dir_all(&missing).unwrap();
        }
        let request = DiscoveryRequest {
            work_dir: missing.clone(),
            start_date: today,
            end_date: today,
            allowed_extensions: vec![".md".to_string()],
            ignored_patterns: vec![],
            excluded_dirs: vec![],
        };

        let error = match discover_files(&request) {
            Ok(_) => panic!("missing work_dir must not be treated as an empty scan"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(
            error.to_string().contains("work_dir does not exist"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn candidate_metadata_failure_is_a_structured_issue() {
        let missing = std::env::temp_dir().join(format!(
            "ai_daily_discovery_missing_candidate_{}_{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let issue = read_candidate_metadata(&missing, &missing)
            .expect_err("missing candidate should return an issue");

        assert_eq!(issue.kind, DiscoveryIssueKind::Metadata);
        assert_eq!(issue.path, Some(contract_path_string(&missing)));
        assert!(issue.message.contains("cannot stat"));
    }

    #[test]
    fn source_version_uses_mtime_ns_and_size() {
        assert_eq!(build_source_version(123, 456), "mtime_ns=123:size=456");
    }

    #[test]
    fn date_range_includes_final_day_nanoseconds_and_excludes_next_day() {
        let start_date = NaiveDate::from_ymd_opt(2026, 5, 24).unwrap();
        let end_date = NaiveDate::from_ymd_opt(2026, 5, 25).unwrap();
        let final_day_last_ns = end_date.and_hms_nano_opt(23, 59, 59, 999_999_999).unwrap();
        let next_day_start = NaiveDate::from_ymd_opt(2026, 5, 26)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();

        assert!(is_within_date_range(final_day_last_ns, start_date, end_date).unwrap());
        assert!(!is_within_date_range(next_day_start, start_date, end_date).unwrap());
    }

    #[test]
    fn date_range_accepts_the_maximum_valid_end_date() {
        let final_instant = NaiveDate::MAX
            .and_hms_nano_opt(23, 59, 59, 999_999_999)
            .unwrap();

        assert!(is_within_date_range(final_instant, NaiveDate::MIN, NaiveDate::MAX).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn discover_files_includes_file_symlink_and_emits_stable_contract() {
        let fixture = TempFixture::new("symlink_contract");
        let work_dir = fixture.path().join("work");
        let target_dir = fixture.path().join("targets");
        std::fs::create_dir(&work_dir).unwrap();
        std::fs::create_dir(&target_dir).unwrap();

        let target = target_dir.join("target.md");
        let regular = work_dir.join("regular.TXT");
        let link = work_dir.join("LINK.MD");
        std::fs::write(&target, "linked").unwrap();
        std::fs::write(&regular, "regular").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let today = Local::now().date_naive();
        let request = DiscoveryRequest {
            work_dir: work_dir.clone(),
            start_date: today,
            end_date: today,
            allowed_extensions: vec![".md".to_string(), ".txt".to_string()],
            ignored_patterns: vec![],
            excluded_dirs: vec![],
        };

        let files = discover_files(&request).unwrap();
        let paths: Vec<&str> = files.iter().map(|item| item.path.as_str()).collect();
        let mut sorted_paths = paths.clone();
        sorted_paths.sort();
        let target_path = target.canonicalize().unwrap().to_string_lossy().to_string();
        let link_path = link.to_string_lossy().to_string();

        assert_eq!(paths, sorted_paths);
        let linked_item = files
            .iter()
            .find(|item| item.path == link_path)
            .expect("file symlink should be discovered through target metadata");
        assert_eq!(linked_item.extension, ".md");
        assert_eq!(
            linked_item.file_identity,
            format!("bootstrap:{}", target_path.to_lowercase())
        );
        assert!(linked_item.source_version.starts_with("mtime_ns="));
        assert!(linked_item.source_version.contains(":size="));
        assert!(chrono::NaiveDateTime::parse_from_str(
            &linked_item.modified_at,
            "%Y-%m-%dT%H:%M:%S%.f"
        )
        .is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn discover_files_keeps_symlink_path_when_target_suffix_differs() {
        let fixture = TempFixture::new("symlink_suffix_mismatch");
        let work_dir = fixture.path().join("work");
        let target_dir = fixture.path().join("targets");
        std::fs::create_dir(&work_dir).unwrap();
        std::fs::create_dir(&target_dir).unwrap();

        let target = target_dir.join("target.txt");
        let link = work_dir.join("report.MD");
        std::fs::write(&target, "linked text").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let today = Local::now().date_naive();
        let request = DiscoveryRequest {
            work_dir,
            start_date: today,
            end_date: today,
            allowed_extensions: vec![".md".to_string()],
            ignored_patterns: vec![],
            excluded_dirs: vec![],
        };

        let files = discover_files(&request).unwrap();
        let target_metadata = std::fs::metadata(&target).unwrap();
        let target_path = target.canonicalize().unwrap().to_string_lossy().to_string();
        let expected_identity = format!("bootstrap:{}", target_path.to_lowercase());

        assert_eq!(files.len(), 1);
        let item = &files[0];
        assert_eq!(item.path, link.to_string_lossy());
        assert_eq!(item.extension, ".md");
        assert_eq!(item.file_identity, expected_identity);
        assert_eq!(item.size_bytes, target_metadata.len());
        assert_eq!(
            item.source_version,
            build_source_version(
                metadata_mtime_ns(&target_metadata).unwrap(),
                target_metadata.len()
            )
        );
    }
}
