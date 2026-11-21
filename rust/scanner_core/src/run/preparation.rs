use super::*;

pub(super) struct PreparedBuild {
    pub(super) work_dir: PathBuf,
    pub(super) profile: NormalizedScannerSettings,
    canonical: crate::store::CanonicalRequest,
    runtime: AttemptRuntime,
}

impl PreparedBuild {
    pub(super) fn begin(
        &self,
        store: &mut ScannerStore,
        request_id: &str,
    ) -> Result<BeginRunOutcome, Diagnostic> {
        let now_ms = run_timestamp()?;
        store
            .begin_run(request_id, &self.canonical, &self.runtime, now_ms)
            .map_err(|error| error.diagnostic(DiagnosticStage::Cache))
    }
}

pub(super) fn prepare_build(request: &BuildContextRequest) -> Result<PreparedBuild, Diagnostic> {
    let work_dir = validate_build_work_dir(&request.work_dir)?;
    let profile = normalize_scanner_settings(&request.scanner_settings, request.report_mode)
        .map_err(|message| {
            diagnostic(
                ErrorCode::InvalidRequest,
                message,
                false,
                DiagnosticStage::Request,
            )
        })?;
    let request_error = |error: StoreError| error.diagnostic(DiagnosticStage::Request);
    let canonical = ScannerStore::canonicalize_request(request, &profile).map_err(request_error)?;
    let runtime = AttemptRuntime::from_request(request).map_err(request_error)?;
    Ok(PreparedBuild {
        work_dir,
        profile,
        canonical,
        runtime,
    })
}

fn validate_build_work_dir(work_dir: &str) -> Result<PathBuf, Diagnostic> {
    let path = Path::new(work_dir);
    let metadata = fs::metadata(path).map_err(|error| {
        let error_code = if error.kind() == std::io::ErrorKind::NotFound {
            ErrorCode::WorkDirNotFound
        } else {
            ErrorCode::DiscoveryEntryUnreadable
        };
        diagnostic(
            error_code,
            "work_dir is unavailable".to_string(),
            error.kind() != std::io::ErrorKind::NotFound,
            DiagnosticStage::Request,
        )
    })?;
    if !metadata.is_dir() {
        return Err(diagnostic(
            ErrorCode::WorkDirNotDirectory,
            "work_dir is not a directory".to_string(),
            false,
            DiagnosticStage::Request,
        ));
    }
    fs::canonicalize(path)
        .map(|canonical| PathBuf::from(normalize_contract_path_text(&canonical.to_string_lossy())))
        .map_err(|_| {
            diagnostic(
                ErrorCode::DiscoveryEntryUnreadable,
                "work_dir could not be canonicalized".to_string(),
                true,
                DiagnosticStage::Request,
            )
        })
}

pub(super) fn split_handshake(
    result: Result<RegisteredWorker, crate::fallback::ParseFailure>,
    backend: &str,
) -> (Option<RegisteredWorker>, Option<Diagnostic>) {
    match result {
        Ok(worker) => (Some(worker), None),
        Err(failure) => {
            let mut diagnostic = failure.diagnostic;
            if diagnostic.backend.0.is_none() {
                diagnostic.backend = Nullable(Some(backend.to_string()));
            }
            (None, Some(diagnostic))
        }
    }
}

pub(super) fn worker_fingerprint(worker: &RegisteredWorker) -> WorkerFingerprint {
    WorkerFingerprint {
        contract: worker.identity.worker_contract_version.clone(),
        version: worker.identity.worker_version.clone(),
        build: worker.identity.worker_build.clone(),
    }
}

pub(super) fn route_stack_fingerprints(
    office_worker: &RegisteredWorker,
    pdf_worker: &RegisteredWorker,
) -> Result<RouteStackFingerprints, String> {
    Ok(RouteStackFingerprints {
        text_like: RouteStackFingerprint::text(crate::PARSER_CONTENT_IDENTITY)?,
        modern_office: RouteStackFingerprint::modern_office(
            crate::PARSER_CONTENT_IDENTITY,
            &office_worker.identity.worker_contract_version,
            &office_worker.cache_identity()?,
        )?,
        pdf: RouteStackFingerprint::pdf(
            crate::PARSER_CONTENT_IDENTITY,
            &pdf_worker.identity.worker_contract_version,
            &pdf_worker.cache_identity()?,
        )?,
    })
}

fn source_guard_from_wire(kind: Option<&str>, hash: Option<&str>) -> Option<SourceGuardV2> {
    let guard = SourceGuardV2 {
        kind: source_guard_kind_from_text(kind?)?,
        guard_sha256: hash.map(str::to_string),
    };
    guard.validate().ok()?;
    Some(guard)
}

pub(super) fn discovery_guards_are_current(
    files: &[DiscoveredFileOut],
    observer: &SourceGuardObserver,
) -> bool {
    let mut all_current = true;
    for file in files {
        let Some(expected) = source_guard_from_wire(
            file.source_guard_kind.as_deref(),
            file.source_guard_sha256.as_deref(),
        ) else {
            all_current = false;
            continue;
        };
        if expected.kind == SourceGuardKind::Unavailable
            || !observer.verify(Path::new(&file.path), &expected)
        {
            all_current = false;
        }
    }
    all_current
}

pub(super) fn artifact_guards_are_current(
    artifact: &ArtifactDraft,
    files: &[DiscoveredFileOut],
    observer: &SourceGuardObserver,
) -> bool {
    let mut all_current = artifact.snapshot_eligible && artifact.file_rows.len() == files.len();
    let mut rows = std::collections::HashMap::with_capacity(artifact.file_rows.len());
    for row in &artifact.file_rows {
        if rows.insert(row.file_identity.as_str(), row).is_some() {
            all_current = false;
        }
    }
    for file in files {
        let discovery_guard = source_guard_from_wire(
            file.source_guard_kind.as_deref(),
            file.source_guard_sha256.as_deref(),
        );
        if let Some(expected) = discovery_guard.as_ref() {
            if expected.kind == SourceGuardKind::Unavailable
                || !observer.verify(Path::new(&file.path), expected)
            {
                all_current = false;
            }
        } else {
            all_current = false;
        }

        let Some(row) = rows.remove(file.file_identity.as_str()) else {
            all_current = false;
            continue;
        };
        let artifact_guard = source_guard_from_wire(
            row.source_guard_kind.as_deref(),
            row.source_guard_sha256.as_deref(),
        );
        if row.legacy_source_version != file.source_version
            || artifact_guard.is_none()
            || artifact_guard != discovery_guard
        {
            all_current = false;
        }
    }
    all_current && rows.is_empty()
}

/// Computes the engine-owned SourceGuardV2 for every discovered file and
/// carries it on the discovery output so cache/snapshot identity can consume
/// it. A hard guard I/O error leaves the file unavailable (fail closed), never
/// an invented metadata identity.
pub(super) fn attach_source_guards(
    files: &mut [DiscoveredFileOut],
    observer: &SourceGuardObserver,
) {
    for file in files {
        let guard = observer
            .compute(Path::new(&file.path))
            .unwrap_or(SourceGuardV2 {
                kind: SourceGuardKind::Unavailable,
                guard_sha256: None,
            });
        file.source_guard_kind = Some(source_guard_kind_text(guard.kind).to_string());
        file.source_guard_sha256 = guard.guard_sha256;
    }
}

pub(super) fn discover_with_timeout(
    work_dir: &Path,
    request: &BuildContextRequest,
    profile: &ai_daily_scanner_contract::NormalizedScannerSettings,
    timeout: Duration,
) -> Result<DiscoveryReport, Diagnostic> {
    let deadline = Instant::now() + timeout;
    let start_date = NaiveDate::parse_from_str(&request.start_date, "%Y-%m-%d").map_err(|_| {
        diagnostic(
            ErrorCode::InvalidRequest,
            "start_date is invalid".to_string(),
            false,
            DiagnosticStage::Request,
        )
    })?;
    let end_date = NaiveDate::parse_from_str(&request.end_date, "%Y-%m-%d").map_err(|_| {
        diagnostic(
            ErrorCode::InvalidRequest,
            "end_date is invalid".to_string(),
            false,
            DiagnosticStage::Request,
        )
    })?;
    let discovery_request = DiscoveryRequest {
        work_dir: work_dir.to_path_buf(),
        start_date,
        end_date,
        // Validated by BuildContextRequest before entering discovery.
        window_start_utc: request
            .window_start_utc
            .as_ref()
            .and_then(|value| value.parse().ok()),
        window_end_utc: request
            .window_end_utc
            .as_ref()
            .and_then(|value| value.parse().ok()),
        follow_symlinks: profile.discovery.follow_symlinks,
        allowed_extensions: profile.discovery.allowed_extensions.clone(),
        ignored_patterns: profile.discovery.ignored_patterns.clone(),
        excluded_dirs: profile
            .discovery
            .excluded_dirs
            .iter()
            .map(PathBuf::from)
            .collect(),
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let _ = sender.send(discover_files_until(&discovery_request, deadline));
    });
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(report)) => {
            if handle.join().is_err() {
                Err(diagnostic(
                    ErrorCode::InternalError,
                    "discovery worker panicked".to_string(),
                    false,
                    DiagnosticStage::Discovery,
                ))
            } else {
                Ok(report)
            }
        }
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::TimedOut => {
            Err(discovery_timeout_diagnostic())
        }
        Ok(Err(error)) => {
            let error_code = match error.kind() {
                std::io::ErrorKind::NotFound => ErrorCode::WorkDirNotFound,
                std::io::ErrorKind::InvalidInput => ErrorCode::WorkDirNotDirectory,
                _ => ErrorCode::DiscoveryEntryUnreadable,
            };
            Err(diagnostic(
                error_code,
                "file discovery failed".to_string(),
                error.kind() != std::io::ErrorKind::InvalidInput,
                DiagnosticStage::Discovery,
            ))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => Err(discovery_timeout_diagnostic()),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(diagnostic(
            ErrorCode::InternalError,
            "file discovery worker exited without a result".to_string(),
            true,
            DiagnosticStage::Discovery,
        )),
    }
}

pub(super) fn discovery_timeout_diagnostic() -> Diagnostic {
    diagnostic(
        ErrorCode::InternalError,
        "file discovery exceeded its configured deadline".to_string(),
        true,
        DiagnosticStage::Discovery,
    )
}

pub(super) fn discovery_issue_diagnostic(issue: &DiscoveryIssue) -> Diagnostic {
    let file_path = issue
        .path
        .as_ref()
        .filter(|path| Path::new(path).is_absolute());
    Diagnostic {
        error_code: ErrorCode::DiscoveryEntryUnreadable,
        message: truncate_chars(&issue.message, 4_096),
        retryable: true,
        stage: DiagnosticStage::Discovery,
        file_path: Nullable(file_path.cloned()),
        backend: Nullable(None),
    }
}
