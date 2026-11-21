use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use glob::Pattern;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

mod access;
mod walk;
use access::EntryAccess;

/// Metadata-only recheck before opening an admitted source's data stream.
pub fn local_candidate_is_readable(path: &Path, root: &Path) -> bool {
    matches!(
        access::inspect(path),
        Ok(EntryAccess::Local | EntryAccess::LocalCloud)
    ) && fs::canonicalize(root)
        .ok()
        .zip(fs::canonicalize(path).ok())
        .is_some_and(|(root, path)| path.starts_with(root))
}

#[derive(Debug, Deserialize)]
pub struct DiscoveryRequest {
    pub work_dir: PathBuf,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    #[serde(default)]
    pub window_start_utc: Option<DateTime<Utc>>,
    #[serde(default)]
    pub window_end_utc: Option<DateTime<Utc>>,
    #[serde(default)]
    pub follow_symlinks: bool,
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
    /// Engine-owned SourceGuardV2 wire kind, filled by the scanner core right
    /// after discovery (the discovery crate cannot depend on the core guard
    /// module). `windows_file_id_change_time_v1 | unix_inode_ctime_v1 |
    /// content_sha256_v1 | unavailable`; null means the guard is not produced
    /// yet (pre-wiring) and never for a full-v2 discovery file.
    #[serde(default)]
    pub source_guard_kind: Option<String>,
    /// Engine-owned SourceGuardV2 SHA-256; null when the kind is unavailable.
    #[serde(default)]
    pub source_guard_sha256: Option<String>,
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
    LinkExcluded,
    CloudUnavailable,
    UnknownReparse,
    OutsideRoot,
    Budget,
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

pub fn discover_files_with_diagnostics(request: &DiscoveryRequest) -> io::Result<DiscoveryReport> {
    discover_files_report(request, |_| Ok(()))
}

/// The caller can stop waiting for an in-flight filesystem call. The walk checks
/// this same absolute deadline before starting the next one.
pub fn discover_files_until(
    request: &DiscoveryRequest,
    deadline: Instant,
) -> io::Result<DiscoveryReport> {
    discover_files_report(request, |_| {
        if Instant::now() >= deadline {
            Err(io::ErrorKind::TimedOut.into())
        } else {
            Ok(())
        }
    })
}

fn discover_files_report(
    request: &DiscoveryRequest,
    check: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<DiscoveryReport> {
    validate_window(request)?;
    check(&request.work_dir)?;
    let root_access = access::inspect(&request.work_dir).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                error.kind(),
                format!("work_dir does not exist: {}", request.work_dir.display()),
            )
        } else {
            error
        }
    })?;
    match root_access {
        EntryAccess::Local | EntryAccess::LocalCloud => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "work_dir is unavailable locally or is a link/unknown reparse point",
            ))
        }
    }
    check(&request.work_dir)?;
    validate_work_dir(&request.work_dir)?;
    check(&request.work_dir)?;
    let resolved_root = fs::canonicalize(&request.work_dir)?;

    let ignored_patterns = compile_patterns(&request.ignored_patterns)?;
    let excluded_dirs = resolve_excluded_dirs(&request.excluded_dirs, &request.work_dir, &check)?;
    let mut files = Vec::new();
    let mut issues = Vec::new();
    walk::visit_files(
        request,
        &resolved_root,
        &excluded_dirs,
        &ignored_patterns,
        &check,
        &mut issues,
        |path, issues| {
            // Most eligible files are outside a report's date window. A cheap
            // metadata prefilter avoids resolving every historical path; accepted
            // rows still run the canonicalize + metadata path below unchanged.
            check(path)?;
            if let Ok(metadata) = fs::metadata(path) {
                if !metadata.is_file() {
                    return Ok(());
                }
                if let Ok(in_window) = metadata_in_window(&metadata, request) {
                    if !in_window {
                        return Ok(());
                    }
                }
            }

            check(path)?;
            let resolved_path = match fs::canonicalize(path) {
                Ok(value) => value,
                Err(error) => {
                    issues.push(path_issue(
                        DiscoveryIssueKind::Canonicalize,
                        path,
                        format!("cannot canonicalize {}: {error}", path.display()),
                    ));
                    return Ok(());
                }
            };
            if !resolved_path.starts_with(&resolved_root) {
                issues.push(path_issue(
                    DiscoveryIssueKind::OutsideRoot,
                    path,
                    "resolved candidate is outside work_dir".to_string(),
                ));
                return Ok(());
            }
            check(&resolved_path)?;
            let metadata = match read_candidate_metadata(&resolved_path, path) {
                Ok(value) => value,
                Err(issue) => {
                    issues.push(issue);
                    return Ok(());
                }
            };
            if !metadata.is_file() {
                return Ok(());
            }
            let modified_local = match metadata_modified_local(&metadata) {
                Ok(value) => value,
                Err(error) => {
                    issues.push(path_issue(
                        DiscoveryIssueKind::ModifiedTime,
                        path,
                        format!("cannot read modified time {}: {error}", path.display()),
                    ));
                    return Ok(());
                }
            };
            let modified_naive = modified_local.naive_local();
            if !metadata_in_window(&metadata, request)? {
                return Ok(());
            }

            let size_bytes = metadata.len();
            let mtime_ns = match metadata_mtime_ns(&metadata) {
                Ok(value) => value,
                Err(error) => {
                    issues.push(path_issue(
                        DiscoveryIssueKind::SourceVersion,
                        path,
                        format!(
                            "cannot read modified nanoseconds {}: {error}",
                            path.display()
                        ),
                    ));
                    return Ok(());
                }
            };
            check(path)?;
            let discovered_path = match absolute_discovered_path(path) {
                Ok(value) => value,
                Err(error) => {
                    issues.push(path_issue(
                        DiscoveryIssueKind::AbsolutePath,
                        path,
                        format!(
                            "cannot build absolute discovered path {}: {error}",
                            path.display()
                        ),
                    ));
                    return Ok(());
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
                source_guard_kind: None,
                source_guard_sha256: None,
            });
            Ok(())
        },
    )?;

    files.sort_by(|left, right| left.path.cmp(&right.path));
    deduplicate_resolved_aliases(&mut files, &mut issues);
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

fn resolve_excluded_dirs(
    paths: &[PathBuf],
    root: &Path,
    check: &impl Fn(&Path) -> io::Result<()>,
) -> io::Result<Vec<PathBuf>> {
    paths
        .iter()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| {
            let rooted = root.join(path);
            check(&rooted)?;
            Ok(fs::canonicalize(&rooted).unwrap_or(rooted))
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

fn validate_window(request: &DiscoveryRequest) -> io::Result<()> {
    match (request.window_start_utc, request.window_end_utc) {
        (None, None) if request.start_date <= request.end_date => Ok(()),
        (Some(start), Some(end)) if start < end => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid scan window",
        )),
    }
}

fn metadata_in_window(metadata: &Metadata, request: &DiscoveryRequest) -> io::Result<bool> {
    if let (Some(start), Some(end)) = (request.window_start_utc, request.window_end_utc) {
        let modified = DateTime::<Utc>::from(metadata.modified()?);
        Ok(modified >= start && modified < end)
    } else {
        is_within_date_range(
            metadata_modified_local(metadata)?.naive_local(),
            request.start_date,
            request.end_date,
        )
    }
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

/// Apply the discovery permission boundary to an already archived path without reading it.
pub fn archived_path_allowed(
    path: &Path,
    root: &Path,
    extensions: &[String],
    patterns: &[String],
    excluded: &[PathBuf],
) -> io::Result<bool> {
    let patterns = compile_patterns(patterns)?;
    let excluded_dirs = resolve_excluded_dirs(excluded, root, &|_| Ok(()))?;
    let extension = lower_extension(path);
    let excluded_name = excluded
        .iter()
        .filter(|p| p.components().count() == 1)
        .any(|p| {
            path.components().any(|part| {
                part.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&p.to_string_lossy())
            })
        });
    Ok(extensions.contains(&extension)
        && !excluded_name
        && !is_excluded_dir(path, &excluded_dirs)
        && !path.components().any(|part| {
            matches_ignored_pattern(
                &part.as_os_str().to_string_lossy().to_lowercase(),
                &patterns,
            )
        }))
}

fn matches_ignored_pattern(file_name_lower: &str, patterns: &[Pattern]) -> bool {
    patterns
        .iter()
        .any(|pattern| pattern.matches(file_name_lower))
}

fn is_excluded_dir(path: &Path, excluded_dirs: &[PathBuf]) -> bool {
    if excluded_dirs.is_empty() {
        return false;
    }
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
mod tests;
