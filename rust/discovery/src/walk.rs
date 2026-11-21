use super::*;

pub(super) fn visit_files(
    request: &DiscoveryRequest,
    root: &Path,
    excluded: &[PathBuf],
    patterns: &[Pattern],
    check: &impl Fn(&Path) -> io::Result<()>,
    issues: &mut Vec<DiscoveryIssue>,
    mut visit: impl FnMut(&Path, &Metadata, &mut Vec<DiscoveryIssue>) -> io::Result<()>,
) -> io::Result<()> {
    let excluded_names: Vec<_> = request
        .excluded_dirs
        .iter()
        .filter(|p| p.components().count() == 1)
        .filter_map(|p| p.to_str().map(str::to_lowercase))
        .collect();
    let mut pending = vec![(request.work_dir.clone(), Vec::<PathBuf>::new())];
    while let Some((dir, mut ancestors)) = pending.pop() {
        check(&dir)?;
        let resolved = match fs::canonicalize(&dir) {
            Ok(path) if path.starts_with(root) => path,
            Ok(_) => {
                issues.push(path_issue(
                    DiscoveryIssueKind::OutsideRoot,
                    &dir,
                    "directory is outside work_dir".into(),
                ));
                continue;
            }
            Err(error) => {
                issues.push(path_issue(
                    DiscoveryIssueKind::Walk,
                    &dir,
                    error.to_string(),
                ));
                continue;
            }
        };
        // A queued directory may have changed since enumeration.
        if dir != request.work_dir && excluded.iter().any(|path| resolved.starts_with(path)) {
            continue;
        }
        if request.follow_symlinks {
            if ancestors.contains(&resolved) {
                issues.push(path_issue(
                    DiscoveryIssueKind::Walk,
                    &dir,
                    "directory link cycle".into(),
                ));
                continue;
            }
            ancestors.push(resolved.clone());
        }
        check(&dir)?;
        let mut entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                issues.push(path_issue(
                    DiscoveryIssueKind::Walk,
                    &dir,
                    error.to_string(),
                ));
                continue;
            }
        };
        loop {
            check(&dir)?;
            let entry = match entries.next() {
                None => break,
                Some(Ok(entry)) => entry,
                Some(Err(error)) => {
                    issues.push(path_issue(
                        DiscoveryIssueKind::Walk,
                        &dir,
                        error.to_string(),
                    ));
                    continue;
                }
            };
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_lowercase();
            check(&path)?;
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    issues.push(path_issue(
                        DiscoveryIssueKind::Walk,
                        &path,
                        error.to_string(),
                    ));
                    continue;
                }
            };
            let eligible = has_allowed_extension(&name, &request.allowed_extensions)
                && !matches_ignored_pattern(&name, patterns);
            if kind.is_dir() || kind.is_symlink() {
                if excluded_names.contains(&name) {
                    continue;
                }
            } else if !eligible {
                continue;
            }
            check(&path)?;
            let inspected = access::inspect_entry(&entry);
            if !excluded.is_empty() && (kind.is_dir() || kind.is_symlink()) {
                check(&path)?;
                let excluded_path = if matches!(&inspected, Ok((EntryAccess::Local, _))) {
                    let ordinary = resolved.join(entry.file_name());
                    excluded.iter().any(|dir| ordinary.starts_with(dir))
                } else {
                    is_excluded_dir(&path, excluded)
                };
                if excluded_path {
                    continue;
                }
            }
            let issue = match inspected {
                Ok((EntryAccess::Local | EntryAccess::LocalCloud, metadata)) => {
                    if kind.is_dir() {
                        pending.push((path, ancestors.clone()));
                    } else if eligible {
                        visit(&path, &metadata, issues)?;
                    }
                    continue;
                }
                Ok((EntryAccess::Link, _)) if request.follow_symlinks => {
                    check(&path)?;
                    match fs::canonicalize(&path) {
                        Ok(target) if target.starts_with(root) => {
                            check(&target)?;
                            match fs::metadata(&target) {
                                Ok(meta) if meta.is_dir() => {
                                    pending.push((path, ancestors.clone()))
                                }
                                Ok(meta) if eligible => visit(&path, &meta, issues)?,
                                Ok(_) => {}
                                Err(error) => issues.push(path_issue(
                                    DiscoveryIssueKind::Metadata,
                                    &path,
                                    error.to_string(),
                                )),
                            }
                            continue;
                        }
                        _ => (
                            DiscoveryIssueKind::OutsideRoot,
                            "link target is outside work_dir",
                        ),
                    }
                }
                Ok((EntryAccess::Link, _)) => {
                    (DiscoveryIssueKind::LinkExcluded, "link traversal disabled")
                }
                Ok((EntryAccess::CloudUnavailable, _)) => (
                    DiscoveryIssueKind::CloudUnavailable,
                    "cloud content unavailable locally; no hydration attempted",
                ),
                Ok((EntryAccess::UnknownReparse, _)) => (
                    DiscoveryIssueKind::UnknownReparse,
                    "unknown reparse tag excluded",
                ),
                Err(_) => (
                    DiscoveryIssueKind::Metadata,
                    "cannot inspect entry metadata",
                ),
            };
            issues.push(path_issue(issue.0, &path, issue.1.into()));
        }
    }
    Ok(())
}
