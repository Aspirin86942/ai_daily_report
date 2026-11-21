//! BudgetedContextScheduler — the single deep-module execution entry (spec
//! Solution / ID-1).
//!
//! `BudgetedContextScheduler::execute(ScheduledRunInput)` is the only execution
//! interface. It freezes the two immutable plans (ClassificationPlan before any
//! classification I/O, ContentAdmissionPlan before any parse I/O), executes the
//! classified/admitted work under a monotonic WorkDeadline, enforces the
//! spec Part 2 state matrix, renders the context, and returns a complete
//! `BudgetedScanOutcome` with terminal intent. The caller must not re-decide
//! after return.
//!
//! Semantic quotas produce deterministic NotParsed (snapshot-eligible); the
//! WorkDeadline stops new work and forms Partial/Error with runtime NotParsed /
//! Timeout and NO snapshot; the AbsoluteDeadline is enforced by the run shell
//! before any terminal COMMIT.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use ai_daily_discovery::{DiscoveredFileOut, DiscoveryIssue};
use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheMissReason, CacheStatus, ClassificationCacheStatus,
    ClassificationTransport, ContextSummary, Diagnostic, DiagnosticStage, ErrorCode,
    ExtensionMetric, NormalizedScannerSettings, Nullable, ParseStatus, ParseTransport,
    PdfClassificationAuditV1, PdfClassificationStatus, RunStatus, StageMetric, StageName,
};
use rayon::prelude::*;

use crate::admission::{
    AdmissionDecision, ClassificationPlan, ClassifiedPlan, ContentAdmissionPlan,
    PdfClassificationResult, PlanAction, PlanCandidate, RejectReason,
};
use crate::budget_model::{ContextBudgetModel, RouteKind};
use crate::compressor::{build_context, fixed_context_sections, ContextBuildOutput};
use crate::decision::ContextFileEvidence;
use crate::parsers::classifier::PdfClassifierPort;
use crate::scanner::next_request_id;
use crate::source_guard::{
    source_guard_kind_from_text, SourceGuardKind, SourceGuardObservationMetrics,
    SourceGuardObserver, SourceGuardV2,
};
use crate::store::{
    CacheLookup, CacheWriteRecord, ClassificationCacheLookup, ClassificationCacheWriteRecord,
    ContextDecisionRecord, ContextRunRecord, DiagnosticSeverity, FileResultRecord, InventoryRecord,
    RunDiagnosticRecord,
};

pub use crate::deadline::{Clock, RealClock, RunDeadlines, FINALIZATION_RESERVE_MS};

mod refill;
mod warnings;

use warnings::project_warnings;

// ---------------------------------------------------------------------------
// Clock / guard / ports
// ---------------------------------------------------------------------------

/// Source guard verifier around worker results (spec SourceGuard v2).
pub trait GuardVerifier: Send + Sync {
    fn verify(&self, path: &str, expected: &SourceGuardV2) -> bool;

    fn is_tainted(&self, _path: &str) -> bool {
        false
    }

    fn metrics(&self) -> Option<SourceGuardObservationMetrics> {
        None
    }
}

/// Production verifier: recomputes the guard for the current file.
pub struct RealGuardVerifier {
    observer: SourceGuardObserver,
}

impl RealGuardVerifier {
    pub fn new(observer: SourceGuardObserver) -> Self {
        Self { observer }
    }
}

impl GuardVerifier for RealGuardVerifier {
    fn verify(&self, path: &str, expected: &SourceGuardV2) -> bool {
        self.observer.verify(Path::new(path), expected)
    }

    fn is_tainted(&self, path: &str) -> bool {
        self.observer.is_tainted(Path::new(path))
    }

    fn metrics(&self) -> Option<SourceGuardObservationMetrics> {
        Some(self.observer.metrics())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachePortError {
    Store { detail: String },
    InvalidKey { detail: String },
    DeadlineExhausted { detail: String },
}

impl CachePortError {
    pub fn diagnostic(&self, stage: DiagnosticStage) -> Diagnostic {
        Diagnostic {
            error_code: match self {
                Self::Store { .. } => ErrorCode::CacheWriteFailed,
                Self::InvalidKey { .. } => ErrorCode::InvalidRequest,
                Self::DeadlineExhausted { .. } => ErrorCode::StageDeadlineExhausted,
            },
            message: match self {
                Self::Store { detail }
                | Self::InvalidKey { detail }
                | Self::DeadlineExhausted { detail } => detail.clone(),
            },
            retryable: matches!(self, Self::Store { .. } | Self::DeadlineExhausted { .. }),
            stage,
            file_path: Nullable(None),
            backend: Nullable(None),
        }
    }
}

/// Per-file parse lookup outcome; the profile hash is computed by the adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseLookupOutcome {
    pub parse_profile_hash: String,
    pub lookup: CacheLookup,
}

/// Local cache adapter seam (spec Solution: `CachePort`). Only verified
/// successful results are accepted for writes.
pub trait CachePort: Send + Sync {
    fn prepare_inventory(
        &self,
        scan_run_id: u64,
        now_ms: u64,
        records: &[InventoryRecord],
    ) -> Result<HashSet<String>, CachePortError>;
    fn lookup_parse(
        &self,
        file: &DiscoveredFileOut,
        route: RouteKind,
        inventory_existed_before: bool,
    ) -> Result<ParseLookupOutcome, CachePortError>;
    fn lookup_classification(
        &self,
        file: &DiscoveredFileOut,
        classifier_profile_hash: &str,
        classifier_build: &str,
        pdf_open_failure_key: &str,
        inventory_existed_before: bool,
    ) -> Result<ClassificationCacheLookup, CachePortError>;
    fn write_parse(&self, now_ms: u64, records: &[CacheWriteRecord]) -> Result<(), CachePortError>;
    fn write_classification(
        &self,
        now_ms: u64,
        records: &[ClassificationCacheWriteRecord],
    ) -> Result<(), CachePortError>;
    /// Batch `last_accessed_bucket` touch for cache hits (spec Part 4: same row
    /// at most once/day, all hits updated in one transaction). Default no-op for
    /// adapters that do not track access buckets.
    fn touch_access(
        &self,
        _now_ms: u64,
        _parse_hits: &[String],
        _classification_hits: &[String],
    ) -> Result<(), CachePortError> {
        Ok(())
    }
}

/// Per-file parse request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseRequest {
    pub selected_pdf_pages: Option<Vec<u64>>,
    pub file: DiscoveredFileOut,
    pub route: RouteKind,
    pub timeout_ms: u64,
}

/// Per-file parse result (Success / Error / Timeout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResult {
    pub file_identity: String,
    pub content: String,
    pub evidence_audit: Option<ai_daily_worker_contract::ParsedContentAudit>,
    pub parser_backend: String,
    pub worker_lane: String,
    pub truncated: bool,
    pub content_sha256: String,
    pub parse_status: ParseStatus,
    pub error: Option<Diagnostic>,
    /// Parser fallback / degradation warnings (e.g. a primary backend failure
    /// that recovered via fallback).
    pub warnings: Vec<Diagnostic>,
    pub failure_class: String,
    pub fallback_backend: String,
    pub fallback_reason_code: String,
    pub parse_transport: ParseTransport,
    pub parse_attempt_count: u64,
    pub primary_duration_ms: u64,
    pub fallback_duration_ms: u64,
    pub parse_duration_ms: u64,
}

/// Local parser adapter seam.
pub trait ParserPort: Send + Sync {
    fn parse(&self, request: &ParseRequest) -> ParseResult;
}

/// Verified worker identities for cache/snapshot identity.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkerIdentities {
    pub office_contract: Option<String>,
    pub office_version: Option<String>,
    pub office_build: Option<String>,
    pub pdf_contract: Option<String>,
    pub pdf_version: Option<String>,
    pub pdf_build: Option<String>,
    pub classifier_build: Option<String>,
}

// ---------------------------------------------------------------------------
// Bounded parallel wave executor (spec Solution / Part 7.3, P4-T0)
// ---------------------------------------------------------------------------

/// Bounded rayon pool used to execute classifier/parser invocations in
/// deterministic waves. The pool holds at most `concurrency` threads; results
/// come back in the same order as the input, so the scheduler merges them by
/// nominal rank and completion order can never change the outcome.
struct WaveExecutor {
    pool: Option<rayon::ThreadPool>,
    concurrency: usize,
}

impl WaveExecutor {
    fn new(concurrency: usize) -> Result<Self, SchedulerFailure> {
        let pool = if concurrency > 1 {
            Some(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(concurrency)
                    .build()
                    .map_err(|_| {
                        SchedulerFailure::new(
                            ErrorCode::InternalError,
                            "scheduler worker pool could not be created".to_string(),
                            true,
                            DiagnosticStage::Process,
                        )
                    })?,
            )
        } else {
            None
        };
        Ok(Self { pool, concurrency })
    }

    /// Runs `f` over `items`. With more than one item and a pool this executes
    /// on the bounded pool (at most `concurrency` tasks in flight); otherwise it
    /// runs sequentially. Output order matches the input order.
    fn map<T, R, F>(&self, items: &[T], f: F) -> Vec<R>
    where
        T: Sync,
        R: Send,
        F: Fn(&T) -> R + Sync,
    {
        match &self.pool {
            Some(pool) if items.len() > 1 => pool.install(|| items.par_iter().map(&f).collect()),
            _ => items.iter().map(&f).collect(),
        }
    }
}

/// A PDF whose classification is queued for a parallel wave.
struct ClassificationTask {
    file: DiscoveredFileOut,
    guard: SourceGuardV2,
    request: crate::parsers::classifier::ClassifyOperation,
    /// The classifier's own per-file timeout (capped by the remaining work
    /// deadline at admission); the wave dispatch re-caps it by the remaining
    /// work deadline at dispatch time.
    own_timeout_ms: u64,
    classifier_profile_hash: String,
    pdf_open_failure_key: String,
    classifier_build: String,
    cache_miss_reason: String,
    nominal_charged_pages: u64,
}

/// An admitted file queued for a parallel parse wave.
struct ParseTask {
    file: DiscoveredFileOut,
    route: RouteKind,
    /// The route's per-file timeout capped by the remaining work deadline at
    /// admission; the wave dispatch re-caps it by the remaining work deadline.
    own_timeout_ms: u64,
    profile_hash: String,
}

// ---------------------------------------------------------------------------
// Input / outcome / failure
// ---------------------------------------------------------------------------

/// Immutable scheduler input (spec Solution). Deadlines are monotonic
/// milliseconds in the same unit as the injected [`Clock`].
#[derive(Debug, Clone)]
pub struct ScheduledRunInput {
    pub scan_run_id: u64,
    /// Wall-clock audit timestamp for the current run.
    pub started_at_ms: u64,
    pub work_dir: String,
    pub discovery: Vec<DiscoveredFileOut>,
    pub discovery_issues: Vec<DiscoveryIssue>,
    pub profile: NormalizedScannerSettings,
    pub workers: WorkerIdentities,
    /// Engine identity used for the local (light-text) parser provenance and
    /// the terminal `context_runs.context_profile_hash`.
    pub engine_version: String,
    pub engine_build: String,
    /// Terminal `context_runs.context_profile_hash` (canonical v1/v2 profile
    /// fingerprint computed by the run shell before execution).
    pub context_profile_hash: String,
    /// Rejected-profile fingerprint for non-admitted file rows (computed by the
    /// run shell from the canonical v1 profile).
    pub rejected_profile_hash: String,
    /// Discovery wall span observed by the run shell (spec Part 5.3).
    pub discovery_duration_ms: u64,
    /// Monotonic origin captured immediately after `begin_run` succeeds.
    pub deadline_origin_ms: u64,
    /// Monotonic ms at which all heavy work must stop (Absolute - 2,000ms).
    pub work_deadline_ms: u64,
    /// Monotonic ms at which no terminal write may begin.
    pub absolute_deadline_ms: u64,
    prepared_inventory: Option<Vec<InventoryRecord>>,
    prepared_inventory_existed_before: Option<HashSet<String>>,
    prepared_inventory_duration_ms: u64,
}

impl ScheduledRunInput {
    /// Constructs the deadline pair from the profile's `total_deadline_ms`
    /// (spec Solution: WorkDeadline = AbsoluteDeadline - 2,000ms, same origin).
    // Keep the frozen run-input fields explicit at this production boundary.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        scan_run_id: u64,
        started_at_ms: u64,
        work_dir: String,
        discovery: Vec<DiscoveredFileOut>,
        discovery_issues: Vec<DiscoveryIssue>,
        profile: NormalizedScannerSettings,
        workers: WorkerIdentities,
        engine_version: String,
        engine_build: String,
        context_profile_hash: String,
        rejected_profile_hash: String,
        discovery_duration_ms: u64,
        clock: &dyn Clock,
    ) -> Result<Self, SchedulerFailure> {
        let deadlines = RunDeadlines::derive(profile.total_deadline_ms, clock)
            .map_err(|message| SchedulerFailure::internal(&message))?;
        Self::new_with_deadlines(
            scan_run_id,
            started_at_ms,
            work_dir,
            discovery,
            discovery_issues,
            profile,
            workers,
            engine_version,
            engine_build,
            context_profile_hash,
            rejected_profile_hash,
            discovery_duration_ms,
            deadlines,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_deadlines(
        scan_run_id: u64,
        started_at_ms: u64,
        work_dir: String,
        discovery: Vec<DiscoveredFileOut>,
        discovery_issues: Vec<DiscoveryIssue>,
        profile: NormalizedScannerSettings,
        workers: WorkerIdentities,
        engine_version: String,
        engine_build: String,
        context_profile_hash: String,
        rejected_profile_hash: String,
        discovery_duration_ms: u64,
        deadlines: RunDeadlines,
    ) -> Result<Self, SchedulerFailure> {
        deadlines
            .validate(profile.total_deadline_ms)
            .map_err(|message| SchedulerFailure::internal(&message))?;
        Ok(Self {
            scan_run_id,
            started_at_ms,
            work_dir,
            discovery,
            discovery_issues,
            profile,
            workers,
            engine_version,
            engine_build,
            context_profile_hash,
            rejected_profile_hash,
            discovery_duration_ms,
            deadline_origin_ms: deadlines.origin_ms,
            work_deadline_ms: deadlines.work_deadline_ms,
            absolute_deadline_ms: deadlines.absolute_deadline_ms,
            prepared_inventory: None,
            prepared_inventory_existed_before: None,
            prepared_inventory_duration_ms: 0,
        })
    }

    /// Production run.rs prepares the global inventory receipt before snapshot
    /// lookup. Tests and alternate adapters may omit it and let Scheduler call
    /// the CachePort itself.
    pub fn with_prepared_inventory(
        mut self,
        inventory: Vec<InventoryRecord>,
        existed_before: HashSet<String>,
        duration_ms: u64,
    ) -> Result<Self, SchedulerFailure> {
        if inventory.len() != self.discovery.len()
            || inventory.iter().zip(&self.discovery).any(|(record, file)| {
                record.file_identity != file.file_identity
                    || record.source_version != file.source_version
            })
            || existed_before
                .iter()
                .any(|identity| !inventory.iter().any(|row| &row.file_identity == identity))
        {
            return Err(SchedulerFailure::internal(
                "prepared inventory receipt disagrees with discovery",
            ));
        }
        self.prepared_inventory = Some(inventory);
        self.prepared_inventory_existed_before = Some(existed_before);
        self.prepared_inventory_duration_ms = duration_ms;
        Ok(self)
    }
}

/// Terminal intent of the outcome (spec Part 2.3). The caller must not re-decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalIntent {
    Success,
    Partial,
    Error,
}

/// Bounded execution metrics collected by the scheduler (spec Part 5.3 subset).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecutionMetrics {
    pub refill_audit: Option<ai_daily_scanner_contract::RefillAudit>,
    /// Discovery-observed accepted source count; fixed to `MAX_SOURCE_FILES_PER_RUN
    /// + 1` when the engine-owned ceiling triggers (spec Part 2.1).
    pub discovery_observed_file_count: u64,
    pub source_guard_content_hash_file_count: u64,
    pub source_guard_unavailable_count: u64,
    pub source_guard_bytes_read: u64,
    pub candidate_file_count: u64,
    pub classification_slot_count: u64,
    pub admitted_file_count: u64,
    pub extraction_slot_count: u64,
    pub stage_deadline_exhausted_count: u64,
    pub parse_cache_lookup_count: u64,
    pub classification_cache_lookup_count: u64,
    pub parse_cache_all_hit: Option<bool>,
    pub classification_cache_all_hit: Option<bool>,
    pub classify_attempt_count: u64,
    pub parse_attempt_count: u64,
    pub reserved_chars: u64,
    pub rendered_chars: u64,
    pub deadline_precommit_elapsed_ms: u64,
    // spec Part 5.3: page/nominal/pdfplumber counts owned by the scheduler.
    pub confirmed_run_inspected_pages_total: u64,
    pub unobserved_classification_attempt_count: u64,
    pub nominal_charged_pages_total: u64,
    pub pdfplumber_invocations: u64,
}

/// Complete outcome of a scheduled run (spec Solution). Callers convert this to
/// the terminal record but must NOT re-decide actions/counts/admission.
#[derive(Debug, Clone)]
pub struct BudgetedScanOutcome {
    pub scan_run_id: u64,
    pub terminal_intent: TerminalIntent,
    pub inventory: Vec<InventoryRecord>,
    pub file_results: Vec<FileResultRecord>,
    pub sources: Vec<crate::sources::ScanSource>,
    /// Committed successful parse-cache receipts (already written by the CachePort).
    pub parse_cache_receipts: Vec<CacheWriteRecord>,
    /// Committed successful classification-cache receipts.
    pub classification_cache_receipts: Vec<ClassificationCacheWriteRecord>,
    /// Per-file PDF classification results (spec Part 3.2). PDFs that reached
    /// the classifier and `not_classified_by_budget` PDFs have an entry; files
    /// rejected before classification and non-PDFs are absent. Consumed by the
    /// artifact write path to persist immutable classifier provenance.
    pub classifications:
        std::collections::BTreeMap<String, crate::admission::PdfClassificationResult>,
    pub diagnostics: Vec<RunDiagnosticRecord>,
    pub stage_metrics: Vec<StageMetric>,
    pub extension_metrics: Vec<ExtensionMetric>,
    pub context: Option<ContextRunRecord>,
    pub execution_metrics: ExecutionMetrics,
}

/// Failure to form a valid outcome (adapter/contract/internal). Defined
/// business terminal states (per-file Error/Timeout, deadlines,
/// `BUDGET_MODEL_MISMATCH`) are returned as `Ok(BudgetedScanOutcome)` with the
/// terminal intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerFailure {
    pub diagnostic: Diagnostic,
}

impl SchedulerFailure {
    pub fn new(
        error_code: ErrorCode,
        message: String,
        retryable: bool,
        stage: DiagnosticStage,
    ) -> Self {
        Self {
            diagnostic: Diagnostic {
                error_code,
                message: message.chars().take(4_096).collect(),
                retryable,
                stage,
                file_path: Nullable(None),
                backend: Nullable(None),
            },
        }
    }

    pub fn internal(message: &str) -> Self {
        Self::new(
            ErrorCode::InternalError,
            message.to_string(),
            false,
            DiagnosticStage::Internal,
        )
    }
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// The scheduler owns the nominal ranking + two-phase plans + classification/
/// cache/parser merging + state transitions + context rendering + deadline
/// terminal states. Parser/classifier/cache/clock are injected adapters.
pub struct BudgetedContextScheduler {
    pub classifier: Box<dyn PdfClassifierPort>,
    pub parser: Box<dyn ParserPort>,
    pub cache: Box<dyn CachePort>,
    pub clock: Box<dyn Clock>,
    pub guard: Box<dyn GuardVerifier>,
    /// WorkDeadline (monotonic ms) resolved at the execute boundary.
    stored_work_deadline: u64,
    stored_workers: WorkerIdentities,
    stored_engine_version: String,
    stored_engine_build: String,
}

struct ParseOutputs {
    results: HashMap<String, ParseResult>,
    runtime_not_parsed: HashSet<String>,
    parse_cache_receipts: Vec<CacheWriteRecord>,
    classification_cache_receipts: Vec<ClassificationCacheWriteRecord>,
    /// Per-file parse-profile hash (route profile fingerprint) used by the
    /// terminal file results.
    parse_profile_hashes: HashMap<String, String>,
    /// Per-file parse cache status (fresh/miss) for the terminal file results.
    parse_cache_status: HashMap<String, CacheStatus>,
    /// Per-file parse cache miss reason for the terminal file results.
    parse_cache_miss_reason: HashMap<String, CacheMissReason>,
    /// Warnings for cache writes that were SKIPPED (space/busy/deadline); the
    /// result itself is still authoritative, only the receipt is not committed.
    cache_write_warnings: Vec<Diagnostic>,
    /// Parser fallback / degradation warnings surfaced by the parser adapter.
    parse_warnings: Vec<Diagnostic>,
    /// Fresh parse-cache hit identities for the batch `last_accessed_bucket` touch.
    parse_fresh_hits: Vec<String>,
}

mod budget;
use budget::{expected_guard, plan_candidate};
mod classification;
use classification::classifier_error_line;
mod dispatch;
pub(crate) use dispatch::parse_worker_lane;
mod recovery;
use recovery::{
    classifier_failure_semantics, is_source_version_changed_result,
    source_version_changed_after_parse, source_version_changed_classification,
    source_version_changed_parse,
};
mod evidence;
pub(crate) use evidence::zero_stage_metrics;
use evidence::{
    build_evidence, build_stage_metrics, enforce_rendered_within_reserved, file_record,
    internal_error_outcome, terminal_state,
};
