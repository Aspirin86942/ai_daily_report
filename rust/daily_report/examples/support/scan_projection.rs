use ai_daily_report::scan::{ScanBenchFile, ScanResult};
use daily_parsers::{
    contract::{ClassifyStatus, SourceGuardKind},
    settings::{EngineStatus, ErrorCode, ParseStatus},
};
use serde::Serialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fmt::Write as _};

#[derive(Default, Serialize)]
struct ExtensionMetric<'a> {
    extension: &'a str,
    file_count: u64,
    parse_duration_ms: u64,
    success_count: u64,
    error_count: u64,
    timeout_count: u64,
}

#[derive(Default)]
struct ExecutionCounts {
    guarded_files: u64,
    guarded_bytes: u64,
    unavailable_guards: u64,
    candidates: u64,
    admitted: u64,
    classification_slots: u64,
    classification_hits: u64,
    extraction_slots: u64,
    reserved_pages: u64,
    inspected_pages: u64,
    unknown_classifications: u64,
}

/// Project the scan facts into the retained M4 driver format after timing the scan.
pub(super) fn project(scan: &ScanResult, observed: &[ScanBenchFile], engine_build: &str) -> Value {
    let files = &scan.files;
    assert_eq!(
        files.len(),
        observed.len(),
        "missing per-file scan observations"
    );
    let profile = &scan.source_profile;
    let stats = &scan.stats;
    let mut file_context = String::from("# File context\n");
    let mut file_rows = Vec::with_capacity(files.len());
    let mut decisions = Vec::with_capacity(files.len());
    let mut pdf_runtime = Vec::new();
    let mut extensions = BTreeMap::new();
    let mut counts = ExecutionCounts::default();
    let mut input_chars = 0;
    let mut included = 0;
    for (file, facts) in files.iter().zip(observed) {
        let excerpt = file.benchmark_excerpt();
        let chars = excerpt.chars;
        let action = match file.parse_status {
            ParseStatus::NotParsed => "omit",
            ParseStatus::Timeout | ParseStatus::Error => "error",
            ParseStatus::Success if chars == 0 => "metadata_only",
            ParseStatus::Success if excerpt.truncated => "compress",
            ParseStatus::Success => "keep",
        };
        if chars > 0 {
            write!(
                file_context,
                "\n## {}\n{}\n",
                file.relative_path, excerpt.text
            )
            .expect("writing to an owned string");
            included += 1;
        }
        input_chars += facts.input_chars;
        decisions.push(json!({
            "relative_path": file.relative_path,
            "action": action,
            "reason": file.reason.unwrap_or("parsed"),
            "priority": file.priority,
            "input_chars": facts.input_chars,
            "output_chars": chars,
            "truncated": file.parse_truncated || excerpt.truncated,
            "error_code": facts.diagnostic_code.as_deref().unwrap_or_default(),
        }));
        let skipped_classification = file.reason == Some("pdf_classification_page_quota_exhausted");
        let runtime = facts.pdf_runtime.as_ref();
        let classification_status = match runtime {
            Some(pdf) => Some(json!(pdf.status)),
            None if skipped_classification => Some(json!("not_classified_by_budget")),
            None => None,
        };
        file_rows.push(json!({
            "relative_path": file.relative_path,
            "parse_status": file.parse_status,
            "parser_backend": file.parser_backend,
            "worker_lane": file.worker_lane,
            "truncated": file.parse_truncated,
            "content_sha256": facts.content_sha256,
            "parse_duration_ms": facts.parse_duration_ms,
            "final_diagnostic": facts.final_diagnostic,
            "pdf_classification": classification_status.map(|status| json!({
                "status": status,
                "page_count": runtime.and_then(|pdf| pdf.page_count),
                "result_examined_pages": runtime.and_then(|pdf| pdf.examined_pages).or(skipped_classification.then_some(0)),
                "nominal_charged_pages": file.pdf_charged_pages.unwrap_or_default(),
                "page_audit": file.pdf_pages(),
            })),
        }));
        let extension = extensions
            .entry(file.extension.as_str())
            .or_insert_with(|| ExtensionMetric {
                extension: &file.extension,
                ..ExtensionMetric::default()
            });
        extension.file_count += 1;
        extension.parse_duration_ms += facts.parse_duration_ms;
        match file.parse_status {
            ParseStatus::Success => extension.success_count += 1,
            ParseStatus::Error => extension.error_count += 1,
            ParseStatus::Timeout => extension.timeout_count += 1,
            ParseStatus::NotParsed => {}
        }
        counts.candidates += u64::from(facts.source_guard_kind.is_some());
        counts.admitted +=
            u64::from(file.parse_status == ParseStatus::Success || facts.parse_attempted);
        counts.unavailable_guards += u64::from(file.reason == Some("source_guard_unavailable"));
        if facts.source_guard_kind == Some(SourceGuardKind::ContentSha256V1) {
            counts.guarded_files += 1;
            counts.guarded_bytes += file.size_bytes;
        }
        if let Some(pages) = file.pdf_charged_pages {
            counts.classification_slots += 1;
            counts.classification_hits += u64::from(runtime.is_some_and(|pdf| pdf.reused));
            counts.reserved_pages += pages;
            counts.extraction_slots += u64::from(file.pdf_extraction_slot == Some(true));
        }
        if let Some(runtime) = runtime {
            if runtime.attempted {
                if let Some(pages) = runtime.examined_pages {
                    counts.inspected_pages += pages;
                } else if runtime.status == ClassifyStatus::Unknown {
                    counts.unknown_classifications += 1;
                }
            }
            pdf_runtime.push(json!({
                "relative_path": file.relative_path,
                "requests": runtime.requests,
                "native_document_opens": runtime.native_document_opens,
                "native_page_reads": runtime.native_page_reads,
            }));
        }
    }

    let run_status = match scan.status {
        EngineStatus::Ok => "success",
        EngineStatus::Partial => "partial",
        EngineStatus::Error => {
            file_context.clear();
            "error"
        }
    };
    let error = (scan.status == EngineStatus::Error)
        .then(|| scan.warnings.first())
        .flatten();
    let summary = json!({
        "source_file_count": files.len(),
        "success_count": extensions.values().map(|row| row.success_count).sum::<u64>(),
        "error_file_count": extensions.values().map(|row| row.error_count).sum::<u64>(),
        "timeout_count": extensions.values().map(|row| row.timeout_count).sum::<u64>(),
        "included_file_count": included,
        "omitted_file_count": files.len() - included,
        "input_chars": input_chars,
        "output_chars": file_context.chars().count(),
        "total_duration_ms": stats.total_duration_ms,
        "discovery_duration_ms": stats.discovery_ms,
        "parse_duration_ms": stats.parse_ms,
        "compression_duration_ms": stats.allocation_ms,
    });
    let stage = |name, item_count, duration_ms| json!({"stage": name, "item_count": item_count, "duration_ms": duration_ms});
    let stage_metrics = [
        stage("discovery", files.len() as u64, stats.discovery_ms),
        stage(
            "cache",
            stats.parse_cache_lookup_count,
            stats.snapshot_lookup_ms + stats.cache_commit_ms,
        ),
        stage("parse", stats.parse_attempt_count, stats.parse_ms),
        stage("context", included as u64, stats.allocation_ms),
    ];
    let reuse_kind = if stats.snapshot_hit {
        "context_snapshot"
    } else if stats.parse_cache_all_hit == Some(true) {
        "parse_cache"
    } else {
        "none"
    };
    let deadline_exhausted = scan
        .warnings
        .iter()
        .any(|warning| warning.error_code == ErrorCode::StageDeadlineExhausted);
    let classification_all_hit = (stats.classification_cache_lookup_count != 0)
        .then_some(counts.classification_hits == stats.classification_cache_lookup_count);
    let mut execution_metrics = json!({
        "discovery_observed_file_count": files.len(),
        "source_guard_content_hash_file_count": counts.guarded_files,
        "source_guard_unavailable_count": counts.unavailable_guards,
        "source_guard_bytes_read": counts.guarded_bytes,
        "candidate_file_count": counts.candidates,
        "admitted_file_count": counts.admitted,
        "classification_slot_count": counts.classification_slots,
        "confirmed_run_inspected_pages_total": counts.inspected_pages,
        "unobserved_classification_attempt_count": counts.unknown_classifications,
        "nominal_charged_pages_total": counts.reserved_pages,
        "extraction_slot_count": counts.extraction_slots,
        "snapshot_hit": stats.snapshot_hit,
        "parse_cache_lookup_count": stats.parse_cache_lookup_count,
        "classification_cache_lookup_count": stats.classification_cache_lookup_count,
        "parse_cache_all_hit": stats.parse_cache_all_hit,
        "classification_cache_all_hit": classification_all_hit,
        "stage_deadline_exhausted_count": u64::from(deadline_exhausted),
        "classify_attempt_count": stats.classify_attempt_count,
        "parse_attempt_count": stats.parse_attempt_count,
        "reserved_chars": stats.reserved_chars,
        "rendered_chars": stats.rendered_chars,
        "discovery_ms": stats.discovery_ms,
        "snapshot_lookup_ms": stats.snapshot_lookup_ms,
    });
    if !stats.refill_rounds.is_empty() {
        execution_metrics["refill_audit"] = json!({
            "rounds": stats.refill_rounds,
            "refunded_classification_pages": stats.refunded_classification_pages,
        });
    }

    let mut sources: Vec<_> = files.iter().collect();
    sources.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    let sources: Vec<_> = sources
        .into_iter()
        .map(|file| {
            let excerpt = file.benchmark_excerpt();
            let mut limits = file.coverage_limits(profile);
            let audit = &mut limits["evidence_audit"];
            let units = excerpt
                .units
                .or_else(|| excerpt.audit?.content_units.as_deref());
            if excerpt.audit.is_some() || units.is_some() {
                audit["content_units"] = json!(units);
            }
            if excerpt.audit.is_some() {
                audit["read_ranges"] = json!(excerpt.read_ranges);
            }
            if !excerpt.omitted_ranges.is_empty() {
                audit["omitted_ranges"] = json!(excerpt.omitted_ranges);
            }
            if let Some(tag) = file.ocr_tag.as_deref() {
                audit["secondary_tags"] = json!([tag]);
            }
            audit["excerpt_ranges"] = json!(excerpt.ranges);
            let (ends, parents): (Vec<_>, Vec<_>) = file.benchmark_blocks().unzip();
            if !ends.is_empty() {
                audit["excerpt_block_ends"] = json!(ends);
                audit["excerpt_block_context"] = json!(parents);
            }
            json!({
                "source_id": file.source_id,
                "relative_path": file.relative_path,
                "extension": file.extension,
                "size_bytes": file.size_bytes,
                "modified_at_utc": file.modified_at_utc,
                "observed_at_utc": profile.observed_at_utc,
                "content_sha256": file.raw_sha256,
                "excerpt_sha256": file.excerpt_sha256,
                "excerpt": excerpt.text,
                "parser_backend": file.parser_backend,
                "worker_lane": file.worker_lane,
                "parse_status": file.parse_status,
                "parse_truncated": file.parse_truncated,
                "excerpt_truncated": excerpt.truncated,
                "coverage_limits": limits,
                "reason": file.reason,
            })
        })
        .collect();
    json!({
        "envelope": {
            "engine_build": engine_build,
            "status": scan.status,
            "file_context": file_context,
            "summary": summary,
            "error": error,
        },
        "evidence": {
            "run_status": run_status,
            "summary": summary,
            "stage_metrics": stage_metrics,
            "extension_metrics": extensions.into_values().collect::<Vec<_>>(),
            "execution_metrics": execution_metrics,
            "reuse_kind": reuse_kind,
            "files": file_rows,
            "decisions": decisions,
            "warnings": scan.warnings,
        },
        "sources": sources,
        "pdf_runtime": pdf_runtime,
        "peak_pdf_requests_in_flight": scan.peak_pdf_requests_in_flight,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_daily_report::scan::{cache::CACHE_FILENAME, ScanRequest, Scanner, ScannerConfig};
    use chrono::{NaiveDate, Utc};
    use std::fs;

    #[test]
    fn zero_pdf_classification_budget_records_zero_pages_in_raw_json() {
        let sandbox = tempfile::tempdir().unwrap();
        let root = sandbox.path();
        let input = root.join("input");
        fs::create_dir(&input).unwrap();
        fs::write(input.join("uninspected.pdf"), b"%PDF-1.7\n%%EOF\n").unwrap();
        let scanner = Scanner::open(ScannerConfig {
            work_dir: input.clone(),
            scan_db_path: root.join("cache").join(CACHE_FILENAME),
            scanner_settings: serde_json::from_value(
                json!({"max_total_pdf_classification_pages": 0}),
            )
            .unwrap(),
            pdf_worker_path: root.join("unused-worker.exe"),
        })
        .unwrap();
        let request = ScanRequest {
            start_date: NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
            end_date: NaiveDate::from_ymd_opt(2099, 12, 31).unwrap(),
            utc_window: None,
        };
        let (scan, observed) = scanner
            .build_context_for_benchmark(&request, Utc::now())
            .unwrap();
        let raw =
            serde_json::to_vec(&project(&scan, &observed, env!("AI_DAILY_REPORT_BUILD"))).unwrap();
        let result: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(
            result["evidence"]["decisions"][0]["reason"],
            "pdf_classification_page_quota_exhausted"
        );
        let classification = &result["evidence"]["files"][0]["pdf_classification"];
        assert_eq!(classification["status"], "not_classified_by_budget");
        assert_eq!(classification["result_examined_pages"], 0);
        assert_eq!(classification["nominal_charged_pages"], 0);
        assert_eq!(result["pdf_runtime"], json!([]));
    }
}
