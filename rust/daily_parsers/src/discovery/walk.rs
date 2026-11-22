use super::*;

pub(super) fn visit_files(
    request: &DiscoveryRequest,
    root: &Path,
    excluded: &[PathBuf],
    patterns: &[Pattern],
    check: &impl Fn(&Path) -> io::Result<()>,
    report: &mut DiscoveryReport,
    mut visit: impl FnMut(PathBuf, &Metadata, EntryAccess, &mut DiscoveryReport) -> io::Result<()>,
) -> io::Result<()> {
    let extensions: Vec<_> = request
        .allowed_extensions
        .iter()
        .map(|value| value.to_lowercase())
        .collect();
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
                report.issues.push(path_issue(
                    DiscoveryIssueKind::OutsideRoot,
                    &dir,
                    "directory is outside work_dir".into(),
                ));
                continue;
            }
            Err(error) => {
                report.issues.push(path_issue(
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
                report.issues.push(path_issue(
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
                report.issues.push(path_issue(
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
                    report.issues.push(path_issue(
                        DiscoveryIssueKind::Walk,
                        &dir,
                        error.to_string(),
                    ));
                    continue;
                }
            };
            let path = entry.path();
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy().to_lowercase();
            check(&path)?;
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    report.issues.push(path_issue(
                        DiscoveryIssueKind::Walk,
                        &path,
                        error.to_string(),
                    ));
                    continue;
                }
            };
            let eligible = has_allowed_extension(&name, &extensions)
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
                    let ordinary = resolved.join(&file_name);
                    excluded.iter().any(|dir| ordinary.starts_with(dir))
                } else {
                    is_excluded_dir(&path, excluded)
                };
                if excluded_path {
                    continue;
                }
            }
            let issue = match inspected {
                Ok((access @ (EntryAccess::Local | EntryAccess::LocalCloud), metadata)) => {
                    if kind.is_dir() {
                        pending.push((path, ancestors.clone()));
                    } else if eligible {
                        visit(path, &metadata, access, report)?;
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
                                Ok(meta) if eligible => {
                                    visit(path, &meta, EntryAccess::Link, report)?
                                }
                                Ok(_) => {}
                                Err(error) => report.issues.push(path_issue(
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
            report
                .issues
                .push(path_issue(issue.0, &path, issue.1.into()));
        }
    }
    Ok(())
}

#[cfg(windows)]
#[derive(Default)]
pub(super) struct RecheckBatch {
    paths: Vec<PathBuf>,
    pool: std::cell::OnceCell<Option<rayon::ThreadPool>>,
}

#[cfg(windows)]
impl RecheckBatch {
    pub(super) const CAPACITY: usize = 512;
    const PARALLEL_MIN: usize = 128;
    const MAX_WORKERS: usize = 4;

    pub(super) fn push(
        &mut self,
        path: PathBuf,
        report: &mut DiscoveryReport,
        check: &impl Fn(&Path) -> io::Result<()>,
        recheck: &(impl Fn(&Path, &mut DiscoveryReport) -> io::Result<()> + Sync),
    ) -> io::Result<()> {
        self.paths.push(path);
        if self.paths.len() == Self::CAPACITY {
            self.flush(report, check, recheck)?;
        }
        Ok(())
    }

    pub(super) fn flush(
        &mut self,
        report: &mut DiscoveryReport,
        check: &impl Fn(&Path) -> io::Result<()>,
        recheck: &(impl Fn(&Path, &mut DiscoveryReport) -> io::Result<()> + Sync),
    ) -> io::Result<()> {
        use rayon::prelude::*;

        if self.paths.len() >= Self::PARALLEL_MIN {
            check(&self.paths[0])?;
            let pool = self.pool.get_or_init(|| {
                let workers = std::thread::available_parallelism()
                    .ok()?
                    .get()
                    .min(Self::MAX_WORKERS);
                if workers < 2 {
                    return None;
                }
                rayon::ThreadPoolBuilder::new()
                    .num_threads(workers)
                    .build()
                    .ok()
            });
            if let Some(pool) = pool {
                let chunk_size = self.paths.len().div_ceil(pool.current_num_threads());
                // Contiguous chunks keep lookups in different directories when
                // the batch spans several directories.
                let paths = &self.paths;
                let chunks = pool.install(|| {
                    paths
                        .par_chunks(chunk_size)
                        .map(|chunk| {
                            let mut verified = DiscoveryReport::default();
                            let result = chunk
                                .iter()
                                .try_for_each(|path| recheck(path, &mut verified));
                            (verified, result)
                        })
                        .collect::<Vec<_>>()
                });
                self.paths.clear();
                let mut result = Ok(());
                for (verified, checked) in chunks {
                    report.files.extend(verified.files);
                    report.issues.extend(verified.issues);
                    result = result.and(checked);
                }
                return result;
            }
        }
        self.paths
            .drain(..)
            .try_for_each(|path| recheck(&path, report))
    }
}
