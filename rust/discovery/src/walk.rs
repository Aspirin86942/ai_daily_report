use super::*;

pub(super) fn visit_files(
    request: &DiscoveryRequest,
    root: &Path,
    excluded: &[PathBuf],
    patterns: &[Pattern],
    check: &impl Fn(&Path) -> io::Result<()>,
    issues: &mut Vec<DiscoveryIssue>,
    mut visit: impl FnMut(&Path, &mut Vec<DiscoveryIssue>) -> io::Result<()>,
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
        if request.follow_symlinks {
            if ancestors.contains(&resolved) {
                issues.push(path_issue(
                    DiscoveryIssueKind::Walk,
                    &dir,
                    "directory link cycle".into(),
                ));
                continue;
            }
            ancestors.push(resolved);
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
                check(&path)?;
                if is_excluded_dir(&path, excluded) {
                    continue;
                }
            } else if !eligible {
                continue;
            }
            check(&path)?;
            let issue = match access::inspect(&path) {
                Ok(EntryAccess::Local | EntryAccess::LocalCloud) => {
                    if kind.is_dir() {
                        pending.push((path, ancestors.clone()));
                    } else if eligible {
                        visit(&path, issues)?;
                    }
                    continue;
                }
                Ok(EntryAccess::Link) if request.follow_symlinks => {
                    check(&path)?;
                    match fs::canonicalize(&path) {
                        Ok(target) if target.starts_with(root) => {
                            check(&target)?;
                            match fs::metadata(&target) {
                                Ok(meta) if meta.is_dir() => {
                                    pending.push((path, ancestors.clone()))
                                }
                                Ok(_) if eligible => visit(&path, issues)?,
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
                Ok(EntryAccess::Link) => {
                    (DiscoveryIssueKind::LinkExcluded, "link traversal disabled")
                }
                Ok(EntryAccess::CloudUnavailable) => (
                    DiscoveryIssueKind::CloudUnavailable,
                    "cloud content unavailable locally; no hydration attempted",
                ),
                Ok(EntryAccess::UnknownReparse) => (
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
