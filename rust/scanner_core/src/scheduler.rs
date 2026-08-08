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

use ai_daily_discovery::{DiscoveryIssue, DiscoveredFileOut};
use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheMissReason, CacheStatus, ContextSummary, Diagnostic, DiagnosticStage,
    ErrorCode, ExtensionMetric, NormalizedScannerProfileV2, Nullable, ParseStatus,
    PdfClassificationStatus, RunStatus, StageMetric, StageName,
};

use crate::admission::{
    AdmissionDecision, ClassificationPlan, ClassifiedPlan, ContentAdmissionPlan, PlanAction,
    PlanCandidate, RejectReason,
};
use crate::budget_model::{count_chars, ContextBudgetModel, RouteKind};
use crate::compressor::{build_context, fixed_context_sections, ContextBuildOutput};
use crate::decision::ContextFileEvidence;
use crate::parsers::classifier::PdfClassifierPort;
use crate::source_guard::{source_guard_kind_from_text, verify_guard, SourceGuardKind, SourceGuardV2};
use crate::store::{
    ClassificationCacheLookup, ClassificationCacheWriteRecord, ContextDecisionRecord,
    ContextRunRecord, DiagnosticSeverity, FileResultRecord, InventoryRecord, RunDiagnosticRecord,
    CacheLookup, CacheWriteRecord,
};

/// Fixed tail reserve for envelope/finalization (spec Solution).
pub const FINALIZATION_RESERVE_MS: u64 = 2_000;

// ---------------------------------------------------------------------------
// Clock / guard / ports
// ---------------------------------------------------------------------------

/// Monotonic clock source (fake-able in tests). Values are milliseconds since
/// the same origin as the deadline values in [`ScheduledRunInput`].
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Production monotonic clock.
#[derive(Debug)]
pub struct RealClock {
    origin: std::time::Instant,
}

impl RealClock {
    pub fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Source guard verifier around worker results (spec SourceGuard v2).
pub trait GuardVerifier: Send + Sync {
    fn verify(&self, path: &str, expected: &SourceGuardV2) -> bool;
}

/// Production verifier: recomputes the guard for the current file.
pub struct RealGuardVerifier;

impl GuardVerifier for RealGuardVerifier {
    fn verify(&self, path: &str, expected: &SourceGuardV2) -> bool {
        verify_guard(Path::new(path), expected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CachePortError {
    Store { detail: String },
    InvalidKey { detail: String },
}

impl CachePortError {
    pub fn diagnostic(&self, stage: DiagnosticStage) -> Diagnostic {
        Diagnostic {
            error_code: match self {
                Self::Store { .. } => ErrorCode::CacheWriteFailed,
                Self::InvalidKey { .. } => ErrorCode::InvalidRequest,
            },
            message: match self {
                Self::Store { detail } | Self::InvalidKey { detail } => detail.clone(),
            },
            retryable: matches!(self, Self::Store { .. }),
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
        inventory_existed_before: bool,
    ) -> Result<ClassificationCacheLookup, CachePortError>;
    fn write_parse(&self, now_ms: u64, records: &[CacheWriteRecord])
        -> Result<(), CachePortError>;
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
    pub file: DiscoveredFileOut,
    pub route: RouteKind,
    pub timeout_ms: u64,
}

/// Per-file parse result (Success / Error / Timeout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResult {
    pub file_identity: String,
    pub content: String,
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
    pub python_contract: Option<String>,
    pub python_version: Option<String>,
    pub python_build: Option<String>,
    pub classifier_build: Option<String>,
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
    pub profile: NormalizedScannerProfileV2,
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
    /// Monotonic ms at which all heavy work must stop (Absolute - 2,000ms).
    pub work_deadline_ms: u64,
    /// Monotonic ms at which no terminal write may begin.
    pub absolute_deadline_ms: u64,
}

impl ScheduledRunInput {
    /// Constructs the deadline pair from the profile's `total_deadline_ms`
    /// (spec Solution: WorkDeadline = AbsoluteDeadline - 2,000ms, same origin).
    pub fn new(
        scan_run_id: u64,
        started_at_ms: u64,
        work_dir: String,
        discovery: Vec<DiscoveredFileOut>,
        discovery_issues: Vec<DiscoveryIssue>,
        profile: NormalizedScannerProfileV2,
        workers: WorkerIdentities,
        engine_version: String,
        engine_build: String,
        context_profile_hash: String,
        rejected_profile_hash: String,
        discovery_duration_ms: u64,
        clock: &dyn Clock,
    ) -> Result<Self, SchedulerFailure> {
        let now = clock.now_ms();
        let absolute = now
            .checked_add(profile.total_deadline_ms)
            .ok_or_else(|| SchedulerFailure::internal("deadline arithmetic overflowed"))?;
        let work = absolute
            .checked_sub(FINALIZATION_RESERVE_MS)
            .ok_or_else(|| SchedulerFailure::internal("work deadline underflowed"))?;
        if work >= absolute {
            return Err(SchedulerFailure::internal(
                "work deadline must precede the absolute deadline",
            ));
        }
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
            work_deadline_ms: work,
            absolute_deadline_ms: absolute,
        })
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
}

/// Complete outcome of a scheduled run (spec Solution). Callers convert this to
/// the terminal record but must NOT re-decide actions/counts/admission.
#[derive(Debug, Clone)]
pub struct BudgetedScanOutcome {
    pub scan_run_id: u64,
    pub terminal_intent: TerminalIntent,
    pub inventory: Vec<InventoryRecord>,
    pub file_results: Vec<FileResultRecord>,
    /// Committed successful parse-cache receipts (already written by the CachePort).
    pub parse_cache_receipts: Vec<CacheWriteRecord>,
    /// Committed successful classification-cache receipts.
    pub classification_cache_receipts: Vec<ClassificationCacheWriteRecord>,
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

impl BudgetedContextScheduler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        classifier: Box<dyn PdfClassifierPort>,
        parser: Box<dyn ParserPort>,
        cache: Box<dyn CachePort>,
        clock: Box<dyn Clock>,
        guard: Box<dyn GuardVerifier>,
    ) -> Self {
        Self {
            classifier,
            parser,
            cache,
            clock,
            guard,
            stored_work_deadline: 0,
            stored_workers: WorkerIdentities::default(),
            stored_engine_version: String::new(),
            stored_engine_build: String::new(),
        }
    }

    pub fn execute(mut self, input: ScheduledRunInput) -> Result<BudgetedScanOutcome, SchedulerFailure> {
        if self.clock.now_ms() >= input.absolute_deadline_ms {
            return Err(SchedulerFailure::internal(
                "absolute deadline already exhausted before execution",
            ));
        }
        self.stored_work_deadline = input.work_deadline_ms;
        self.stored_workers = input.workers.clone();
        self.stored_engine_version = input.engine_version.clone();
        self.stored_engine_build = input.engine_build.clone();
        // spec Part 5.3: discovery issues surface as run-level warnings (they
        // mark the run Partial and never silently disappear).
        let mut diagnostics: Vec<RunDiagnosticRecord> = input
            .discovery_issues
            .iter()
            .map(discovery_issue_warning)
            .collect();
        let inventory = build_inventory(&input.discovery, &input.work_dir)?;
        let cache_started = self.clock.now_ms();
        let existed_before = self
            .cache
            .prepare_inventory(input.scan_run_id, input.started_at_ms, &inventory)
            .map_err(|error| SchedulerFailure {
                diagnostic: error.diagnostic(DiagnosticStage::Cache),
            })?;
        let cache_duration_ms = self.clock.now_ms().saturating_sub(cache_started);

        let discovery_count = input.discovery.len() as u64;
        let profile = input.profile.clone();
        let mut metrics = source_guard_metrics(&input.discovery);

        // ---- Stage A: freeze ClassificationPlan before any classification I/O ----
        let candidates: Vec<PlanCandidate> = input.discovery.iter().map(plan_candidate).collect();
        let classified = ClassificationPlan::build(
            candidates,
            &profile,
            profile.max_total_pdf_classification_pages,
        );
        metrics.candidate_file_count = classified
            .iter()
            .filter(|plan| matches!(plan.action, PlanAction::Admit { .. }))
            .count() as u64;
        metrics.classification_slot_count = classified
            .iter()
            .filter(|plan| matches!(plan.pdf_classification, crate::admission::PdfClassificationPlan::Classify { .. }))
            .count() as u64;

        // ---- Execute the selected PDF classifications ----
        let classifier_profile_hash = crate::store::classifier_profile_hash(&profile)
            .map_err(|message| SchedulerFailure::internal(&message))?;
        let needs_classifier = classified.iter().any(|plan| {
            matches!(
                plan.pdf_classification,
                crate::admission::PdfClassificationPlan::Classify { .. }
            )
        });
        let classifier_build = match &input.workers.classifier_build {
            Some(build) => build.clone(),
            None if needs_classifier => {
                return Err(SchedulerFailure::new(
                    ErrorCode::WorkerHandshakeFailed,
                    "classifier build is required when PDFs are classified".to_string(),
                    false,
                    DiagnosticStage::Process,
                ))
            }
            None => "0".repeat(64),
        };
        let snapshot: HashMap<String, &DiscoveredFileOut> = input
            .discovery
            .iter()
            .map(|file| (file.file_identity.clone(), file))
            .collect();
        let (classifications, runtime_classification, classification_fresh_hits) =
            self.run_classifications(
                &classified,
                &snapshot,
                &profile,
                &classifier_profile_hash,
                &classifier_build,
                &existed_before,
                &mut metrics,
            );

        // ---- Fixed context sections + budget model ----
        let mut fixed =
            fixed_context_sections(&profile.context, profile.report_mode, discovery_count);
        for plan in &classified {
            if let Some(line) =
                classifier_error_line(plan, &classifications, &runtime_classification)
            {
                fixed.push(line);
            }
        }
        let model = ContextBudgetModel::new(&profile.context, &fixed).map_err(|error| {
            SchedulerFailure::new(
                ErrorCode::ContextFixedSectionsOverBudget,
                error.to_string(),
                false,
                DiagnosticStage::Context,
            )
        })?;

        // ---- Freeze ContentAdmissionPlan BEFORE any parse I/O ----
        let filtered: Vec<ClassifiedPlan> = classified
            .iter()
            .filter(|plan| {
                !runtime_classification.contains(&plan.file_identity)
                    && plan.pdf_classification
                        != crate::admission::PdfClassificationPlan::NotClassifiedByBudget
            })
            .cloned()
            .collect();
        let admission = ContentAdmissionPlan::build(&filtered, &profile, &model, &classifications);
        metrics.admitted_file_count = admission
            .iter()
            .filter(|decision| matches!(decision.action, PlanAction::Admit { .. }))
            .count() as u64;
        metrics.extraction_slot_count = admission
            .iter()
            .filter(|decision| {
                matches!(decision.action, PlanAction::Admit { route: RouteKind::Pdf })
                    && classifications.get(&decision.file_identity).map(|r| r.status)
                        == Some(PdfClassificationStatus::TextInParseWindow)
            })
            .count() as u64;

        // ---- Execute admitted parses ----
        let parse_started = self.clock.now_ms();
        let parse_outputs = self.run_parses(
            &admission,
            &snapshot,
            &profile,
            &existed_before,
            &runtime_classification,
            &mut metrics,
        )?;
        let parse_duration_ms = self.clock.now_ms().saturating_sub(parse_started);

        // ---- Build per-file evidence + decide + render ----
        let evidence = build_evidence(
            &input.discovery,
            &input.work_dir,
            &classified,
            &admission,
            &classifications,
            &runtime_classification,
            &parse_outputs,
        )?;
        let render_started = self.clock.now_ms();
        let rendered = match build_context(evidence, &profile.context, profile.report_mode) {
            Ok(rendered) => rendered,
            Err(message) => {
                // spec Solution: BUDGET_MODEL_MISMATCH is a defined terminal
                // state -> Ok(outcome) with Error intent, never Err(SchedulerFailure).
                return Ok(internal_error_outcome(
                    input.scan_run_id,
                    metrics,
                    diagnostics,
                    parse_outputs,
                    ErrorCode::BudgetModelMismatch,
                    message,
                ));
            }
        };
        let compression_duration_ms = self.clock.now_ms().saturating_sub(render_started);

        // ---- Enforce the rendered <= reserved budget-model invariant ----
        if let Err(failure) = enforce_rendered_within_reserved(&rendered, &admission, &model) {
            return Ok(internal_error_outcome(
                input.scan_run_id,
                metrics,
                diagnostics,
                parse_outputs,
                failure.diagnostic.error_code,
                failure.diagnostic.message,
            ));
        }

        metrics.reserved_chars = model
            .base_chars()
            .saturating_add(admission.iter().map(|d| d.reserved_chars).sum::<u64>());
        metrics.rendered_chars = count_chars(&rendered.content);

        // ---- Terminal file results + intent + bounded diagnostics ----
        let file_results = build_file_results(
            &classified,
            &snapshot,
            &input.work_dir,
            &admission,
            &parse_outputs,
            &runtime_classification,
            &input.rejected_profile_hash,
        )?;
        let (mut terminal_intent, run_diagnostics) = terminal_state(
            &rendered,
            &file_results,
            &runtime_classification,
            &parse_outputs,
            metrics.stage_deadline_exhausted_count,
        );
        diagnostics.extend(
            parse_outputs
                .cache_write_warnings
                .iter()
                .cloned()
                .map(|diagnostic| RunDiagnosticRecord {
                    severity: DiagnosticSeverity::Warning,
                    diagnostic,
                }),
        );
        diagnostics.extend(
            parse_outputs
                .parse_warnings
                .iter()
                .cloned()
                .map(|diagnostic| RunDiagnosticRecord {
                    severity: DiagnosticSeverity::Warning,
                    diagnostic,
                }),
        );
        diagnostics.extend(run_diagnostics);
        // spec Part 2.2: 257-warning bounded projection (run-level first,
        // 256 detail + 1 DIAGNOSTICS_AGGREGATED).
        diagnostics = project_warnings(diagnostics);
        // Discovery issues / parser degradation / cache-write warnings are
        // run-level warnings: they mark the run Partial, never Success.
        if terminal_intent == TerminalIntent::Success
            && diagnostics
                .iter()
                .any(|record| record.severity == DiagnosticSeverity::Warning)
        {
            terminal_intent = TerminalIntent::Partial;
        }

        let stage_metrics = build_stage_metrics(
            discovery_count,
            rendered.decisions.len() as u64,
            discovery_count,
            metrics.parse_attempt_count,
            input.discovery_duration_ms,
            cache_duration_ms,
            parse_duration_ms,
            compression_duration_ms,
        );
        let extension_metrics = crate::context_audit::extension_metrics(&inventory, &file_results)
            .map_err(|message| SchedulerFailure::internal(&message))?;

        let total_duration_ms = input
            .discovery_duration_ms
            .saturating_add(cache_duration_ms)
            .saturating_add(parse_duration_ms)
            .saturating_add(compression_duration_ms);
        let context = match terminal_intent {
            // Error envelopes carry an EMPTY file_context; run.rs builds the
            // error envelope from the outcome diagnostics.
            TerminalIntent::Error => None,
            _ if rendered.content.is_empty() => None,
            _ => {
                let summary = ContextSummary {
                    source_file_count: rendered.source_file_count,
                    success_count: rendered.success_count,
                    timeout_count: rendered.timeout_count,
                    included_file_count: rendered.included_file_count,
                    omitted_file_count: rendered.omitted_file_count,
                    error_file_count: rendered.error_file_count,
                    input_chars: rendered.input_chars,
                    output_chars: rendered.output_chars,
                    total_duration_ms,
                    discovery_duration_ms: input.discovery_duration_ms,
                    parse_duration_ms,
                    compression_duration_ms,
                };
                let decisions = rendered
                    .decisions
                    .into_iter()
                    .map(|record| ContextDecisionRecord {
                        file_identity: record.file_identity,
                        decision: record.decision,
                    })
                    .collect();
                Some(ContextRunRecord {
                    context_profile_hash: input.context_profile_hash,
                    status: match terminal_intent {
                        TerminalIntent::Success => RunStatus::Success,
                        TerminalIntent::Partial => RunStatus::Partial,
                        TerminalIntent::Error => RunStatus::Error,
                    },
                    final_context: rendered.content.clone(),
                    context_sha256: crate::store::sha256_hex(rendered.content.as_bytes()),
                    summary,
                    decisions,
                })
            }
        };

        // spec Part 4: batch `last_accessed_bucket` touch for cache hits, all in
        // one transaction. Best-effort: a busy/deadline failure never fails the run.
        let _ = self.cache.touch_access(
            self.clock.now_ms(),
            &parse_outputs.parse_fresh_hits,
            &classification_fresh_hits,
        );

        metrics.deadline_precommit_elapsed_ms = self.clock.now_ms();

        Ok(BudgetedScanOutcome {
            scan_run_id: input.scan_run_id,
            terminal_intent,
            inventory,
            file_results,
            parse_cache_receipts: parse_outputs.parse_cache_receipts,
            classification_cache_receipts: parse_outputs.classification_cache_receipts,
            diagnostics,
            stage_metrics,
            extension_metrics,
            context,
            execution_metrics: metrics,
        })
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

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

fn source_guard_metrics(discovery: &[DiscoveredFileOut]) -> ExecutionMetrics {
    let mut metrics = ExecutionMetrics::default();
    for file in discovery {
        match source_guard_kind_from_text(file.source_guard_kind.as_deref().unwrap_or("")) {
            Some(SourceGuardKind::Unavailable) => metrics.source_guard_unavailable_count += 1,
            Some(SourceGuardKind::ContentSha256V1) => {
                metrics.source_guard_content_hash_file_count += 1;
                metrics.source_guard_bytes_read =
                    metrics.source_guard_bytes_read.saturating_add(file.size_bytes);
            }
            _ => {}
        }
    }
    metrics
}

/// Surfaces a discovery issue as a run-level warning (spec Part 5.3); an
/// unreadable discovery entry must mark the run Partial, never silently vanish.
fn discovery_issue_warning(issue: &DiscoveryIssue) -> RunDiagnosticRecord {
    let file_path = issue
        .path
        .as_ref()
        .filter(|path| Path::new(path).is_absolute());
    RunDiagnosticRecord {
        severity: DiagnosticSeverity::Warning,
        diagnostic: Diagnostic {
            error_code: ErrorCode::DiscoveryEntryUnreadable,
            message: issue
                .message
                .chars()
                .take(4_096)
                .collect(),
            retryable: true,
            stage: DiagnosticStage::Discovery,
            file_path: Nullable(file_path.cloned()),
            backend: Nullable(None),
        },
    }
}

fn build_inventory(
    discovery: &[DiscoveredFileOut],
    work_dir: &str,
) -> Result<Vec<InventoryRecord>, SchedulerFailure> {
    let mut inventory = Vec::with_capacity(discovery.len());
    for file in discovery {
        let relative_path =
            crate::context_audit::relative_contract_path(Path::new(work_dir), &file.path)
                .map_err(|message| SchedulerFailure::internal(&message))?;
        inventory.push(
            InventoryRecord::from_discovered(file, relative_path)
                .map_err(|message| SchedulerFailure::internal(&message))?,
        );
    }
    Ok(inventory)
}

fn plan_candidate(file: &DiscoveredFileOut) -> PlanCandidate {
    let guard_kind = file
        .source_guard_kind
        .as_deref()
        .and_then(source_guard_kind_from_text)
        .unwrap_or(SourceGuardKind::Unavailable);
    PlanCandidate {
        file_identity: file.file_identity.clone(),
        relative_path: file.path.clone(),
        extension: file.extension.clone(),
        size_bytes: file.size_bytes,
        source_guard_kind: guard_kind,
    }
}

fn expected_guard(file: &DiscoveredFileOut) -> Option<SourceGuardV2> {
    let kind = file
        .source_guard_kind
        .as_deref()
        .and_then(source_guard_kind_from_text)?;
    if kind == SourceGuardKind::Unavailable {
        return None;
    }
    let hash = file.source_guard_sha256.clone()?;
    let guard = SourceGuardV2 {
        kind,
        guard_sha256: Some(hash),
    };
    guard.validate().ok()?;
    Some(guard)
}

fn classifier_error_line(
    plan: &ClassifiedPlan,
    classifications: &BTreeMap<String, crate::admission::PdfClassificationResult>,
    runtime: &HashSet<String>,
) -> Option<String> {
    if !matches!(plan.pdf_classification, crate::admission::PdfClassificationPlan::Classify { .. }) {
        return None;
    }
    if runtime.contains(&plan.file_identity) {
        return None;
    }
    let status = classifications.get(&plan.file_identity).map(|r| r.status);
    match status {
        Some(PdfClassificationStatus::Unknown) => Some(format!(
            "- {} | reason=parse_error | error=PARSER_TIMEOUT",
            plan.relative_path
        )),
        Some(PdfClassificationStatus::Error) => Some(format!(
            "- {} | reason=parse_error | error=PARSER_FAILED",
            plan.relative_path
        )),
        _ => None,
    }
}

fn outcome_page_count(status: &PdfClassificationStatus) -> Option<u64> {
    match status {
        PdfClassificationStatus::TextInParseWindow
        | PdfClassificationStatus::NoTextInParseWindow => Some(1),
        _ => None,
    }
}

fn classification_cache_record(
    file: &DiscoveredFileOut,
    classifier_profile_hash: &str,
    classifier_build: &str,
    status: &PdfClassificationStatus,
    page_count: Option<u64>,
) -> Option<ClassificationCacheWriteRecord> {
    let kind = file.source_guard_kind.as_deref()?;
    let hash = file.source_guard_sha256.clone()?;
    let status_text = match status {
        PdfClassificationStatus::TextInParseWindow => "text_in_parse_window",
        PdfClassificationStatus::NoTextInParseWindow => "no_text_in_parse_window",
        _ => return None,
    };
    let pages = page_count.unwrap_or(0).max(1);
    Some(ClassificationCacheWriteRecord {
        file_identity: file.file_identity.clone(),
        source_version: file.source_version.clone(),
        source_guard_kind: kind.to_string(),
        source_guard_sha256: hash,
        classifier_profile_hash: classifier_profile_hash.to_string(),
        classifier_build: classifier_build.to_string(),
        status: status_text.to_string(),
        page_count: pages,
        result_examined_pages: pages,
    })
}

fn route_timeout_ms(route: RouteKind, profile: &NormalizedScannerProfileV2) -> u64 {
    let extension = match route {
        RouteKind::Pdf => ".pdf",
        RouteKind::PythonOffice => ".xls",
        RouteKind::RustXlsx => ".xlsx",
        _ => return profile.execution.file_timeout_ms,
    };
    profile
        .execution
        .file_timeout_by_extension_ms
        .get(extension)
        .copied()
        .unwrap_or(profile.execution.file_timeout_ms)
}

fn parse_worker_lane(lane: &str) -> AuditWorkerLane {
    match lane {
        "rust_core" => AuditWorkerLane::RustCore,
        "rust_office_process" => AuditWorkerLane::RustOfficeProcess,
        "python_document_process" => AuditWorkerLane::PythonDocumentProcess,
        _ => AuditWorkerLane::NotParsed,
    }
}

fn next_request_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let mut bytes = (nanos ^ (u128::from(std::process::id()) << 64) ^ counter).to_be_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

// ---------------------------------------------------------------------------
// classification execution
// ---------------------------------------------------------------------------

impl BudgetedContextScheduler {
    fn run_classifications(
        &self,
        classified: &[ClassifiedPlan],
        snapshot: &HashMap<String, &DiscoveredFileOut>,
        profile: &NormalizedScannerProfileV2,
        classifier_profile_hash: &str,
        classifier_build: &str,
        existed_before: &HashSet<String>,
        metrics: &mut ExecutionMetrics,
    ) -> (
        BTreeMap<String, crate::admission::PdfClassificationResult>,
        HashSet<String>,
        Vec<String>,
    ) {
        let mut results = BTreeMap::new();
        let mut runtime_not_parsed = HashSet::new();
        let mut classification_fresh_hits = Vec::new();
        let mut any_lookup = false;
        let mut any_miss = false;
        for plan in classified {
            if !matches!(plan.pdf_classification, crate::admission::PdfClassificationPlan::Classify { .. }) {
                continue;
            }
            let Some(file) = snapshot.get(&plan.file_identity) else {
                continue;
            };
            let work_deadline = self.stored_work_deadline;
            let now = self.clock.now_ms();
            if now >= work_deadline {
                runtime_not_parsed.insert(plan.file_identity.clone());
                metrics.stage_deadline_exhausted_count = 1;
                continue;
            }
            let Some(guard) = expected_guard(file) else {
                continue;
            };
            if !self.guard.verify(&file.path, &guard) {
                results.insert(
                    plan.file_identity.clone(),
                    crate::admission::PdfClassificationResult {
                        file_identity: plan.file_identity.clone(),
                        status: PdfClassificationStatus::Error,
                        page_count: None,
                        result_examined_pages: None,
                        error_code: Some("SOURCE_VERSION_CHANGED".to_string()),
                    },
                );
                continue;
            }
            metrics.classification_cache_lookup_count += 1;
            any_lookup = true;
            let existed = existed_before.contains(&plan.file_identity);
            match self
                .cache
                .lookup_classification(file, classifier_profile_hash, classifier_build, existed)
            {
                Ok(ClassificationCacheLookup::Fresh(entry)) => {
                    classification_fresh_hits.push(plan.file_identity.clone());
                    let status = match entry.status.as_str() {
                        "text_in_parse_window" => PdfClassificationStatus::TextInParseWindow,
                        _ => PdfClassificationStatus::NoTextInParseWindow,
                    };
                    results.insert(
                        plan.file_identity.clone(),
                        crate::admission::PdfClassificationResult {
                            file_identity: plan.file_identity.clone(),
                            status,
                            page_count: Some(entry.page_count),
                            result_examined_pages: Some(entry.result_examined_pages),
                            error_code: None,
                        },
                    );
                }
                Ok(ClassificationCacheLookup::Miss(_)) => {
                    any_miss = true;
                    let remaining = work_deadline.saturating_sub(self.clock.now_ms());
                    if remaining == 0 {
                        runtime_not_parsed.insert(plan.file_identity.clone());
                        metrics.stage_deadline_exhausted_count = 1;
                        continue;
                    }
                    let timeout = Duration::from_millis(
                        profile.pdf_classification_timeout_ms.min(remaining),
                    );
                    metrics.classify_attempt_count += 1;
                    let request = ai_daily_scanner_contract::PdfClassifierRequestV1 {
                        contract: "ai_daily_pdf_classifier".to_string(),
                        protocol_version: 1,
                        request_id: next_request_id(),
                        file_path: file.path.clone(),
                        source_version: file.source_version.clone(),
                        max_pages: profile.parse.pdf.max_pages,
                        policy_version: profile.classifier_policy_version.clone(),
                    };
                    let outcome = self.classifier.classify_pdf(&request, timeout);
                    let (status, error_code) = match outcome {
                        Ok(result) => {
                            let status = match result.status {
                                ai_daily_scanner_contract::PdfClassifierResultStatus::TextInParseWindow => {
                                    PdfClassificationStatus::TextInParseWindow
                                }
                                ai_daily_scanner_contract::PdfClassifierResultStatus::NoTextInParseWindow => {
                                    PdfClassificationStatus::NoTextInParseWindow
                                }
                                ai_daily_scanner_contract::PdfClassifierResultStatus::Unknown => {
                                    PdfClassificationStatus::Unknown
                                }
                                ai_daily_scanner_contract::PdfClassifierResultStatus::Error => {
                                    PdfClassificationStatus::Error
                                }
                            };
                            let error_code = result.diagnostic.0.as_ref().map(|diag| {
                                if diag.error_code
                                    == ai_daily_scanner_contract::PythonOperationErrorCode::ParserTimeout
                                {
                                    "PARSER_TIMEOUT".to_string()
                                } else {
                                    "PARSER_FAILED".to_string()
                                }
                            });
                            (status, error_code)
                        }
                        Err(failure) => {
                            let timed_out =
                                failure.diagnostic.error_code == ErrorCode::ParserTimeout;
                            (
                                PdfClassificationStatus::Unknown,
                                Some(if timed_out {
                                    "PARSER_TIMEOUT".to_string()
                                } else {
                                    "PARSER_FAILED".to_string()
                                }),
                            )
                        }
                    };
                    if !self.guard.verify(&file.path, &guard) {
                        results.insert(
                            plan.file_identity.clone(),
                            crate::admission::PdfClassificationResult {
                                file_identity: plan.file_identity.clone(),
                                status: PdfClassificationStatus::Error,
                                page_count: None,
                                result_examined_pages: None,
                                error_code: Some("SOURCE_VERSION_CHANGED".to_string()),
                            },
                        );
                        continue;
                    }
                    let page_count = if matches!(
                        status,
                        PdfClassificationStatus::TextInParseWindow
                            | PdfClassificationStatus::NoTextInParseWindow
                    ) {
                        outcome_page_count(&status)
                    } else {
                        None
                    };
                    results.insert(
                        plan.file_identity.clone(),
                        crate::admission::PdfClassificationResult {
                            file_identity: plan.file_identity.clone(),
                            status,
                            page_count,
                            result_examined_pages: page_count,
                            error_code,
                        },
                    );
                    // Success-only classification cache write while remaining > 0.
                    if self.clock.now_ms() < work_deadline {
                        if let Some(record) = classification_cache_record(
                            file,
                            classifier_profile_hash,
                            classifier_build,
                            &status,
                            page_count,
                        ) {
                            let _ = self
                                .cache
                                .write_classification(self.clock.now_ms(), &[record]);
                        }
                    }
                }
                Err(_) => {
                    results.insert(
                        plan.file_identity.clone(),
                        crate::admission::PdfClassificationResult {
                            file_identity: plan.file_identity.clone(),
                            status: PdfClassificationStatus::Error,
                            page_count: None,
                            result_examined_pages: None,
                            error_code: Some("CACHE_WRITE_FAILED".to_string()),
                        },
                    );
                }
            }
        }
        metrics.classification_cache_all_hit = if any_lookup { Some(!any_miss) } else { None };
        (results, runtime_not_parsed, classification_fresh_hits)
    }
}

impl BudgetedContextScheduler {
    /// Worker identity used for successful parse-cache provenance (spec
    /// Part 5.2). Light-text parses are local engine work; office routes use
    /// the office worker; python routes use the python worker.
    fn worker_identity_for(&self, route: RouteKind) -> (String, String, String) {
        match route {
            RouteKind::RustOffice | RouteKind::RustXlsx => (
                self.stored_workers
                    .office_contract
                    .clone()
                    .unwrap_or_else(|| "ai_daily_worker_v1".to_string()),
                self.stored_workers
                    .office_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                self.stored_workers
                    .office_build
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            RouteKind::Pdf | RouteKind::PythonOffice | RouteKind::PythonSharepointText => (
                self.stored_workers
                    .python_contract
                    .clone()
                    .unwrap_or_else(|| "ai_daily_worker_v1".to_string()),
                self.stored_workers
                    .python_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                self.stored_workers
                    .python_build
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            RouteKind::LightText => (
                "ai_daily_context_v1".to_string(),
                self.stored_engine_version.clone(),
                self.stored_engine_build.clone(),
            ),
        }
    }

    fn run_parses(
        &self,
        admission: &[AdmissionDecision],
        snapshot: &HashMap<String, &DiscoveredFileOut>,
        profile: &NormalizedScannerProfileV2,
        existed_before: &HashSet<String>,
        runtime_classification: &HashSet<String>,
        metrics: &mut ExecutionMetrics,
    ) -> Result<ParseOutputs, SchedulerFailure> {
        let mut results = HashMap::new();
        let mut runtime_not_parsed = HashSet::new();
        let mut parse_cache_receipts = Vec::new();
        let classification_cache_receipts = Vec::new();
        let mut parse_profile_hashes = HashMap::new();
        let mut parse_cache_status = HashMap::new();
        let mut parse_cache_miss_reason = HashMap::new();
        let mut cache_write_warnings = Vec::new();
        let mut parse_warnings = Vec::new();
        let mut parse_fresh_hits = Vec::new();
        let mut any_lookup = false;
        let mut any_miss = false;
        let work_deadline = self.stored_work_deadline;

        let mut admitted: Vec<(DiscoveredFileOut, RouteKind, u64, String)> = Vec::new();
        for decision in admission {
            if let PlanAction::Admit { route } = &decision.action {
                let Some(file) = snapshot.get(&decision.file_identity) else {
                    continue;
                };
                if runtime_classification.contains(&decision.file_identity) {
                    continue;
                }
                metrics.parse_cache_lookup_count += 1;
                any_lookup = true;
                let existed_before_file = existed_before.contains(&decision.file_identity);
                let lookup = self
                    .cache
                    .lookup_parse(file, *route, existed_before_file)
                    .map_err(|error| SchedulerFailure {
                        diagnostic: error.diagnostic(DiagnosticStage::Cache),
                    })?;
                let profile_hash = lookup.parse_profile_hash.clone();
                match lookup.lookup {
                    CacheLookup::Fresh(entry) => {
                        // spec SourceGuard v2: the parse cache key is guard-bound,
                        // and the guard is verified BEFORE consuming the cached
                        // value (TOCTOU between discovery and lookup).
                        let Some(guard) = expected_guard(file) else {
                            continue;
                        };
                        if !self.guard.verify(&file.path, &guard) {
                            results.insert(
                                file.file_identity.clone(),
                                source_version_changed_parse(file),
                            );
                            parse_profile_hashes
                                .insert(decision.file_identity.clone(), profile_hash);
                            continue;
                        }
                        parse_profile_hashes
                            .insert(decision.file_identity.clone(), profile_hash);
                        parse_cache_status
                            .insert(decision.file_identity.clone(), CacheStatus::Fresh);
                        parse_cache_miss_reason
                            .insert(decision.file_identity.clone(), CacheMissReason::None);
                        parse_fresh_hits.push(decision.file_identity.clone());
                        results.insert(
                            decision.file_identity.clone(),
                            ParseResult {
                                file_identity: decision.file_identity.clone(),
                                content: entry.content,
                                parser_backend: entry.parser_backend,
                                worker_lane: entry.worker_lane,
                                truncated: entry.truncated,
                                content_sha256: entry.content_sha256,
                                parse_status: ParseStatus::Success,
                                error: None,
                                warnings: Vec::new(),
                                failure_class: String::new(),
                                fallback_backend: String::new(),
                                fallback_reason_code: String::new(),
                                primary_duration_ms: 0,
                                fallback_duration_ms: 0,
                                parse_duration_ms: 0,
                            },
                        );
                    }
                    CacheLookup::Miss(reason) => {
                        any_miss = true;
                        parse_cache_status
                            .insert(decision.file_identity.clone(), CacheStatus::Miss);
                        parse_cache_miss_reason
                            .insert(decision.file_identity.clone(), reason);
                        if self.clock.now_ms() >= work_deadline {
                            runtime_not_parsed.insert(decision.file_identity.clone());
                            metrics.stage_deadline_exhausted_count = 1;
                            continue;
                        }
                        let remaining = work_deadline.saturating_sub(self.clock.now_ms());
                        let timeout_ms = route_timeout_ms(*route, profile).min(remaining);
                        metrics.parse_attempt_count += 1;
                        admitted.push((
                            (*file).clone(),
                            *route,
                            timeout_ms,
                            profile_hash,
                        ));
                    }
                }
            }
        }

        for (file, route, timeout_ms, profile_hash) in admitted {
            if self.clock.now_ms() >= work_deadline {
                runtime_not_parsed.insert(file.file_identity.clone());
                metrics.stage_deadline_exhausted_count = 1;
                continue;
            }
            let Some(guard) = expected_guard(&file) else {
                continue;
            };
            if !self.guard.verify(&file.path, &guard) {
                results.insert(file.file_identity.clone(), source_version_changed_parse(&file));
                parse_profile_hashes.insert(file.file_identity.clone(), profile_hash);
                continue;
            }
            let result = self.parser.parse(&ParseRequest {
                file: file.clone(),
                route,
                timeout_ms,
            });
            parse_warnings.extend(result.warnings.iter().cloned());
            if result.parse_status == ParseStatus::Success {
                if !self.guard.verify(&file.path, &guard) {
                    results.insert(file.file_identity.clone(), source_version_changed_parse(&file));
                    parse_profile_hashes.insert(file.file_identity.clone(), profile_hash);
                    continue;
                }
                if self.clock.now_ms() < work_deadline {
                    let (worker_contract, worker_version, worker_build) =
                        self.worker_identity_for(route);
                    let record = CacheWriteRecord {
                        file_identity: file.file_identity.clone(),
                        source_version: file.source_version.clone(),
                        source_guard_kind: crate::source_guard::source_guard_kind_text(guard.kind)
                            .to_string(),
                        source_guard_sha256: guard
                            .guard_sha256
                            .clone()
                            .unwrap_or_default(),
                        parse_profile_hash: profile_hash.clone(),
                        content: result.content.clone(),
                        content_sha256: result.content_sha256.clone(),
                        parser_backend: result.parser_backend.clone(),
                        worker_lane: result.worker_lane.clone(),
                        truncated: result.truncated,
                        worker_contract_version: worker_contract,
                        worker_version,
                        worker_build,
                    };
                    // spec Solution: cache COMMIT is a receipt; a failed write is
                    // a SKIPPED receipt with a warning, never a committed one.
                    match self.cache.write_parse(self.clock.now_ms(), &[record.clone()]) {
                        Ok(()) => parse_cache_receipts.push(record),
                        Err(error) => cache_write_warnings.push(Diagnostic {
                            error_code: ErrorCode::CacheWriteFailed,
                            message: format!("parse cache write skipped: {}", error.diagnostic(DiagnosticStage::Cache).message),
                            retryable: true,
                            stage: DiagnosticStage::Cache,
                            file_path: Nullable(Some(file.path.clone())),
                            backend: Nullable(Some(route.backend().to_string())),
                        }),
                    }
                }
            }
            results.insert(file.file_identity.clone(), result);
            parse_profile_hashes.insert(file.file_identity.clone(), profile_hash);
        }
        metrics.parse_cache_all_hit = if any_lookup { Some(!any_miss) } else { None };
        let _ = existed_before;
        Ok(ParseOutputs {
            results,
            runtime_not_parsed,
            parse_cache_receipts,
            classification_cache_receipts,
            parse_profile_hashes,
            parse_cache_status,
            parse_cache_miss_reason,
            cache_write_warnings,
            parse_warnings,
            parse_fresh_hits,
        })
    }
}

fn source_version_changed_parse(file: &DiscoveredFileOut) -> ParseResult {
    ParseResult {
        file_identity: file.file_identity.clone(),
        content: String::new(),
        parser_backend: "not_parsed".to_string(),
        worker_lane: "not_parsed".to_string(),
        truncated: false,
        content_sha256: crate::store::sha256_hex(b""),
        parse_status: ParseStatus::Error,
        error: Some(Diagnostic {
            error_code: ErrorCode::SourceVersionChanged,
            message: "file source version changed before or during parsing".to_string(),
            retryable: true,
            stage: DiagnosticStage::Parse,
            file_path: Nullable(Some(file.path.clone())),
            backend: Nullable(Some(file.extension.clone())),
        }),
        warnings: Vec::new(),
        failure_class: "deterministic".to_string(),
        fallback_backend: String::new(),
        fallback_reason_code: "source_version_changed".to_string(),
        primary_duration_ms: 0,
        fallback_duration_ms: 0,
        parse_duration_ms: 0,
    }
}

// ---------------------------------------------------------------------------
// evidence + file results + terminal state
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn build_evidence(
    discovery: &[DiscoveredFileOut],
    work_dir: &str,
    classified: &[ClassifiedPlan],
    admission: &[AdmissionDecision],
    classifications: &BTreeMap<String, crate::admission::PdfClassificationResult>,
    runtime_classification: &HashSet<String>,
    parse_outputs: &ParseOutputs,
) -> Result<Vec<ContextFileEvidence>, SchedulerFailure> {
    let snapshot: HashMap<String, &DiscoveredFileOut> = discovery
        .iter()
        .map(|file| (file.file_identity.clone(), file))
        .collect();
    let mut evidence = Vec::with_capacity(discovery.len());
    for plan in classified {
        let file = snapshot.get(&plan.file_identity).ok_or_else(|| {
            SchedulerFailure::internal("classified plan has no discovery file")
        })?;
        let relative_path = crate::context_audit::relative_contract_path(Path::new(work_dir), &file.path)
            .map_err(|message| SchedulerFailure::internal(&message))?;
        let admission_action = admission
            .iter()
            .find(|decision| decision.file_identity == plan.file_identity)
            .map(|decision| decision.action.clone());
        let action = admission_action.as_ref().unwrap_or(&plan.action);
        evidence.push(file_evidence(
            file,
            &relative_path,
            action,
            classifications,
            runtime_classification,
            parse_outputs,
        ));
    }
    Ok(evidence)
}

fn file_evidence(
    file: &DiscoveredFileOut,
    relative_path: &str,
    action: &PlanAction,
    classifications: &BTreeMap<String, crate::admission::PdfClassificationResult>,
    runtime_classification: &HashSet<String>,
    parse_outputs: &ParseOutputs,
) -> ContextFileEvidence {
    match action {
        PlanAction::NotParsed { reason } => ContextFileEvidence {
            file_identity: file.file_identity.clone(),
            absolute_path: file.path.clone(),
            relative_path: relative_path.to_string(),
            extension: file.extension.clone(),
            size_bytes: Some(file.size_bytes),
            content: String::new(),
            parser_backend: "not_parsed".to_string(),
            worker_lane: AuditWorkerLane::NotParsed,
            cache_status: CacheStatus::Miss,
            parse_status: ParseStatus::NotParsed,
            truncated: false,
            error: None,
            reason: Some(reason.as_str().to_string()),
        },
        PlanAction::Reject { reason } => {
            let (code, retryable) = match reason {
                RejectReason::SourceGuardUnavailable => (ErrorCode::SourceGuardUnavailable, true),
                RejectReason::ProfileRouteInvariant => (ErrorCode::ProfileRouteInvariant, false),
            };
            ContextFileEvidence {
                file_identity: file.file_identity.clone(),
                absolute_path: file.path.clone(),
                relative_path: relative_path.to_string(),
                extension: file.extension.clone(),
                size_bytes: Some(file.size_bytes),
                content: String::new(),
                parser_backend: "not_parsed".to_string(),
                worker_lane: AuditWorkerLane::NotParsed,
                cache_status: CacheStatus::Miss,
                parse_status: ParseStatus::Error,
                truncated: false,
                error: Some(Diagnostic {
                    error_code: code,
                    message: reason.as_str().to_string(),
                    retryable,
                    stage: DiagnosticStage::Parse,
                    file_path: Nullable(Some(file.path.clone())),
                    backend: Nullable(None),
                }),
                reason: Some(reason.as_str().to_string()),
            }
        }
        PlanAction::ClassifierFailed { status } => {
            let (parse_status, code, retryable) = match status {
                PdfClassificationStatus::Unknown => {
                    (ParseStatus::Timeout, ErrorCode::ParserTimeout, true)
                }
                PdfClassificationStatus::Error => {
                    (ParseStatus::Error, ErrorCode::ParserFailed, false)
                }
                _ => (ParseStatus::Error, ErrorCode::ParserFailed, false),
            };
            ContextFileEvidence {
                file_identity: file.file_identity.clone(),
                absolute_path: file.path.clone(),
                relative_path: relative_path.to_string(),
                extension: file.extension.clone(),
                size_bytes: Some(file.size_bytes),
                content: String::new(),
                parser_backend: "not_parsed".to_string(),
                worker_lane: AuditWorkerLane::NotParsed,
                cache_status: CacheStatus::Miss,
                parse_status,
                truncated: false,
                error: Some(Diagnostic {
                    error_code: code,
                    message: format!("pdf classification {status:?}"),
                    retryable,
                    stage: DiagnosticStage::Parse,
                    file_path: Nullable(Some(file.path.clone())),
                    backend: Nullable(Some("pdf_classifier".to_string())),
                }),
                reason: Some("parse_error".to_string()),
            }
        }
        PlanAction::Admit { .. } => {
            if runtime_classification.contains(&file.file_identity)
                || parse_outputs.runtime_not_parsed.contains(&file.file_identity)
            {
                // spec Part 2.3: runtime NotParsed (deadline) — NEVER snapshot.
                ContextFileEvidence {
                    file_identity: file.file_identity.clone(),
                    absolute_path: file.path.clone(),
                    relative_path: relative_path.to_string(),
                    extension: file.extension.clone(),
                    size_bytes: Some(file.size_bytes),
                    content: String::new(),
                    parser_backend: "not_parsed".to_string(),
                    worker_lane: AuditWorkerLane::NotParsed,
                    cache_status: CacheStatus::Miss,
                    parse_status: ParseStatus::NotParsed,
                    truncated: false,
                    error: None,
                    reason: Some("runtime_deadline_exhausted".to_string()),
                }
            } else if let Some(result) = parse_outputs.results.get(&file.file_identity) {
                ContextFileEvidence {
                    file_identity: file.file_identity.clone(),
                    absolute_path: file.path.clone(),
                    relative_path: relative_path.to_string(),
                    extension: file.extension.clone(),
                    size_bytes: Some(file.size_bytes),
                    content: result.content.clone(),
                    parser_backend: result.parser_backend.clone(),
                    worker_lane: parse_worker_lane(&result.worker_lane),
                    cache_status: CacheStatus::Miss,
                    parse_status: result.parse_status,
                    truncated: result.truncated,
                    error: result.error.clone(),
                    reason: if result.parse_status == ParseStatus::Success {
                        None
                    } else {
                        Some("parse_error".to_string())
                    },
                }
            } else {
                // no-text PDF metadata-only draft (no parser started).
                let status = classifications
                    .get(&file.file_identity)
                    .map(|r| r.status)
                    .unwrap_or(PdfClassificationStatus::NoTextInParseWindow);
                if status == PdfClassificationStatus::NoTextInParseWindow {
                    ContextFileEvidence {
                        file_identity: file.file_identity.clone(),
                        absolute_path: file.path.clone(),
                        relative_path: relative_path.to_string(),
                        extension: file.extension.clone(),
                        size_bytes: Some(file.size_bytes),
                        content: String::new(),
                        parser_backend: "pdf_metadata_v1".to_string(),
                        worker_lane: AuditWorkerLane::RustCore,
                        cache_status: CacheStatus::Miss,
                        parse_status: ParseStatus::Success,
                        truncated: false,
                        error: None,
                        reason: Some("pdf_no_text_in_parse_window".to_string()),
                    }
                } else {
                    ContextFileEvidence {
                        file_identity: file.file_identity.clone(),
                        absolute_path: file.path.clone(),
                        relative_path: relative_path.to_string(),
                        extension: file.extension.clone(),
                        size_bytes: Some(file.size_bytes),
                        content: String::new(),
                        parser_backend: "not_parsed".to_string(),
                        worker_lane: AuditWorkerLane::NotParsed,
                        cache_status: CacheStatus::Miss,
                        parse_status: ParseStatus::Error,
                        truncated: false,
                        error: Some(Diagnostic {
                            error_code: ErrorCode::InternalError,
                            message: "admitted text pdf has no parse result".to_string(),
                            retryable: false,
                            stage: DiagnosticStage::Parse,
                            file_path: Nullable(Some(file.path.clone())),
                            backend: Nullable(None),
                        }),
                        reason: Some("parse_error".to_string()),
                    }
                }
            }
        }
    }
}

fn enforce_rendered_within_reserved(
    rendered: &ContextBuildOutput,
    admission: &[AdmissionDecision],
    model: &ContextBudgetModel,
) -> Result<(), SchedulerFailure> {
    for decision in admission {
        if let PlanAction::Admit { .. } = decision.action {
            if let Some(rendered_chars) =
                rendered.rendered_by_identity.get(&decision.file_identity)
            {
                model
                    .check_rendered_within_reserved(*rendered_chars, decision.reserved_chars)
                    .map_err(|_| {
                        SchedulerFailure::new(
                            ErrorCode::BudgetModelMismatch,
                            format!(
                                "rendered section for {} exceeded its reserved budget",
                                decision.relative_path
                            ),
                            false,
                            DiagnosticStage::Context,
                        )
                    })?;
            }
        }
    }
    Ok(())
}

fn terminal_state(
    rendered: &ContextBuildOutput,
    file_results: &[FileResultRecord],
    runtime_classification: &HashSet<String>,
    parse_outputs: &ParseOutputs,
    stage_deadline_exhausted_count: u64,
) -> (TerminalIntent, Vec<RunDiagnosticRecord>) {
    let has_failure = rendered.error_file_count > 0
        || rendered.timeout_count > 0
        || !runtime_classification.is_empty()
        || !parse_outputs.runtime_not_parsed.is_empty()
        || stage_deadline_exhausted_count > 0;
    let mut diagnostics = Vec::new();
    if stage_deadline_exhausted_count > 0 {
        diagnostics.push(RunDiagnosticRecord {
            severity: DiagnosticSeverity::Warning,
            diagnostic: Diagnostic {
                error_code: ErrorCode::StageDeadlineExhausted,
                message: "work deadline exhausted; remaining work stopped".to_string(),
                retryable: true,
                stage: DiagnosticStage::Parse,
                file_path: Nullable(None),
                backend: Nullable(None),
            },
        });
    }
    // Per-file Error/Timeout become warnings (spec Part 2.3). The REAL final
    // Diagnostic is preserved verbatim (error_code/retryable/file_path/backend);
    // the run shell must not re-synthesize or collapse it.
    let mut primary_error: Option<Diagnostic> = None;
    if has_failure {
        for result in file_results {
            if let Some(error) = &result.error {
                if primary_error.is_none() {
                    primary_error = Some(error.clone());
                }
                diagnostics.push(RunDiagnosticRecord {
                    severity: DiagnosticSeverity::Warning,
                    diagnostic: error.clone(),
                });
            }
        }
    }
    let intent = if !has_failure {
        TerminalIntent::Success
    } else if rendered.included_file_count == 0 {
        // Cannot construct a valid non-empty context with included files.
        TerminalIntent::Error
    } else {
        TerminalIntent::Partial
    };
    if intent == TerminalIntent::Error {
        // spec Part 2.3: an Error envelope MUST carry an error Diagnostic. A
        // deadline-driven empty context uses the run-level deadline diagnostic
        // as the envelope error.
        if let Some(error) = primary_error {
            diagnostics.push(RunDiagnosticRecord {
                severity: DiagnosticSeverity::Error,
                diagnostic: error,
            });
        } else if stage_deadline_exhausted_count > 0 {
            diagnostics.push(RunDiagnosticRecord {
                severity: DiagnosticSeverity::Error,
                diagnostic: Diagnostic {
                    error_code: ErrorCode::StageDeadlineExhausted,
                    message: "work deadline exhausted before any included context".to_string(),
                    retryable: true,
                    stage: DiagnosticStage::Parse,
                    file_path: Nullable(None),
                    backend: Nullable(None),
                },
            });
        }
    }
    (intent, diagnostics)
}

/// spec Part 2.2 bounded warning projection: run-level warnings first, then up
/// to 256 detail rows, then a single `DIAGNOSTICS_AGGREGATED` row
/// (`stage=internal`, `retryable=any folded true`, `file_path/backend=null`).
/// Error-severity diagnostics pass through untouched (the envelope error).
fn project_warnings(diagnostics: Vec<RunDiagnosticRecord>) -> Vec<RunDiagnosticRecord> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for record in diagnostics {
        if record.severity == DiagnosticSeverity::Error {
            errors.push(record);
        } else {
            warnings.push(record);
        }
    }
    if warnings.len() <= 256 {
        warnings.extend(errors);
        return warnings;
    }
    let tail = warnings.split_off(256);
    warnings.push(aggregate_warning(&tail));
    warnings.extend(errors);
    warnings
}

fn aggregate_warning(folded: &[RunDiagnosticRecord]) -> RunDiagnosticRecord {
    let mut groups: BTreeMap<(String, String, bool), u64> = BTreeMap::new();
    let mut any_retryable = false;
    for record in folded {
        let key = (
            crate::store::inventory::enum_text(&record.diagnostic.stage),
            crate::store::inventory::enum_text(&record.diagnostic.error_code),
            record.diagnostic.retryable,
        );
        *groups.entry(key).or_insert(0) += 1;
        any_retryable |= record.diagnostic.retryable;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut other_count = 0_u64;
    for ((stage, code, retryable), count) in groups {
        let part = format!("{stage}:{code}:retryable={retryable}:{count}");
        let projected = parts.iter().map(|p| p.len() + 1).sum::<usize>() + part.len();
        if projected <= 4_096 {
            parts.push(part);
        } else {
            other_count = other_count.saturating_add(count);
        }
    }
    if other_count > 0 {
        parts.push(format!("other:{other_count}"));
    }
    let message = format!("aggregated {} diagnostics: {}", folded.len(), parts.join(","));
    RunDiagnosticRecord {
        severity: DiagnosticSeverity::Warning,
        diagnostic: Diagnostic {
            error_code: ErrorCode::DiagnosticsAggregated,
            message: message.chars().take(4_096).collect(),
            retryable: any_retryable,
            stage: DiagnosticStage::Internal,
            file_path: Nullable(None),
            backend: Nullable(None),
        },
    }
}

/// Defines a minimal Error outcome for defined internal terminal states
/// (`BUDGET_MODEL_MISMATCH`, `CONTEXT_FIXED_SECTIONS_OVER_BUDGET`,
/// `enforce_rendered_within_reserved`) — returned as `Ok`, never `Err`
/// (spec Solution: defined business terminal states are `Ok(outcome)`).
#[allow(clippy::too_many_arguments)]
fn internal_error_outcome(
    scan_run_id: u64,
    metrics: ExecutionMetrics,
    diagnostics: Vec<RunDiagnosticRecord>,
    parse_outputs: ParseOutputs,
    error_code: ErrorCode,
    message: String,
) -> BudgetedScanOutcome {
    let mut diagnostics = project_warnings(diagnostics);
    diagnostics.push(RunDiagnosticRecord {
        severity: DiagnosticSeverity::Error,
        diagnostic: Diagnostic {
            error_code,
            message: message.chars().take(4_096).collect(),
            retryable: false,
            stage: DiagnosticStage::Context,
            file_path: Nullable(None),
            backend: Nullable(None),
        },
    });
    BudgetedScanOutcome {
        scan_run_id,
        terminal_intent: TerminalIntent::Error,
        inventory: Vec::new(),
        file_results: Vec::new(),
        parse_cache_receipts: parse_outputs.parse_cache_receipts,
        classification_cache_receipts: parse_outputs.classification_cache_receipts,
        diagnostics,
        stage_metrics: Vec::new(),
        extension_metrics: Vec::new(),
        context: None,
        execution_metrics: metrics,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_file_results(
    classified: &[ClassifiedPlan],
    snapshot: &HashMap<String, &DiscoveredFileOut>,
    work_dir: &str,
    admission: &[AdmissionDecision],
    parse_outputs: &ParseOutputs,
    runtime_classification: &HashSet<String>,
    rejected_profile_hash: &str,
) -> Result<Vec<FileResultRecord>, SchedulerFailure> {
    let mut records = Vec::with_capacity(classified.len());
    for plan in classified {
        let file = snapshot.get(&plan.file_identity).ok_or_else(|| {
            SchedulerFailure::internal("classified plan has no discovery file")
        })?;
        let relative_path = crate::context_audit::relative_contract_path(Path::new(work_dir), &file.path)
            .map_err(|message| SchedulerFailure::internal(&message))?;
        let admission_action = admission
            .iter()
            .find(|decision| decision.file_identity == plan.file_identity)
            .map(|decision| decision.action.clone());
        let action = admission_action.as_ref().unwrap_or(&plan.action);
        let (parse_status, backend, lane, error, content_sha256, truncated, primary_ms, fallback_ms, total_ms, failure_class, fallback_backend, fallback_reason) =
            match action {
                PlanAction::NotParsed { .. } => (
                    ParseStatus::NotParsed,
                    "not_parsed".to_string(),
                    AuditWorkerLane::NotParsed,
                    None,
                    crate::store::sha256_hex(b""),
                    false,
                    0,
                    0,
                    0,
                    String::new(),
                    String::new(),
                    String::new(),
                ),
                PlanAction::Reject { reason } => (
                    ParseStatus::Error,
                    "not_parsed".to_string(),
                    AuditWorkerLane::NotParsed,
                    Some(Diagnostic {
                        error_code: match reason {
                            RejectReason::SourceGuardUnavailable => ErrorCode::SourceGuardUnavailable,
                            RejectReason::ProfileRouteInvariant => ErrorCode::ProfileRouteInvariant,
                        },
                        message: reason.as_str().to_string(),
                        retryable: reason == &RejectReason::SourceGuardUnavailable,
                        stage: DiagnosticStage::Parse,
                        file_path: Nullable(Some(file.path.clone())),
                        backend: Nullable(None),
                    }),
                    crate::store::sha256_hex(b""),
                    false,
                    0,
                    0,
                    0,
                    String::new(),
                    String::new(),
                    String::new(),
                ),
                PlanAction::ClassifierFailed { status } => (
                    if *status == PdfClassificationStatus::Unknown {
                        ParseStatus::Timeout
                    } else {
                        ParseStatus::Error
                    },
                    "not_parsed".to_string(),
                    AuditWorkerLane::NotParsed,
                    Some(Diagnostic {
                        error_code: if *status == PdfClassificationStatus::Unknown {
                            ErrorCode::ParserTimeout
                        } else {
                            ErrorCode::ParserFailed
                        },
                        message: format!("pdf classification {status:?}"),
                        retryable: *status == PdfClassificationStatus::Unknown,
                        stage: DiagnosticStage::Parse,
                        file_path: Nullable(Some(file.path.clone())),
                        backend: Nullable(Some("pdf_classifier".to_string())),
                    }),
                    crate::store::sha256_hex(b""),
                    false,
                    0,
                    0,
                    0,
                    String::new(),
                    String::new(),
                    String::new(),
                ),
                PlanAction::Admit { .. } => {
                    if runtime_classification.contains(&file.file_identity)
                        || parse_outputs.runtime_not_parsed.contains(&file.file_identity)
                    {
                        (
                            ParseStatus::NotParsed,
                            "not_parsed".to_string(),
                            AuditWorkerLane::NotParsed,
                            None,
                            crate::store::sha256_hex(b""),
                            false,
                            0,
                            0,
                            0,
                            String::new(),
                            String::new(),
                            String::new(),
                        )
                    } else if let Some(result) = parse_outputs.results.get(&file.file_identity) {
                        (
                            result.parse_status,
                            result.parser_backend.clone(),
                            parse_worker_lane(&result.worker_lane),
                            result.error.clone(),
                            result.content_sha256.clone(),
                            result.truncated,
                            result.primary_duration_ms,
                            result.fallback_duration_ms,
                            result.parse_duration_ms,
                            result.failure_class.clone(),
                            result.fallback_backend.clone(),
                            result.fallback_reason_code.clone(),
                        )
                    } else {
                        (
                            ParseStatus::Success,
                            "pdf_metadata_v1".to_string(),
                            AuditWorkerLane::RustCore,
                            None,
                            crate::store::sha256_hex(b""),
                            false,
                            0,
                            0,
                            0,
                            String::new(),
                            String::new(),
                            String::new(),
                        )
                    }
                }
            };
        let profile_hash = match action {
            PlanAction::Admit { .. } => parse_outputs
                .parse_profile_hashes
                .get(&file.file_identity)
                .cloned()
                .unwrap_or_else(|| rejected_profile_hash.to_string()),
            _ => rejected_profile_hash.to_string(),
        };
        let (cache_status, cache_miss_reason) = match action {
            PlanAction::Admit { .. } => (
                parse_outputs
                    .parse_cache_status
                    .get(&file.file_identity)
                    .copied()
                    .unwrap_or(CacheStatus::Miss),
                parse_outputs
                    .parse_cache_miss_reason
                    .get(&file.file_identity)
                    .copied()
                    .unwrap_or(CacheMissReason::NewFile),
            ),
            _ => (CacheStatus::Miss, CacheMissReason::NewFile),
        };
        records.push(FileResultRecord {
            file_identity: file.file_identity.clone(),
            relative_path,
            source_version: file.source_version.clone(),
            parse_profile_hash: profile_hash,
            cache_status,
            cache_miss_reason,
            parse_status,
            parser_backend: backend,
            worker_lane: lane,
            truncated,
            content_sha256,
            primary_duration_ms: primary_ms,
            fallback_duration_ms: fallback_ms,
            parse_duration_ms: total_ms,
            failure_class,
            fallback_backend,
            fallback_reason_code: fallback_reason,
            error,
        });
    }
    Ok(records)
}

fn build_stage_metrics(
    source_file_count: u64,
    context_item_count: u64,
    cache_item_count: u64,
    parse_item_count: u64,
    discovery_duration_ms: u64,
    cache_duration_ms: u64,
    parse_duration_ms: u64,
    context_duration_ms: u64,
) -> Vec<StageMetric> {
    vec![
        StageMetric {
            stage: StageName::Discovery,
            item_count: source_file_count,
            duration_ms: discovery_duration_ms,
        },
        StageMetric {
            stage: StageName::Cache,
            item_count: cache_item_count,
            duration_ms: cache_duration_ms,
        },
        StageMetric {
            stage: StageName::Parse,
            item_count: parse_item_count,
            duration_ms: parse_duration_ms,
        },
        StageMetric {
            stage: StageName::Context,
            item_count: context_item_count,
            duration_ms: context_duration_ms,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warning(error_code: ErrorCode, retryable: bool) -> RunDiagnosticRecord {
        RunDiagnosticRecord {
            severity: DiagnosticSeverity::Warning,
            diagnostic: Diagnostic {
                error_code,
                message: "synthetic".to_string(),
                retryable,
                stage: DiagnosticStage::Parse,
                file_path: Nullable(Some("C:\\f\\a.md".to_string())),
                backend: Nullable(None),
            },
        }
    }

    #[test]
    fn project_warnings_folds_overflow_into_one_aggregate() {
        let mut diagnostics: Vec<RunDiagnosticRecord> = (0..300)
            .map(|index| {
                warning(
                    if index % 2 == 0 {
                        ErrorCode::ParserFailed
                    } else {
                        ErrorCode::ParserTimeout
                    },
                    index % 3 == 0,
                )
            })
            .collect();
        // Keep one Error diagnostic to assert it passes through untouched.
        diagnostics.push(RunDiagnosticRecord {
            severity: DiagnosticSeverity::Error,
            diagnostic: Diagnostic {
                error_code: ErrorCode::InternalError,
                message: "primary".to_string(),
                retryable: false,
                stage: DiagnosticStage::Internal,
                file_path: Nullable(None),
                backend: Nullable(None),
            },
        });

        let projected = project_warnings(diagnostics);
        let warnings: Vec<_> = projected
            .iter()
            .filter(|record| record.severity == DiagnosticSeverity::Warning)
            .collect();
        let errors: Vec<_> = projected
            .iter()
            .filter(|record| record.severity == DiagnosticSeverity::Error)
            .collect();
        // 256 detail + 1 DIAGNOSTICS_AGGREGATED.
        assert_eq!(warnings.len(), 257);
        assert_eq!(
            warnings
                .iter()
                .filter(|record| record.diagnostic.error_code == ErrorCode::DiagnosticsAggregated)
                .count(),
            1
        );
        let aggregate = warnings
            .iter()
            .find(|record| record.diagnostic.error_code == ErrorCode::DiagnosticsAggregated)
            .expect("aggregate");
        assert_eq!(aggregate.diagnostic.stage, DiagnosticStage::Internal);
        assert!(aggregate.diagnostic.retryable, "any folded retryable => true");
        assert!(aggregate.diagnostic.file_path.0.is_none());
        assert!(aggregate.diagnostic.backend.0.is_none());
        assert_eq!(errors.len(), 1, "error diagnostics pass through untouched");
    }

    #[test]
    fn project_warnings_passes_through_few_warnings() {
        let diagnostics = vec![
            warning(ErrorCode::ParserFailed, false),
            warning(ErrorCode::ParserTimeout, true),
        ];
        let projected = project_warnings(diagnostics);
        assert_eq!(projected.len(), 2);
        assert!(!projected
            .iter()
            .any(|record| record.diagnostic.error_code == ErrorCode::DiagnosticsAggregated));
    }
}
