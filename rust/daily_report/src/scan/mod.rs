mod allocation;
pub mod cache;
pub mod config;
mod evidence;
mod parse;
pub mod pdf;
mod scheduler;
mod source_guard;
pub mod sources;
#[cfg(test)]
mod tests;

use self::{
    cache::{
        CacheBatch, CacheKey, CacheReference, CacheSnapshot, CachedNegative, CachedParse, ScanCache,
    },
    parse::{Candidate, Completed, Outcome, PDF},
    pdf::{pdf_worker_identity, PdfClient},
    scheduler::Scheduler,
};
use crate::ReportError;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use daily_parsers::{
    discovery::{self, DiscoveryIssueKind, DiscoveryRequest},
    pdf_wire::file_identity_sha256,
    settings::{
        Diagnostic, DiagnosticStage, EngineStatus, ErrorCode, NormalizedScannerSettings,
        ParseStatus, RefillRoundAudit, ReportMode, ScannerSettings,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    fs::File,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub use crate::infra::json::digest as sha256_hex;
pub use daily_parsers::PARSER_CONTENT_IDENTITY;
pub use evidence::{PdfRuntime, ScanResult, ScanStats};
pub use sources::{ScanBenchFile, ScannedFile, SourceProfile};

const SNAPSHOT_POLICY: &str = "scan_snapshot_v4";

pub type ScannerError = ReportError;

#[derive(Debug, Serialize)]
pub struct ScannerDoctorReport {
    pub status: EngineStatus,
    pub checks: Vec<ScannerDoctorCheck>,
}

#[derive(Debug, Serialize)]
pub struct ScannerDoctorCheck {
    pub name: &'static str,
    pub status: EngineStatus,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub work_dir: PathBuf,
    pub scan_db_path: PathBuf,
    pub scanner_settings: ScannerSettings,
    pub pdf_worker_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub utc_window: Option<(DateTime<Utc>, DateTime<Utc>)>,
}

pub struct Scanner {
    work_dir: String,
    cache_dir: PathBuf,
    pdf_worker_path: PathBuf,
    daily_profile: NormalizedScannerSettings,
    run_lock: Mutex<Option<VerifiedPdf>>,
}

struct VerifiedPdf {
    stamps: (String, String),
    identity: String,
}

impl Scanner {
    pub fn open(config: ScannerConfig) -> Result<Self, ScannerError> {
        let daily_profile =
            config::normalize_scanner_settings(&config.scanner_settings, ReportMode::Daily)
                .map_err(invalid_configuration)?;
        if !config.pdf_worker_path.is_absolute()
            || config
                .pdf_worker_path
                .to_str()
                .is_none_or(|path| path.contains('\0') || path.chars().count() > 32_767)
        {
            return Err(invalid_configuration(
                "pdf_worker_path must be an absolute path",
            ));
        }
        let mut cache_dir = config.scan_db_path;
        if !config.work_dir.is_absolute()
            || !cache_dir.is_absolute()
            || !cache_dir.ends_with(cache::CACHE_FILENAME)
        {
            return Err(invalid_configuration(format!(
                "work_dir must be absolute and scan_db_path must name an absolute {}",
                cache::CACHE_FILENAME
            )));
        }
        cache_dir.pop();
        let work_dir = config
            .work_dir
            .into_os_string()
            .into_string()
            .map_err(|_| invalid_configuration("work_dir must be valid Unicode"))?;
        Ok(Self {
            work_dir,
            cache_dir,
            pdf_worker_path: config.pdf_worker_path,
            daily_profile,
            run_lock: Mutex::new(None),
        })
    }

    pub fn build_context(&self, request: &ScanRequest) -> Result<ScanResult, ScannerError> {
        self.build_context_at(request, Utc::now())
    }

    pub fn build_context_at(
        &self,
        request: &ScanRequest,
        observed_at: DateTime<Utc>,
    ) -> Result<ScanResult, ScannerError> {
        self.build_context_inner(request, observed_at, None)
    }

    /// Collect additional observations for local comparison drivers.
    pub fn build_context_for_benchmark(
        &self,
        request: &ScanRequest,
        observed_at: DateTime<Utc>,
    ) -> Result<(ScanResult, Vec<ScanBenchFile>), ScannerError> {
        let mut files = Vec::new();
        let result = self.build_context_inner(request, observed_at, Some(&mut files))?;
        Ok((result, files))
    }

    fn build_context_inner(
        &self,
        request: &ScanRequest,
        observed_at: DateTime<Utc>,
        benchmark: Option<&mut Vec<ScanBenchFile>>,
    ) -> Result<ScanResult, ScannerError> {
        let started = Instant::now();
        let mut verified_pdf = self.run_lock.try_lock().map_err(|_| {
            ReportError::new(
                "SCAN_ALREADY_RUNNING",
                "another build_context call is active",
            )
        })?;
        let profile = &self.daily_profile;
        let discovery_request = discovery_request(request, profile, &self.work_dir)?;
        self.check_ocr()?;
        let deadline = started + Duration::from_millis(profile.total_deadline_ms);
        let dispatch_deadline = deadline - Duration::from_secs(2);
        let discovery_deadline = dispatch_deadline
            .min(started + Duration::from_millis(profile.execution.discovery_timeout_ms));
        let mut cache = ScanCache::open(&self.cache_dir, deadline)?;
        let root = discovery_request.work_dir.as_path();
        let mut warnings = Vec::new();
        let mut discovery_failed = false;
        let discovery_started = Instant::now();
        let found =
            match discovery::discover_files_partial_until(&discovery_request, discovery_deadline) {
                Ok(found) => {
                    warnings.extend(found.issues.into_iter().map(|issue| {
                        let code = if issue.kind == DiscoveryIssueKind::Budget {
                            ErrorCode::StageDeadlineExhausted
                        } else {
                            ErrorCode::DiscoveryEntryUnreadable
                        };
                        warning(code, DiagnosticStage::Discovery, issue.message, issue.path)
                    }));
                    found.files
                }
                Err(error) => {
                    let code = match error.kind() {
                        std::io::ErrorKind::TimedOut => ErrorCode::StageDeadlineExhausted,
                        std::io::ErrorKind::NotFound => ErrorCode::WorkDirNotFound,
                        std::io::ErrorKind::NotADirectory => ErrorCode::WorkDirNotDirectory,
                        _ => ErrorCode::DiscoveryEntryUnreadable,
                    };
                    discovery_failed = code != ErrorCode::StageDeadlineExhausted;
                    warnings.push(warning(
                        code,
                        DiagnosticStage::Discovery,
                        error.to_string(),
                        Some(self.work_dir.clone()),
                    ));
                    Vec::new()
                }
            };
        let discovery_ms = parse::elapsed_ms(discovery_started);
        let mut files = ordered_files(found, root, profile, dispatch_deadline);
        let pdf_path = &self.pdf_worker_path;
        let mut pending_pdfs = files
            .iter_mut()
            .filter(|file| file.backend == Some(PDF) && file.completed.is_none())
            .peekable();
        let pdf_identity =
            if profile.max_total_pdf_classification_pages > 0 && pending_pdfs.peek().is_some() {
                match verify_pdf_identity(pdf_path, &mut verified_pdf, dispatch_deadline) {
                    Ok(identity) => Some(identity),
                    Err(error) => {
                        for file in pending_pdfs {
                            file.completed = Some(Completed::new(parse::failed(
                                file,
                                "PARSER_START_FAILED",
                                error.to_string(),
                                true,
                            )));
                        }
                        None
                    }
                }
            } else {
                None
            };
        let snapshot_key =
            snapshot_key(&discovery_request, profile, &files, pdf_identity.as_deref())?;
        let mut scheduler = Scheduler::new(root, profile, dispatch_deadline, pdf_path);
        let snapshot_started = Instant::now();
        let snapshot_source = if warnings.is_empty() && Instant::now() < dispatch_deadline {
            scheduler.restore_snapshot(
                &mut files,
                &mut cache,
                &snapshot_key,
                pdf_identity.as_deref(),
            )?
        } else {
            None
        };
        let mut snapshot_ms = parse::elapsed_ms(snapshot_started);
        let mut batch = CacheBatch::default();
        let mut parse_ms = 0;
        if snapshot_source.is_none() {
            let parse_started = Instant::now();
            scheduler.scan(&mut files, &mut cache, pdf_identity.as_deref())?;
            parse_ms = parse::elapsed_ms(parse_started);
            if warnings.is_empty() && Instant::now() < dispatch_deadline {
                let manifest_started = Instant::now();
                batch.snapshot = snapshot_manifest(&files, profile, &scheduler, snapshot_key)?;
                snapshot_ms += parse::elapsed_ms(manifest_started);
            }
        }
        let scan = evidence::Scan {
            files: &files,
            observed_at,
            profile,
            warnings,
            scheduler: &scheduler,
            discovery_failed,
            snapshot_hit: snapshot_source.is_some(),
            discovery_ms,
            parse_ms,
            snapshot_ms,
        };
        let mut result = match benchmark {
            Some(files) => evidence::assemble::<true>(scan, files),
            None => evidence::assemble::<false>(scan, &mut Vec::new()),
        };
        for completed in files
            .into_iter()
            .flat_map(|file| [file.completed, file.ocr])
            .flatten()
        {
            if completed.reused {
                continue;
            }
            let Some(query) = completed.query else {
                continue;
            };
            match completed.outcome {
                Outcome::Parsed {
                    result,
                    source_sha256,
                    ..
                } => batch.parsed.push(CachedParse {
                    query,
                    result: *result,
                    source_sha256,
                    pdf_observation: completed.pdf.and_then(parse::PdfScan::into_observation),
                }),
                Outcome::Negative(kind) => {
                    if let Some(evidence) = completed.pdf.and_then(|pdf| pdf.into_negative(kind)) {
                        batch.negatives.push(CachedNegative { query, evidence });
                    }
                }
                _ => {}
            }
        }
        batch.hits = std::mem::take(&mut scheduler.hits);
        result.stats.refill_rounds = std::mem::take(&mut scheduler.rounds);
        drop(scheduler);
        let commit_started = Instant::now();
        if let Err(error) = cache.commit(batch) {
            if result.status == EngineStatus::Ok {
                result.status = EngineStatus::Partial;
            }
            result.warnings.push(warning(
                ErrorCode::CacheWriteFailed,
                DiagnosticStage::Cache,
                error.to_string(),
                None,
            ));
        }
        result.stats.cache_commit_ms = parse::elapsed_ms(commit_started);
        result.stats.total_duration_ms = parse::elapsed_ms(started);
        Ok(result)
    }

    pub fn doctor(&self) -> Result<ScannerDoctorReport, ScannerError> {
        let deadline = Instant::now() + Duration::from_secs(30);
        self.check_ocr()?;
        ScanCache::open(&self.cache_dir, deadline)?.initialize()?;
        ScanCache::check(&self.cache_dir, deadline)?;
        let worker_path = &self.pdf_worker_path;
        let identity = pdf_worker_identity(worker_path)
            .map_err(|error| ReportError::new("SCANNER_PDF_IDENTITY_FAILED", error.to_string()))?;
        PdfClient::new(worker_path.clone(), &self.daily_profile, Default::default())
            .check(deadline)
            .map_err(|error| ReportError::new("SCANNER_PDF_HANDSHAKE_FAILED", error.to_string()))?;
        let check = |name, message| ScannerDoctorCheck {
            name,
            status: EngineStatus::Ok,
            message,
        };
        Ok(ScannerDoctorReport {
            status: EngineStatus::Ok,
            checks: vec![
                check(
                    "scan_cache_v4",
                    "schema and SQLite quick_check passed".into(),
                ),
                check(
                    "office_parser",
                    format!("in-process parser build {PARSER_CONTENT_IDENTITY}"),
                ),
                check(
                    "pdf_worker",
                    format!(
                        "handshake passed, worker {}, PDFium {}",
                        identity.worker_sha256, identity.pdfium_sha256
                    ),
                ),
            ],
        })
    }

    fn check_ocr(&self) -> Result<(), ScannerError> {
        if self.daily_profile.parse.pdf.image_ocr.is_some() && !daily_parsers::OCR_AVAILABLE {
            return Err(ReportError::new(
                "SCANNER_OCR_UNAVAILABLE",
                "OCR is unavailable for this scan profile",
            ));
        }
        Ok(())
    }
}

fn discovery_request(
    request: &ScanRequest,
    profile: &NormalizedScannerSettings,
    work_dir: &str,
) -> Result<DiscoveryRequest, ScannerError> {
    let ScanRequest {
        start_date,
        end_date,
        utc_window,
    } = *request;
    if start_date > end_date || start_date.year() < 0 || end_date.year() > 9999 {
        return Err(invalid_configuration("invalid scan date range"));
    }
    let (window_start_utc, window_end_utc) = match utc_window {
        Some((start, end)) => {
            let valid = |t: DateTime<Utc>| {
                let nanos = t.timestamp_subsec_nanos();
                (1..=9999).contains(&t.year())
                    && nanos < 1_000_000_000
                    && nanos.is_multiple_of(1_000)
            };
            if start >= end || !valid(start) || !valid(end) {
                return Err(invalid_configuration("invalid UTC observation window"));
            }
            (start, end)
        }
        None => (
            crate::types::utc_window(start_date)?.0,
            crate::types::utc_window(end_date)?.1,
        ),
    };
    let discovery = &profile.discovery;
    Ok(DiscoveryRequest {
        work_dir: PathBuf::from(discovery::normalize_contract_path_text(work_dir)),
        start_date,
        end_date,
        window_start_utc: Some(window_start_utc),
        window_end_utc: Some(window_end_utc),
        follow_symlinks: discovery.follow_symlinks,
        allowed_extensions: discovery.allowed_extensions.clone(),
        ignored_patterns: discovery.ignored_patterns.clone(),
        excluded_dirs: discovery.excluded_dirs.iter().map(PathBuf::from).collect(),
    })
}

fn ordered_files(
    found: Vec<discovery::DiscoveredFileOut>,
    root: &Path,
    profile: &NormalizedScannerSettings,
    deadline: Instant,
) -> Vec<Candidate> {
    let mut files: Vec<_> = found
        .into_iter()
        .map(|file| {
            let path = Path::new(&file.path);
            let relative_path = path
                .strip_prefix(root)
                .map_or_else(
                    |_| path.file_name().unwrap_or_default().to_string_lossy(),
                    Path::to_string_lossy,
                )
                .replace('\\', "/");
            let backend = parse::backend(&file.extension, profile);
            let priority = priority(&relative_path, &file.extension);
            Candidate {
                file,
                relative_path,
                priority,
                backend,
                guard: None,
                completed: None,
                ocr: None,
            }
        })
        .collect();
    files.sort_by_cached_key(|file| {
        (
            file.priority,
            file.relative_path.to_lowercase(),
            file.relative_path.clone(),
            file.file.file_identity.clone(),
        )
    });
    let mut slots = profile.max_candidate_files;
    for file in &mut files {
        let outcome = if file.file.size_bytes > parse::file_size_limit(profile, file.backend) {
            Outcome::Omitted("file_size_policy")
        } else if file.backend.is_none() {
            if matches!(file.file.extension.as_str(), ".doc" | ".ppt") {
                Outcome::Omitted("legacy_extension_disabled")
            } else {
                parse::failed(
                    file,
                    "PROFILE_ROUTE_INVARIANT",
                    "file type has no in-process parser".into(),
                    false,
                )
            }
        } else if slots == 0 {
            Outcome::Omitted("semantic_file_quota_exhausted")
        } else {
            match source_guard::observe(&file.file, root, deadline) {
                Ok(guard) => {
                    slots -= 1;
                    file.guard = Some(guard);
                    continue;
                }
                Err(error) => parse::source_error(file, &error),
            }
        };
        file.completed = Some(Completed::new(outcome));
    }
    files
}

fn priority(relative_path: &str, extension: &str) -> u64 {
    let path = format!("/{}/", relative_path.to_lowercase());
    if path.contains("/.pytest_cache/") || path.contains("/data/benchmarks/") {
        70
    } else if path.contains("/logs/") {
        60
    } else {
        match extension {
            ".doc" | ".docx" | ".pdf" | ".ppt" | ".pptx" | ".xls" | ".xlsm" | ".xlsx" => 20,
            ".md" | ".txt" => 30,
            _ => 50,
        }
    }
}

fn verify_pdf_identity(
    path: &Path,
    current: &mut Option<VerifiedPdf>,
    deadline: Instant,
) -> Result<String, ScannerError> {
    source_guard::check_deadline(deadline)?;
    let path = path
        .canonicalize()
        .map_err(|error| ReportError::new("SCANNER_PDF_IDENTITY_FAILED", error.to_string()))?;
    let stamps = || {
        let stamp =
            |path: &Path| file_identity_sha256(&File::open(path).ok()?).map(|(_, hash)| hash);
        Some((stamp(&path)?, stamp(&path.with_file_name("pdfium.dll"))?))
    };
    let before = stamps();
    if let Some(cached) = current
        .as_ref()
        .filter(|cached| Some(&cached.stamps) == before.as_ref())
    {
        return Ok(cached.identity.clone());
    }
    let identity = pdf_worker_identity(&path)
        .map_err(|error| ReportError::new("SCANNER_PDF_IDENTITY_FAILED", error.to_string()))?;
    source_guard::check_deadline(deadline)?;
    if before != stamps() {
        return Err(ReportError::new(
            "SCANNER_PDF_IDENTITY_FAILED",
            "PDF bundle changed during verification",
        ));
    }
    let identity = sha256_hex(&serde_json::to_vec(&(
        identity.worker_sha256,
        identity.pdfium_sha256,
    ))?);
    *current = before.map(|stamps| VerifiedPdf {
        stamps,
        identity: identity.clone(),
    });
    Ok(identity)
}

fn snapshot_key(
    request: &DiscoveryRequest,
    profile: &NormalizedScannerSettings,
    files: &[Candidate],
    pdf_identity: Option<&str>,
) -> Result<String, ScannerError> {
    let candidates: Vec<_> = files
        .iter()
        .map(|file| {
            (
                &file.relative_path,
                &file.file.file_identity,
                &file.file.source_version,
                file.guard.as_ref().map(|guard| (guard.kind, &guard.sha256)),
            )
        })
        .collect();
    let mut rules = profile.clone();
    // Scheduling resources affect when work finishes, never which inputs are admitted.
    rules.execution.max_workers = 1;
    Ok(sha256_hex(&serde_json::to_vec(&(
        SNAPSHOT_POLICY,
        PARSER_CONTENT_IDENTITY,
        pdf_identity,
        &request.work_dir,
        request.start_date,
        request.end_date,
        request.window_start_utc,
        request.window_end_utc,
        config::SnapshotRules(&rules),
        candidates,
    ))?))
}

// Keep the v4 JSON tuple and the eight-byte origin prefix readable by older packages.
#[derive(PartialEq, Serialize, Deserialize)]
struct SnapshotRow<'a>(
    Cow<'a, str>,
    ParseStatus,
    Option<Cow<'a, str>>,
    Option<Cow<'a, CacheKey>>,
    Option<(u64, bool)>,
);

impl<'a> SnapshotRow<'a> {
    fn new(file: &'a Candidate, complete: &'a Completed) -> Self {
        Self(
            Cow::Borrowed(&file.relative_path),
            complete.outcome.status(),
            complete.outcome.reason().map(Cow::Borrowed),
            complete
                .query
                .as_ref()
                .map(|query| Cow::Borrowed(&query.key)),
            complete
                .pdf
                .as_ref()
                .map(|pdf| (pdf.charged_pages, pdf.extraction_slot)),
        )
    }
}

type SnapshotManifest<'a> = (
    Cow<'a, str>,
    u64,
    u64,
    u64,
    u64,
    Cow<'a, [RefillRoundAudit]>,
    Vec<SnapshotRow<'a>>,
);

fn snapshot_limits(profile: &NormalizedScannerSettings) -> [u64; 4] {
    [
        profile.max_candidate_files,
        profile.max_pdf_text_extractions,
        profile.max_total_pdf_classification_pages,
        evidence::char_budget(profile),
    ]
}

fn snapshot_reference(complete: &Completed) -> Option<(&CacheKey, bool)> {
    let key = &complete.query.as_ref()?.key;
    match complete.outcome {
        Outcome::Parsed { .. } => Some((key, true)),
        Outcome::Negative(_) => Some((key, false)),
        _ => None,
    }
}

fn snapshot_manifest(
    files: &[Candidate],
    profile: &NormalizedScannerSettings,
    scheduler: &Scheduler<'_>,
    key: String,
) -> Result<Option<CacheSnapshot>, ScannerError> {
    let mut rows = Vec::with_capacity(files.len());
    let mut references = Vec::new();
    for (file, complete) in files
        .iter()
        .flat_map(|file| file.results().map(move |complete| (file, complete)))
    {
        if matches!(complete.outcome, Outcome::Failed { .. })
            || (matches!(complete.outcome, Outcome::Omitted(_)) && complete.pdf.is_some())
        {
            return Ok(None);
        }
        if let Some((key, parsed)) = snapshot_reference(complete) {
            references.push(if parsed {
                CacheReference::Parse(key.clone())
            } else {
                CacheReference::Negative(key.clone())
            });
        }
        rows.push(SnapshotRow::new(file, complete));
    }
    let [files, extractions, pages, chars] = snapshot_limits(profile);
    let plan: SnapshotManifest<'_> = (
        Cow::Borrowed(SNAPSHOT_POLICY),
        files,
        extractions,
        pages,
        chars,
        Cow::Borrowed(&scheduler.rounds),
        rows,
    );
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let origin = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX).max(1);
    let mut manifest = origin.to_le_bytes().to_vec();
    serde_json::to_writer(&mut manifest, &plan)?;
    Ok(Some(CacheSnapshot {
        key,
        manifest,
        references,
    }))
}

fn warning(
    code: ErrorCode,
    stage: DiagnosticStage,
    message: String,
    file_path: Option<String>,
) -> Diagnostic {
    Diagnostic {
        error_code: code,
        stage,
        message,
        retryable: true,
        file_path,
        backend: None,
    }
}

fn invalid_configuration(message: impl Into<String>) -> ScannerError {
    ReportError::new("SCANNER_INVALID_CONFIGURATION", message)
}
