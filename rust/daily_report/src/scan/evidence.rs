use super::{
    allocation::{allocate_excerpts, ExcerptInput},
    parse::{self, Candidate, Completed, Outcome},
    scheduler::{Scheduler, REFILL_ROUNDS},
    sha256_hex,
    sources::{ScanSource, ScannedFile, SourceProfile},
    ContextResult, PdfRuntime, CONTRACT, ENGINE_BUILD, PARSER_CONTENT_IDENTITY,
};
use chrono::{DateTime, Utc};
use daily_parsers::{
    contract::{ClassifyStatus, WorkerDiagnostic, WorkerLane},
    pdf_wire::PdfNegativeKind,
    settings::*,
};
use serde::{ser::SerializeStruct, Serialize, Serializer};
use std::{collections::BTreeMap, fmt::Write as _, time::Instant};

pub(super) struct Scan<'a> {
    pub files: &'a [Candidate],
    pub request_id: String,
    pub observed_at: DateTime<Utc>,
    pub profile: &'a NormalizedScannerSettings,
    pub warnings: Vec<Diagnostic>,
    pub scheduler: &'a Scheduler<'a>,
    pub discovery_failed: bool,
    pub snapshot_source: Option<u64>,
    pub started: Instant,
    pub discovery_ms: u64,
    pub parse_ms: u64,
    pub snapshot_ms: u64,
}

pub(super) fn char_budget(profile: &NormalizedScannerSettings) -> u64 {
    parse::MAX_TOTAL_CHARS
        .min(profile.parse.aggregate_max_chars)
        .min(profile.context.global_max_chars)
}

pub(super) fn scan_id(request_id: &str) -> u64 {
    let hash = sha256_hex(request_id.as_bytes());
    u64::from_str_radix(&hash[..16], 16)
        .expect("valid SHA-256 hex")
        .max(1)
}

pub(super) fn assemble(scan: Scan<'_>) -> ContextResult {
    let compression_started = Instant::now();
    let excerpt_inputs: Vec<_> = scan
        .files
        .iter()
        .map(|file| {
            let result = file.result().obtained().map(|(result, _)| result);
            let content = result.map_or("", |result| result.content.as_str());
            ExcerptInput {
                relative_path: &file.relative_path,
                file_identity: &file.file.file_identity,
                extension: &file.file.extension,
                content,
                content_chars: content.chars().count(),
                evidence_audit: result.and_then(|result| result.evidence_audit.as_ref()),
                size_bytes: file.file.size_bytes,
                truncated: result.is_some_and(|result| result.truncated),
            }
        })
        .collect();
    let excerpts = allocate_excerpts(
        &excerpt_inputs,
        char_budget(scan.profile) as usize,
        &scan.profile.parse.excerpt_priority_paths,
    );
    let snapshot_hit = scan.snapshot_source.is_some();
    let mut warnings = scan.warnings;
    let mut summary = ContextSummary {
        source_file_count: scan.files.len() as u64,
        discovery_duration_ms: scan.discovery_ms,
        parse_duration_ms: scan.parse_ms,
        ..Default::default()
    };
    let mut metrics = ExecutionMetricsV2 {
        discovery_observed_file_count: summary.source_file_count,
        snapshot_hit,
        parse_cache_lookup_count: scan.scheduler.lookup_count,
        classification_cache_lookup_count: scan.scheduler.pdf_lookup_count,
        discovery_ms: scan.discovery_ms,
        snapshot_lookup_ms: scan.snapshot_ms,
        ..Default::default()
    };
    let mut file_context = "# File context\n".to_owned();
    let mut files = Vec::with_capacity(scan.files.len());
    let mut decisions = Vec::with_capacity(scan.files.len());
    let mut extension_metrics = BTreeMap::new();
    let mut pdf_runtime = Vec::new();
    let mut parse_hits = 0;
    let mut pdf_hits = 0;
    let source_profile = SourceProfile::new(scan.profile, scan.observed_at);
    for ((file, excerpt), input) in scan.files.iter().zip(excerpts).zip(&excerpt_inputs) {
        let completed = file.result();
        let native = file.completed.as_ref().expect("native scan completed");
        let ocr = file.ocr.is_some();
        parse_hits += u64::from(native.reused) + u64::from(ocr && completed.reused);
        let parse_attempted = completed.attempted
            && (ocr
                || completed.pdf.as_ref().is_none_or(|pdf| {
                    pdf.extraction_slot
                        && pdf.classification.status == ClassifyStatus::TextInParseWindow
                }));
        let status = completed.outcome.status();
        let failed = matches!(status, ParseStatus::Error | ParseStatus::Timeout);
        let parsed = completed.obtained();
        let pdf_audit = pdf_classification(file, native, snapshot_hit);
        let truncated = parsed.is_some_and(|(result, _)| result.truncated)
            || matches!(
                completed.outcome,
                Outcome::Negative(PdfNegativeKind::NoText)
            ) && pdf_audit
                .as_ref()
                .is_some_and(|pdf| pdf.page_count.0 > pdf.result_examined_pages.0);
        let (backend, lane) = match &completed.outcome {
            _ if ocr && (parsed.is_some() || parse_attempted) => {
                (OCR_BACKEND, AuditWorkerLane::RustPdfProcessV3)
            }
            Outcome::Negative(PdfNegativeKind::NoText) => {
                ("pdf_metadata_v2", AuditWorkerLane::RustCore)
            }
            _ if parsed.is_some() || (parse_attempted && failed) => {
                let backend = file.backend.expect("parsed file has a backend");
                let lane = match backend.lane() {
                    WorkerLane::RustCore => AuditWorkerLane::RustCore,
                    WorkerLane::RustOfficeInProcess => AuditWorkerLane::RustOfficeInProcess,
                    WorkerLane::RustOfficeProcessV3 => AuditWorkerLane::RustOfficeProcessV3,
                    WorkerLane::RustPdfProcessV3 => AuditWorkerLane::RustPdfProcessV3,
                };
                (backend.as_str(), lane)
            }
            _ => ("not_parsed", AuditWorkerLane::NotParsed),
        };
        let can_reuse = completed.reused && parsed.is_some();
        let parsed_now = parse_attempted && status != ParseStatus::NotParsed;
        let (cache_status, transport) = match (can_reuse, snapshot_hit, parsed_now) {
            (true, true, _) => (ParseCacheStatus::Snapshot, ParseTransport::Snapshot),
            (true, false, _) => (ParseCacheStatus::Fresh, ParseTransport::NotApplicable),
            (false, _, true) if completed.pdf.is_some() => {
                (ParseCacheStatus::Miss, ParseTransport::Session)
            }
            (false, _, true) => (ParseCacheStatus::Miss, ParseTransport::RustInProcess),
            _ => (
                ParseCacheStatus::NotApplicable,
                ParseTransport::NotApplicable,
            ),
        };
        let miss = cache_status == ParseCacheStatus::Miss;
        let duration_ms = if miss { completed.duration_ms } else { 0 };
        let diagnostic = completed.diagnostic().filter(|_| failed).map(diagnostic);
        warnings.extend(diagnostic.iter().cloned());
        let error_code = completed
            .diagnostic()
            .map_or("", |error| error.error_code.as_str());
        let audit = FileAuditV2 {
            relative_path: file.relative_path.clone(),
            file_identity: file.file.file_identity.clone(),
            source_version: file.file.source_version.clone(),
            source_guard_kind: file
                .guard
                .as_ref()
                .map_or(SourceGuardKind::Unavailable, |guard| guard.kind),
            source_guard_sha256: Nullable(file.guard.as_ref().map(|guard| guard.sha256.clone())),
            parse_status: status,
            parser_backend: backend.into(),
            worker_lane: lane,
            parse_cache_status: cache_status,
            cache_miss_reason: miss_reason(miss),
            truncated,
            content_sha256: sha256_hex(input.content.as_bytes()),
            parse_duration_ms: duration_ms,
            failure_class: (if failed { error_code } else { "" }).into(),
            fallback_backend: String::new(),
            fallback_reason_code: String::new(),
            parse_transport: transport,
            parse_attempt_count: u64::from(miss),
            final_diagnostic: Nullable(diagnostic),
            pdf_classification: Nullable(pdf_audit),
        };
        let source = ScannedFile::new(file, excerpt, audit, scan.profile);
        let output_chars = source.excerpt.chars as u64;
        let input_chars = input.content_chars as u64;
        let action = match status {
            ParseStatus::NotParsed => ContextAction::Omit,
            ParseStatus::Timeout | ParseStatus::Error => ContextAction::Error,
            ParseStatus::Success if output_chars == 0 => ContextAction::MetadataOnly,
            ParseStatus::Success if source.excerpt.truncated => ContextAction::Compress,
            ParseStatus::Success => ContextAction::Keep,
        };
        if output_chars > 0 {
            write!(
                file_context,
                "\n## {}\n{}\n",
                source.audit.relative_path, source.excerpt.text
            )
            .expect("writing to owned string");
        }
        let reason = source.reason.unwrap_or("parsed");
        decisions.push(ContextDecision {
            relative_path: file.relative_path.clone(),
            action,
            reason: reason.into(),
            priority: file.priority,
            input_chars,
            output_chars,
            truncated: source.audit.truncated || source.excerpt.truncated,
            error_code: error_code.into(),
        });
        let extension = extension_metrics
            .entry(&file.file.extension)
            .or_insert_with(|| ExtensionMetric {
                extension: file.file.extension.clone(),
                file_count: 0,
                parse_duration_ms: 0,
                success_count: 0,
                error_count: 0,
                timeout_count: 0,
            });
        extension.file_count += 1;
        extension.parse_duration_ms += duration_ms;
        match status {
            ParseStatus::Success => {
                summary.success_count += 1;
                extension.success_count += 1;
            }
            ParseStatus::Error => {
                summary.error_file_count += 1;
                extension.error_count += 1;
            }
            ParseStatus::Timeout => {
                summary.timeout_count += 1;
                extension.timeout_count += 1;
            }
            ParseStatus::NotParsed => {}
        }
        summary.included_file_count += u64::from(output_chars > 0);
        summary.omitted_file_count += u64::from(output_chars == 0);
        summary.input_chars += input_chars;
        metrics.candidate_file_count += u64::from(file.guard.is_some());
        metrics.admitted_file_count += u64::from(status == ParseStatus::Success || parse_attempted);
        metrics.parse_attempt_count += u64::from(parse_attempted);
        if file
            .guard
            .as_ref()
            .is_some_and(|guard| guard.kind == SourceGuardKind::ContentSha256V1)
        {
            metrics.source_guard_content_hash_file_count += 1;
            metrics.source_guard_bytes_read += file.file.size_bytes;
        }
        metrics.source_guard_unavailable_count += u64::from(reason == "source_guard_unavailable");
        if let Some(pdf) = &native.pdf {
            pdf_hits += u64::from(native.reused);
            metrics.classification_slot_count += 1;
            metrics.nominal_charged_pages_total += pdf.charged_pages;
            metrics.extraction_slot_count += u64::from(pdf.extraction_slot);
            if native.attempted {
                metrics.classify_attempt_count += 1;
                if let Some(pages) = pdf.classification.result_examined_pages {
                    metrics.confirmed_run_inspected_pages_total += pages;
                } else if pdf.classification.status == ClassifyStatus::Unknown {
                    metrics.unobserved_classification_attempt_count += 1;
                }
            }
            let ocr = file.ocr.as_ref().and_then(|complete| complete.pdf.as_ref());
            pdf_runtime.push(PdfRuntime {
                relative_path: file.relative_path.clone(),
                requests: pdf.requests + ocr.map_or(0, |ocr| ocr.requests),
                native_document_opens: pdf.document_opens + ocr.map_or(0, |ocr| ocr.document_opens),
                native_page_reads: pdf.page_reads + ocr.map_or(0, |ocr| ocr.page_reads),
            });
        }
        files.push(source);
    }
    warnings.truncate(100_000);
    metrics.stage_deadline_exhausted_count = u64::from(
        warnings
            .iter()
            .any(|warning| warning.error_code == ErrorCode::StageDeadlineExhausted),
    );
    let all_hit = |hits, lookups| Nullable((lookups != 0).then_some(hits == lookups));
    metrics.parse_cache_all_hit = all_hit(parse_hits, metrics.parse_cache_lookup_count);
    metrics.classification_cache_all_hit =
        all_hit(pdf_hits, metrics.classification_cache_lookup_count);
    metrics.rendered_chars = decisions.iter().map(|decision| decision.output_chars).sum();
    metrics.reserved_chars = char_budget(scan.profile);
    if !scan.scheduler.rounds.is_empty() {
        let deadline = metrics.stage_deadline_exhausted_count > 0;
        let quota = decisions
            .iter()
            .any(|decision| decision.reason == "pdf_classification_page_quota_exhausted");
        metrics.refill_audit = Some(RefillAudit {
            policy_version: scan.profile.admission_policy_version.clone(),
            stop_reason: if deadline {
                "deadline"
            } else if quota {
                "quota_exhausted"
            } else {
                "complete"
            }
            .into(),
            max_rounds: REFILL_ROUNDS,
            candidate_cap: scan.profile.max_candidate_files,
            classification_page_cap: scan.profile.max_total_pdf_classification_pages,
            extraction_cap: scan.profile.max_pdf_text_extractions,
            context_char_cap: char_budget(scan.profile),
            candidate_slots_used: metrics.candidate_file_count,
            classification_pages_held: metrics
                .nominal_charged_pages_total
                .saturating_sub(scan.scheduler.pages_refunded),
            extraction_slots_used: metrics.extraction_slot_count,
            refunded_chars: 0,
            refunded_classification_pages: scan.scheduler.pages_refunded,
            rounds: scan.scheduler.rounds.clone(),
        });
    }
    let mut error = None;
    let (status, run_status) = if scan.discovery_failed {
        error = warnings.first().cloned();
        file_context.clear();
        (EngineStatus::Error, RunStatus::Error)
    } else if warnings.is_empty() {
        (EngineStatus::Ok, RunStatus::Success)
    } else {
        (EngineStatus::Partial, RunStatus::Partial)
    };
    summary.output_chars = file_context.chars().count() as u64;
    summary.compression_duration_ms = parse::elapsed_ms(compression_started);
    summary.total_duration_ms = parse::elapsed_ms(scan.started);
    let id = scan_id(&scan.request_id);
    let reuse_kind = if snapshot_hit {
        ReuseKind::ContextSnapshot
    } else if metrics.parse_cache_all_hit.0 == Some(true) {
        ReuseKind::ParseCache
    } else {
        ReuseKind::None
    };
    let stage_metrics = vec![
        StageMetric {
            stage: StageName::Discovery,
            item_count: summary.source_file_count,
            duration_ms: scan.discovery_ms,
        },
        StageMetric {
            stage: StageName::Cache,
            item_count: scan.scheduler.lookup_count,
            duration_ms: scan.snapshot_ms,
        },
        StageMetric {
            stage: StageName::Parse,
            item_count: metrics.parse_attempt_count,
            duration_ms: scan.parse_ms,
        },
        StageMetric {
            stage: StageName::Context,
            item_count: summary.included_file_count,
            duration_ms: summary.compression_duration_ms,
        },
    ];
    ContextResult {
        envelope: ContextEnvelope {
            contract: CONTRACT.into(),
            protocol_version: 1,
            request_id: scan.request_id.clone(),
            engine_version: env!("CARGO_PKG_VERSION").into(),
            engine_build: ENGINE_BUILD.into(),
            status,
            file_context,
            summary: summary.clone(),
            scan_run_id: Nullable(Some(id)),
            context_run_id: Nullable(Some(id)),
            warnings: warnings.clone(),
            error: Nullable(error),
        },
        evidence_summary: ScannerEvidence {
            contract: CONTRACT.into(),
            protocol_version: 1,
            request_id: scan.request_id,
            scan_run_id: id,
            context_run_id: Nullable(Some(id)),
            run_status,
            summary,
            stage_metrics,
            extension_metrics: extension_metrics.into_values().collect(),
            files: Vec::new(),
            decisions,
            warnings,
            artifact_id: Nullable((status != EngineStatus::Error).then_some(id)),
            reused_from_context_run_id: Nullable(scan.snapshot_source),
            reuse_kind,
            execution_metrics: metrics,
        },
        files,
        source_profile,
        pdf_runtime,
        peak_pdf_requests_in_flight: scan.scheduler.pdf_activity.peak(),
    }
}

impl ContextResult {
    /// The serialized evidence view includes one audit row per file in admission order.
    pub fn evidence(&self) -> ScannerEvidence {
        let mut evidence = self.evidence_summary.clone();
        evidence.files = self.files.iter().map(|file| file.audit.clone()).collect();
        evidence
    }

    /// The serialized source view is ordered by relative path.
    pub fn sources(&self) -> Vec<ScanSource> {
        let mut files: Vec<_> = self.files.iter().collect();
        files.sort_by(|a, b| a.audit.relative_path.cmp(&b.audit.relative_path));
        files
            .into_iter()
            .map(|file| file.scan_view(&self.source_profile))
            .collect()
    }
}

impl Serialize for ContextResult {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut result = serializer.serialize_struct("ContextResult", 5)?;
        result.serialize_field("envelope", &self.envelope)?;
        result.serialize_field("evidence", &self.evidence())?;
        result.serialize_field("sources", &self.sources())?;
        result.serialize_field("pdf_runtime", &self.pdf_runtime)?;
        result.serialize_field(
            "peak_pdf_requests_in_flight",
            &self.peak_pdf_requests_in_flight,
        )?;
        result.end()
    }
}

fn pdf_classification(
    file: &Candidate,
    completed: &Completed,
    snapshot_hit: bool,
) -> Option<PdfClassificationAuditV1> {
    let pdf = completed.pdf.as_ref();
    let omitted = completed.outcome.reason() == Some("pdf_classification_page_quota_exhausted");
    if pdf.is_none() && !omitted {
        return None;
    }
    let result = pdf.map(|pdf| &pdf.classification);
    let status = match result.map(|result| result.status) {
        Some(ClassifyStatus::TextInParseWindow) => PdfClassificationStatus::TextInParseWindow,
        Some(ClassifyStatus::NoTextInParseWindow) => PdfClassificationStatus::NoTextInParseWindow,
        Some(ClassifyStatus::Unknown) => PdfClassificationStatus::Unknown,
        Some(ClassifyStatus::Error) => PdfClassificationStatus::Error,
        None => PdfClassificationStatus::NotClassifiedByBudget,
    };
    let open_failure = matches!(
        completed.outcome,
        Outcome::Negative(PdfNegativeKind::InvalidPdf | PdfNegativeKind::Protected)
    );
    let (cache_status, transport) = match (omitted, completed.reused, snapshot_hit && !open_failure)
    {
        (true, _, _) => (
            ClassificationCacheStatus::NotEligible,
            ClassificationTransport::NotApplicable,
        ),
        (false, true, true) => (
            ClassificationCacheStatus::Snapshot,
            ClassificationTransport::Snapshot,
        ),
        (false, true, false) => (
            ClassificationCacheStatus::Fresh,
            ClassificationTransport::NotApplicable,
        ),
        _ => (
            ClassificationCacheStatus::Miss,
            ClassificationTransport::Session,
        ),
    };
    let key = completed.query.as_ref().map(|query| &query.key);
    let examined_pages = if omitted {
        Some(0)
    } else {
        result.and_then(|r| r.result_examined_pages)
    };
    Some(PdfClassificationAuditV1 {
        pdf_open_failure_key: key.filter(|_| open_failure).map(|key| {
            sha256_hex(
                format!("{}/{}/{}", key.file_id, key.source_guard, key.profile_hash).as_bytes(),
            )
        }),
        page_audit: result.and_then(|r| r.page_audit.clone()),
        status,
        page_count: Nullable(result.and_then(|r| r.page_count)),
        classification_cache_status: cache_status,
        classification_cache_miss_reason: miss_reason(
            cache_status == ClassificationCacheStatus::Miss,
        ),
        result_examined_pages: Nullable(examined_pages),
        run_inspected_pages: Nullable(if completed.attempted {
            examined_pages
        } else {
            Some(0)
        }),
        nominal_charged_pages: pdf.map_or(0, |pdf| pdf.charged_pages),
        duration_ms: completed.duration_ms,
        transport,
        attempt_count: u64::from(completed.attempted),
        classifier_build: PARSER_CONTENT_IDENTITY.into(),
        classifier_profile_hash: key.map_or_else(
            || sha256_hex(file.file.extension.as_bytes()),
            |key| key.profile_hash.clone(),
        ),
    })
}

fn diagnostic(error: &WorkerDiagnostic) -> Diagnostic {
    let code = serde_json::from_value::<ErrorCode>(error.error_code.clone().into())
        .unwrap_or(ErrorCode::ParserFailed);
    let backend = if !error.retryable
        && matches!(code, ErrorCode::ParserFailed | ErrorCode::PdfProtected)
        && error.backend.as_deref() == Some("rust_pdfium_v3")
        && error.message.contains("FPDF_ERR_")
    {
        Some("pdf_classifier".into())
    } else {
        error.backend.clone()
    };
    Diagnostic {
        error_code: code,
        stage: DiagnosticStage::Parse,
        message: error.message.chars().take(4096).collect(),
        retryable: error.retryable,
        file_path: Nullable(error.file_path.clone()),
        backend: Nullable(backend),
    }
}

fn miss_reason(missed: bool) -> String {
    if missed {
        "entry_absent_or_evicted".into()
    } else {
        String::new()
    }
}

pub(super) fn add_warning(result: &mut ContextResult, diagnostic: Diagnostic) {
    if result.envelope.status == EngineStatus::Ok {
        result.envelope.status = EngineStatus::Partial;
    }
    result.envelope.warnings.push(diagnostic.clone());
    if result.evidence_summary.run_status == RunStatus::Success {
        result.evidence_summary.run_status = RunStatus::Partial;
    }
    result.evidence_summary.warnings.push(diagnostic);
}

pub(super) fn finish(result: &mut ContextResult, started: Instant, cache_commit_ms: u64) {
    let elapsed = parse::elapsed_ms(started);
    result.envelope.summary.total_duration_ms = elapsed;
    result.evidence_summary.summary.total_duration_ms = elapsed;
    if let Some(cache) = result
        .evidence_summary
        .stage_metrics
        .iter_mut()
        .find(|metric| metric.stage == StageName::Cache)
    {
        cache.duration_ms += cache_commit_ms;
    }
}
