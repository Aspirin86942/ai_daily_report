use super::{
    allocation::{allocate_excerpts, ExcerptInput},
    parse::{self, Candidate},
    scheduler::Scheduler,
    sha256_hex,
    sources::{ScanBenchFile, ScannedFile, SourceProfile},
};
use chrono::{DateTime, Utc};
use daily_parsers::{
    contract::{ClassifyStatus, WorkerDiagnostic},
    settings::{
        Diagnostic, DiagnosticStage, EngineStatus, ErrorCode, NormalizedScannerSettings,
        ParseStatus, RefillRoundAudit,
    },
};
use serde::{de::value::StrDeserializer, Deserialize};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub status: EngineStatus,
    pub warnings: Vec<Diagnostic>,
    pub stats: ScanStats,
    pub peak_pdf_requests_in_flight: u64,
    pub files: Vec<ScannedFile>,
    pub source_profile: SourceProfile,
}

#[derive(Debug, Clone, Default)]
pub struct ScanStats {
    pub parse_attempt_count: u64,
    pub parse_cache_lookup_count: u64,
    pub parse_cache_all_hit: Option<bool>,
    pub classify_attempt_count: u64,
    pub classification_cache_lookup_count: u64,
    pub refunded_classification_pages: u64,
    pub refill_rounds: Vec<RefillRoundAudit>,
    pub rendered_chars: u64,
    pub reserved_chars: u64,
    pub snapshot_hit: bool,
    pub discovery_ms: u64,
    pub snapshot_lookup_ms: u64,
    pub parse_ms: u64,
    pub allocation_ms: u64,
    pub total_duration_ms: u64,
    pub cache_commit_ms: u64,
}

#[derive(Debug, Clone)]
pub struct PdfRuntime {
    pub status: ClassifyStatus,
    pub examined_pages: Option<u64>,
    pub page_count: Option<u64>,
    pub reused: bool,
    pub attempted: bool,
    pub requests: u64,
    pub native_document_opens: u64,
    pub native_page_reads: u64,
}

pub(super) struct Scan<'a> {
    pub files: &'a [Candidate],
    pub observed_at: DateTime<Utc>,
    pub profile: &'a NormalizedScannerSettings,
    pub warnings: Vec<Diagnostic>,
    pub scheduler: &'a Scheduler<'a>,
    pub discovery_failed: bool,
    pub snapshot_hit: bool,
    pub discovery_ms: u64,
    pub parse_ms: u64,
    pub snapshot_ms: u64,
}

pub(super) fn char_budget(profile: &NormalizedScannerSettings) -> u64 {
    parse::MAX_TOTAL_CHARS
        .min(profile.parse.aggregate_max_chars)
        .min(profile.context.global_max_chars)
}

pub(super) fn assemble<const BENCHMARK: bool>(
    scan: Scan<'_>,
    benchmark: &mut Vec<ScanBenchFile>,
) -> ScanResult {
    let allocation_started = BENCHMARK.then(Instant::now);
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
    let budget = char_budget(scan.profile);
    let excerpts = allocate_excerpts(
        &excerpt_inputs,
        budget as usize,
        &scan.profile.parse.excerpt_priority_paths,
    );
    let mut warnings = scan.warnings;
    let mut stats = ScanStats {
        snapshot_hit: scan.snapshot_hit,
        parse_cache_lookup_count: scan.scheduler.lookup_count,
        classification_cache_lookup_count: scan.scheduler.pdf_lookup_count,
        refunded_classification_pages: scan.scheduler.pages_refunded,
        discovery_ms: scan.discovery_ms,
        snapshot_lookup_ms: scan.snapshot_ms,
        parse_ms: scan.parse_ms,
        reserved_chars: budget,
        ..Default::default()
    };
    let mut files = Vec::with_capacity(scan.files.len());
    if BENCHMARK {
        benchmark.reserve_exact(scan.files.len());
    }
    let mut parse_hits = 0;
    for (index, (file, excerpt)) in scan.files.iter().zip(excerpts).enumerate() {
        let completed = file.result();
        let native = file.completed.as_ref().expect("native scan completed");
        let pdf = native.pdf.as_ref();
        let ocr = file.ocr.is_some();
        parse_hits += u64::from(native.reused) + u64::from(ocr && completed.reused);
        let parse_attempted = completed.attempted
            && (ocr
                || completed.pdf.as_ref().is_none_or(|pdf| {
                    pdf.extraction_slot
                        && pdf.classification.status == ClassifyStatus::TextInParseWindow
                }));
        let status = completed.outcome.status();
        let worker_diagnostic = completed.diagnostic();
        let diagnostic = worker_diagnostic
            .filter(|_| matches!(status, ParseStatus::Error | ParseStatus::Timeout))
            .map(diagnostic);
        let source = ScannedFile::new(
            file,
            native,
            excerpt,
            parse_attempted,
            diagnostic.as_ref().map(|error| error.error_code),
            scan.profile,
        );
        if BENCHMARK {
            let input = &excerpt_inputs[index];
            let ocr_pdf = file.ocr.as_ref().and_then(|complete| complete.pdf.as_ref());
            benchmark.push(ScanBenchFile {
                input_chars: input.content_chars as u64,
                content_sha256: sha256_hex(input.content.as_bytes()),
                parse_duration_ms: if parse_attempted
                    && status != ParseStatus::NotParsed
                    && !(completed.reused && completed.obtained().is_some())
                {
                    completed.duration_ms
                } else {
                    0
                },
                diagnostic_code: worker_diagnostic.map(|error| error.error_code.clone()),
                final_diagnostic: diagnostic.clone(),
                source_guard_kind: file.guard.as_ref().map(|guard| guard.kind),
                parse_attempted,
                pdf_runtime: pdf.map(|pdf| PdfRuntime {
                    status: pdf.classification.status,
                    examined_pages: pdf.classification.result_examined_pages,
                    page_count: pdf.classification.page_count,
                    reused: native.reused,
                    attempted: native.attempted,
                    requests: pdf.requests + ocr_pdf.map_or(0, |ocr| ocr.requests),
                    native_document_opens: pdf.document_opens
                        + ocr_pdf.map_or(0, |ocr| ocr.document_opens),
                    native_page_reads: pdf.page_reads + ocr_pdf.map_or(0, |ocr| ocr.page_reads),
                }),
            });
        }
        warnings.extend(diagnostic);
        stats.rendered_chars += source.excerpt.chars as u64;
        stats.parse_attempt_count += u64::from(parse_attempted);
        stats.classify_attempt_count += u64::from(pdf.is_some() && native.attempted);
        files.push(source);
    }
    warnings.truncate(100_000);
    let lookups = stats.parse_cache_lookup_count;
    stats.parse_cache_all_hit = (lookups != 0).then_some(parse_hits == lookups);
    let source_profile = SourceProfile::new(scan.profile, scan.observed_at);
    stats.allocation_ms = allocation_started.map_or(0, parse::elapsed_ms);
    let status = if scan.discovery_failed {
        EngineStatus::Error
    } else if warnings.is_empty() {
        EngineStatus::Ok
    } else {
        EngineStatus::Partial
    };
    ScanResult {
        status,
        warnings,
        stats,
        peak_pdf_requests_in_flight: scan.scheduler.pdf_activity.peak(),
        files,
        source_profile,
    }
}

fn diagnostic(error: &WorkerDiagnostic) -> Diagnostic {
    let code = ErrorCode::deserialize(StrDeserializer::<serde::de::value::Error>::new(
        &error.error_code,
    ))
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
        file_path: error.file_path.clone(),
        backend,
    }
}
