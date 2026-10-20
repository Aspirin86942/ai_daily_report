//! Bounded metadata inspection, with an explicit restartable directory cursor.
use super::*;
use std::time::Instant;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiscoveryCursor {
    root: PathBuf,
    stack: Vec<DirectoryPosition>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DirectoryPosition {
    path: PathBuf,
    offset: usize,
}

pub fn discover_files_bounded(
    request: &DiscoveryRequest,
    deadline: Instant,
    max_entries: usize,
) -> io::Result<DiscoveryReport> {
    discover_files_bounded_from(request, deadline, max_entries, None).map(|(report, _, _)| report)
}

/// A page inspects at most `max_entries` new entries; reopening a directory may
/// replay names to reach its saved offset. Replay never reads file metadata or
/// bodies, checks the same deadline, and is reported as incomplete coverage.
/// The standard filesystem API has no portable durable directory seek token.
pub fn discover_files_bounded_from(
    request: &DiscoveryRequest,
    deadline: Instant,
    max_entries: usize,
    cursor: Option<DiscoveryCursor>,
) -> io::Result<(DiscoveryReport, Option<DiscoveryCursor>, usize)> {
    validate_window(request)?;
    let root = fs::canonicalize(&request.work_dir)?;
    if !matches!(
        access::inspect(&root)?,
        EntryAccess::Local | EntryAccess::LocalCloud
    ) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "root unavailable",
        ));
    }
    validate_work_dir(&root)?;
    let resumed = cursor.is_some();
    let mut cursor = cursor.unwrap_or_else(|| DiscoveryCursor {
        root: root.clone(),
        stack: vec![DirectoryPosition {
            path: root.clone(),
            offset: 0,
        }],
    });
    if cursor.root != root
        || cursor.stack.len() > 256
        || cursor.stack.iter().any(|p| {
            !p.path.starts_with(&root)
                || p.path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid discovery cursor",
        ));
    }
    let patterns = compile_patterns(&request.ignored_patterns)?;
    let excluded = resolve_excluded_dirs(&request.excluded_dirs, Some(&request.work_dir));
    let names: Vec<_> = request
        .excluded_dirs
        .iter()
        .filter(|p| p.components().count() == 1)
        .map(|p| p.to_string_lossy().to_lowercase())
        .collect();
    let mut report = DiscoveryReport {
        files: Vec::new(),
        issues: Vec::new(),
    };
    let mut visited = 0;
    'walk: while visited < max_entries && Instant::now() < deadline {
        let Some(position) = cursor.stack.last_mut() else {
            break;
        };
        // Revalidate each resumed directory so redirects cannot escape the root.
        if !local_candidate_is_readable(&position.path, &root) {
            report.issues.push(path_issue(
                DiscoveryIssueKind::LinkExcluded,
                &position.path,
                "directory no longer available locally".into(),
            ));
            cursor.stack.pop();
            continue;
        }
        let mut directory = match fs::read_dir(&position.path) {
            Ok(value) => value,
            Err(error) => {
                report.issues.push(path_issue(
                    DiscoveryIssueKind::Walk,
                    &position.path,
                    error.to_string(),
                ));
                cursor.stack.pop();
                continue;
            }
        };
        for _ in 0..position.offset {
            if Instant::now() >= deadline {
                break 'walk;
            }
            if directory.next().is_none() {
                break;
            }
        }
        loop {
            if visited >= max_entries || Instant::now() >= deadline {
                break 'walk;
            }
            let Some(entry) = directory.next() else {
                cursor.stack.pop();
                break;
            };
            position.offset += 1;
            visited += 1;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    report.issues.push(path_issue(
                        DiscoveryIssueKind::Walk,
                        &position.path,
                        error.to_string(),
                    ));
                    continue;
                }
            };
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if names.contains(&name)
                || is_excluded_dir(&path, &excluded)
                || matches_ignored_pattern(&name, &patterns)
            {
                continue;
            }
            if !local_candidate_is_readable(&path, &root) {
                report.issues.push(path_issue(
                    DiscoveryIssueKind::LinkExcluded,
                    &path,
                    "entry unavailable locally or redirected".into(),
                ));
                continue;
            }
            if Instant::now() >= deadline {
                position.offset -= 1;
                break 'walk;
            }
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    report.issues.push(path_issue(
                        DiscoveryIssueKind::Metadata,
                        &path,
                        error.to_string(),
                    ));
                    continue;
                }
            };
            if metadata.is_dir() {
                if cursor.stack.len() >= 256 {
                    report.issues.push(path_issue(
                        DiscoveryIssueKind::Budget,
                        &path,
                        "directory depth limit".into(),
                    ));
                    break;
                }
                cursor.stack.push(DirectoryPosition { path, offset: 0 });
                break;
            }
            if !metadata.is_file() || !has_allowed_extension(&name, &request.allowed_extensions) {
                continue;
            }
            match file(request, &path, &metadata) {
                Ok(Some(file)) => report.files.push(file),
                Ok(None) => {}
                Err(error) => report.issues.push(path_issue(
                    DiscoveryIssueKind::Metadata,
                    &path,
                    error.to_string(),
                )),
            }
        }
    }
    let next = (!cursor.stack.is_empty()).then_some(cursor);
    if next.is_some() || resumed {
        report.issues.push(path_issue(DiscoveryIssueKind::Budget, &root,
            "paged inventory; membership changes between pages may cause gaps; deletions are not inferred".into()));
    }
    report.files.sort_by(|a, b| a.path.cmp(&b.path));
    deduplicate_resolved_aliases(&mut report.files, &mut report.issues);
    Ok((report, next, visited))
}

fn file(
    request: &DiscoveryRequest,
    path: &Path,
    metadata: &Metadata,
) -> io::Result<Option<DiscoveredFileOut>> {
    if !metadata_in_window(metadata, request)? {
        return Ok(None);
    }
    let resolved = contract_path_string(&fs::canonicalize(path)?);
    Ok(Some(DiscoveredFileOut {
        file_identity: bootstrap_file_identity_from_text(&resolved),
        path: contract_path_string(&absolute_discovered_path(path)?),
        extension: lower_extension(path),
        modified_at: metadata_modified_local(metadata)?
            .naive_local()
            .format("%Y-%m-%dT%H:%M:%S%.6f")
            .to_string(),
        size_bytes: metadata.len(),
        source_version: build_source_version(metadata_mtime_ns(metadata)?, metadata.len()),
        source_guard_kind: None,
        source_guard_sha256: None,
    }))
}
