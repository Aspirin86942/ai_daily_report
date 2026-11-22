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
use chrono::{DateTime, Duration as Days, FixedOffset, NaiveDate, Utc};
use daily_parsers::{
    discovery::{self, DiscoveryIssueKind, DiscoveryRequest},
    pdf_wire::file_identity_sha256,
    settings::{
        validate_scan_request, AdapterPaths, CompressionProfile, ContextEnvelope, Diagnostic,
        DiagnosticStage, DoctorCheck, DoctorCheckStatus, DoctorResponse, EngineStatus,
        NormalizedScannerSettings, Nullable, ParseStatus, RefillRoundAudit, ReportMode,
        ScannerEvidence, ScannerSettings, Validate,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    fs::File,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use daily_parsers::settings::ErrorCode;
pub use daily_parsers::PARSER_CONTENT_IDENTITY;

const CONTRACT: &str = "ai_daily_context";
const ENGINE_BUILD: &str = env!("AI_DAILY_REPORT_BUILD");
const SNAPSHOT_POLICY: &str = "scan_snapshot_v4";
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub type ScannerError = ReportError;

#[derive(Debug, Clone)]
pub struct ContextResult {
    pub envelope: ContextEnvelope,
    pub(crate) files: Vec<sources::ScannedFile>,
    pub(crate) source_profile: sources::SourceProfile,
    // File audit rows live with their excerpts and are added at the API boundary.
    evidence_summary: ScannerEvidence,
    pub pdf_runtime: Vec<PdfRuntime>,
    pub peak_pdf_requests_in_flight: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PdfRuntime {
    pub relative_path: String,
    pub requests: u64,
    pub native_document_opens: u64,
    pub native_page_reads: u64,
}

#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub work_dir: String,
    pub scan_db_path: String,
    pub scanner_settings: ScannerSettings,
    pub adapters: AdapterPaths,
}

#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub report_mode: ReportMode,
    pub start_date: String,
    pub end_date: String,
    pub window_start_utc: Option<String>,
    pub window_end_utc: Option<String>,
    pub compression_profile: Option<CompressionProfile>,
}

#[derive(Debug)]
pub struct ScannerOperation<T> {
    pub value: T,
    pub exit_code: i32,
}

pub struct Scanner {
    config: ScannerConfig,
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
        config.adapters.validate().map_err(invalid_configuration)?;
        let db = Path::new(&config.scan_db_path);
        if !Path::new(&config.work_dir).is_absolute()
            || !db.is_absolute()
            || db
                .file_name()
                .is_none_or(|name| name != cache::CACHE_FILENAME)
        {
            return Err(invalid_configuration(format!(
                "work_dir must be absolute and scan_db_path must name an absolute {}",
                cache::CACHE_FILENAME
            )));
        }
        Ok(Self {
            config,
            daily_profile,
            run_lock: Mutex::new(None),
        })
    }

    pub fn build_context(
        &self,
        request: &ScanRequest,
    ) -> Result<ScannerOperation<ContextResult>, ScannerError> {
        self.build_context_with_request_id(request, request_id())
    }

    pub fn build_context_with_request_id(
        &self,
        request: &ScanRequest,
        request_id: String,
    ) -> Result<ScannerOperation<ContextResult>, ScannerError> {
        self.build_context_with_request_id_at(request, request_id, Utc::now())
    }

    pub fn build_context_with_request_id_at(
        &self,
        request: &ScanRequest,
        request_id: String,
        observed_at: DateTime<Utc>,
    ) -> Result<ScannerOperation<ContextResult>, ScannerError> {
        let started = Instant::now();
        let mut verified_pdf = self.run_lock.try_lock().map_err(|_| {
            ReportError::new(
                "SCAN_ALREADY_RUNNING",
                "another build_context call is active",
            )
        })?;
        validate_scan_request(
            &request_id,
            &request.start_date,
            &request.end_date,
            request.window_start_utc.as_deref(),
            request.window_end_utc.as_deref(),
            request.report_mode,
            request.compression_profile,
        )
        .map_err(invalid_configuration)?;
        let profile = self.profile(request.report_mode)?;
        let deadline = started + Duration::from_millis(profile.total_deadline_ms);
        let dispatch_deadline = deadline - Duration::from_secs(2);
        let discovery_deadline = dispatch_deadline
            .min(started + Duration::from_millis(profile.execution.discovery_timeout_ms));
        let mut cache = ScanCache::open(self.cache_dir(), deadline)?;
        let discovery_request = discovery_request(request, &profile, &self.config.work_dir)?;
        let root = discovery_request.work_dir.as_path();
        let mut warnings = Vec::new();
        let mut discovery_failed = false;
        let discovery_started = Instant::now();
        let found =
            match discovery::discover_files_partial_until(&discovery_request, discovery_deadline) {
                Ok(found) => {
                    warnings.extend(found.issues.iter().map(|issue| {
                        let code = if issue.kind == DiscoveryIssueKind::Budget {
                            ErrorCode::StageDeadlineExhausted
                        } else {
                            ErrorCode::DiscoveryEntryUnreadable
                        };
                        warning(
                            code,
                            DiagnosticStage::Discovery,
                            issue.message.clone(),
                            issue.path.clone(),
                        )
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
                        Some(self.config.work_dir.clone()),
                    ));
                    Vec::new()
                }
            };
        let discovery_ms = parse::elapsed_ms(discovery_started);
        let mut files = ordered_files(found, root, &profile, dispatch_deadline);
        let pdf_path = Path::new(&self.config.adapters.pdf_worker_path);
        let pdf_identity = if profile.max_total_pdf_classification_pages > 0
            && files
                .iter()
                .any(|file| file.backend == Some(PDF) && file.completed.is_none())
        {
            match verify_pdf_identity(pdf_path, &mut verified_pdf, dispatch_deadline) {
                Ok(identity) => Some(identity),
                Err(error) => {
                    for file in files
                        .iter_mut()
                        .filter(|file| file.backend == Some(PDF) && file.completed.is_none())
                    {
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
        let snapshot_key = snapshot_key(
            &discovery_request,
            &profile,
            &files,
            pdf_identity.as_deref(),
        )?;
        let mut scheduler = Scheduler::new(root, &profile, dispatch_deadline, pdf_path);
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
                batch.snapshot = snapshot_manifest(
                    &files,
                    &profile,
                    &scheduler,
                    snapshot_key,
                    evidence::scan_id(&request_id),
                )?;
                snapshot_ms += parse::elapsed_ms(manifest_started);
            }
        }
        let mut result = evidence::assemble(evidence::Scan {
            files: &files,
            request_id,
            observed_at,
            profile: &profile,
            warnings,
            scheduler: &scheduler,
            discovery_failed,
            snapshot_source,
            started,
            discovery_ms,
            parse_ms,
            snapshot_ms,
        });
        for completed in files
            .into_iter()
            .flat_map(|file| [file.completed, file.ocr])
            .flatten()
        {
            if !completed.reused {
                if let Some(query) = completed.query {
                    match completed.outcome {
                        Outcome::Parsed {
                            result,
                            source_sha256,
                            ..
                        } => batch.parsed.push(CachedParse {
                            query,
                            result: *result,
                            source_sha256,
                            pdf_observation: completed
                                .pdf
                                .and_then(parse::PdfScan::into_observation),
                        }),
                        Outcome::Negative(kind) => {
                            if let Some(evidence) =
                                completed.pdf.and_then(|pdf| pdf.into_negative(kind))
                            {
                                batch.negatives.push(CachedNegative { query, evidence });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        batch.hits = std::mem::take(&mut scheduler.hits);
        drop(scheduler);
        let commit_started = Instant::now();
        if let Err(error) = cache.commit(batch) {
            evidence::add_warning(
                &mut result,
                warning(
                    ErrorCode::CacheWriteFailed,
                    DiagnosticStage::Cache,
                    error.to_string(),
                    None,
                ),
            );
        }
        evidence::finish(&mut result, started, parse::elapsed_ms(commit_started));
        let exit_code = i32::from(result.envelope.status == EngineStatus::Error) * 2;
        Ok(ScannerOperation {
            value: result,
            exit_code,
        })
    }

    pub fn doctor(&self) -> Result<ScannerOperation<DoctorResponse>, ScannerError> {
        self.doctor_with_request_id(request_id())
    }

    pub fn doctor_with_request_id(
        &self,
        request_id: String,
    ) -> Result<ScannerOperation<DoctorResponse>, ScannerError> {
        let deadline = Instant::now() + Duration::from_secs(30);
        let profile = self.profile(ReportMode::Daily)?;
        ScanCache::open(self.cache_dir(), deadline)?.initialize()?;
        ScanCache::check(self.cache_dir(), deadline)?;
        let worker_path = PathBuf::from(&self.config.adapters.pdf_worker_path);
        let identity = pdf_worker_identity(&worker_path)
            .map_err(|error| ReportError::new("SCANNER_PDF_IDENTITY_FAILED", error.to_string()))?;
        PdfClient::new(worker_path, &profile, Default::default())
            .check(deadline)
            .map_err(|error| ReportError::new("SCANNER_PDF_HANDSHAKE_FAILED", error.to_string()))?;
        let checks = vec![
            DoctorCheck {
                name: "scan_cache_v4".into(),
                status: DoctorCheckStatus::Ok,
                message: "schema and SQLite quick_check passed".into(),
            },
            DoctorCheck {
                name: "office_parser".into(),
                status: DoctorCheckStatus::Ok,
                message: format!("in-process parser build {PARSER_CONTENT_IDENTITY}"),
            },
            DoctorCheck {
                name: "pdf_worker".into(),
                status: DoctorCheckStatus::Ok,
                message: format!(
                    "handshake passed, worker {}, PDFium {}",
                    identity.worker_sha256, identity.pdfium_sha256
                ),
            },
        ];
        let value = DoctorResponse {
            contract: CONTRACT.into(),
            protocol_version: 1,
            request_id,
            status: EngineStatus::Ok,
            engine_version: env!("CARGO_PKG_VERSION").into(),
            engine_build: ENGINE_BUILD.into(),
            checks,
            warnings: Vec::new(),
            error: Nullable(None),
        };
        value.validate().map_err(invalid_configuration)?;
        Ok(ScannerOperation {
            value,
            exit_code: 0,
        })
    }

    fn cache_dir(&self) -> &Path {
        Path::new(&self.config.scan_db_path)
            .parent()
            .expect("absolute cache has a directory")
    }

    fn profile(
        &self,
        mode: ReportMode,
    ) -> Result<Cow<'_, NormalizedScannerSettings>, ScannerError> {
        let profile = if mode == ReportMode::Daily {
            Cow::Borrowed(&self.daily_profile)
        } else {
            Cow::Owned(
                config::normalize_scanner_settings(&self.config.scanner_settings, mode)
                    .map_err(invalid_configuration)?,
            )
        };
        if profile.parse.pdf.image_ocr.is_some() && !daily_parsers::OCR_AVAILABLE {
            return Err(ReportError::new(
                "SCANNER_OCR_UNAVAILABLE",
                "OCR is unavailable for this scan profile",
            ));
        }
        Ok(profile)
    }
}

fn discovery_request(
    request: &ScanRequest,
    profile: &NormalizedScannerSettings,
    work_dir: &str,
) -> Result<DiscoveryRequest, ScannerError> {
    let date = |value| {
        NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|err| invalid_configuration(err.to_string()))
    };
    let start_date = date(&request.start_date)?;
    let end_date = date(&request.end_date)?;
    let (window_start_utc, window_end_utc) =
        match (&request.window_start_utc, &request.window_end_utc) {
            (Some(start), Some(end)) => {
                let utc = |value: &str| {
                    DateTime::parse_from_rfc3339(value)
                        .map(|time| time.with_timezone(&Utc))
                        .map_err(|err| invalid_configuration(err.to_string()))
                };
                (utc(start)?, utc(end)?)
            }
            _ => {
                let shanghai = FixedOffset::east_opt(8 * 60 * 60).expect("Shanghai time offset");
                let at_start = |day: NaiveDate| {
                    day.and_hms_opt(0, 0, 0)
                        .expect("day start")
                        .and_local_timezone(shanghai)
                        .single()
                        .expect("fixed-offset time")
                        .with_timezone(&Utc)
                };
                let through = end_date
                    .checked_add_signed(Days::days(1))
                    .ok_or_else(|| invalid_configuration("scan end date overflows".into()))?;
                (at_start(start_date), at_start(through))
            }
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
        &rules,
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
    origin: u64,
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
        file_path: Nullable(file_path),
        backend: Nullable(None),
    }
}

fn invalid_configuration(message: String) -> ScannerError {
    ReportError::new("SCANNER_INVALID_CONFIGURATION", message)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    daily_parsers::hex(&Sha256::digest(bytes))
}

fn request_id() -> String {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = u128::from(REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let mut id = time ^ (u128::from(std::process::id()) << 64) ^ sequence;
    id = (id & !(0xf << 76)) | (4 << 76);
    id = (id & !(0x3 << 62)) | (0x2 << 62);
    let hex = format!("{id:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}
