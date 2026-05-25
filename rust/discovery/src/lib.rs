use chrono::{Local, NaiveDate, TimeZone};
use glob::Pattern;
use serde::{Deserialize, Serialize};
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
    pub excluded_dirs: Vec<PathBuf>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DiscoveredFileOut {
    pub file_identity: String,
    pub path: String,
    pub extension: String,
    pub modified_at: String,
    pub size_bytes: u64,
    pub source_version: String,
}

pub fn discover_files(request: &DiscoveryRequest) -> io::Result<Vec<DiscoveredFileOut>> {
    let ignored_patterns = compile_patterns(&request.ignored_patterns)?;
    let excluded_dirs = resolve_excluded_dirs(&request.excluded_dirs);
    let mut files = Vec::new();

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
                eprintln!("warning: cannot walk entry: {error}");
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

        let metadata = match fs::metadata(entry.path()) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("warning: cannot stat {}: {}", entry.path().display(), error);
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let modified_local = match metadata_modified_local(&metadata) {
            Ok(value) => value,
            Err(error) => {
                eprintln!(
                    "warning: cannot read modified time {}: {}",
                    entry.path().display(),
                    error
                );
                continue;
            }
        };
        let modified_naive = modified_local.naive_local();
        if !is_within_date_range(modified_naive, request.start_date, request.end_date)? {
            continue;
        }

        let resolved_path = match fs::canonicalize(entry.path()) {
            Ok(value) => value,
            Err(error) => {
                eprintln!(
                    "warning: cannot canonicalize {}: {}",
                    entry.path().display(),
                    error
                );
                continue;
            }
        };
        let size_bytes = metadata.len();
        let mtime_ns = match metadata_mtime_ns(&metadata) {
            Ok(value) => value,
            Err(error) => {
                eprintln!(
                    "warning: cannot read modified nanoseconds {}: {}",
                    entry.path().display(),
                    error
                );
                continue;
            }
        };
        files.push(DiscoveredFileOut {
            file_identity: format!(
                "bootstrap:{}",
                resolved_path.to_string_lossy().to_lowercase()
            ),
            path: resolved_path.to_string_lossy().to_string(),
            extension: lower_extension(entry.path()),
            modified_at: modified_naive.format("%Y-%m-%dT%H:%M:%S%.6f").to_string(),
            size_bytes,
            source_version: build_source_version(mtime_ns, size_bytes),
        });
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn resolve_excluded_dirs(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect()
}

fn is_within_date_range(
    modified_at: chrono::NaiveDateTime,
    start_date: NaiveDate,
    end_date: NaiveDate,
) -> io::Result<bool> {
    let start_dt = start_date.and_hms_opt(0, 0, 0).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid start date boundary")
    })?;
    let next_day = end_date
        .succ_opt()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid end date boundary"))?;
    let next_day_start = next_day
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid end date boundary"))?;

    Ok(modified_at >= start_dt && modified_at < next_day_start)
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

fn build_source_version(mtime_ns: u128, size_bytes: u64) -> String {
    format!("mtime_ns={mtime_ns}:size={size_bytes}")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempFixture {
        root: PathBuf,
    }

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

        assert!(is_excluded_dir(Path::new("/work/skip"), &[root.clone()]));
        assert!(is_excluded_dir(Path::new("/work/skip/nested"), &[root]));
        assert!(!is_excluded_dir(
            Path::new("/work/keep"),
            &[PathBuf::from("/work/skip")]
        ));
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

        assert_eq!(paths, sorted_paths);
        let linked_item = files
            .iter()
            .find(|item| item.path == target_path)
            .expect("file symlink should be discovered through target metadata");
        assert_eq!(linked_item.extension, ".md");
        assert!(linked_item.source_version.starts_with("mtime_ns="));
        assert!(linked_item.source_version.contains(":size="));
        assert!(chrono::NaiveDateTime::parse_from_str(
            &linked_item.modified_at,
            "%Y-%m-%dT%H:%M:%S%.f"
        )
        .is_ok());
    }
}
