use super::*;

pub(super) fn classifier_error_line(
    plan: &ClassifiedPlan,
    classifications: &BTreeMap<String, crate::admission::PdfClassificationResult>,
    runtime: &HashSet<String>,
) -> Option<String> {
    if !matches!(
        plan.pdf_classification,
        crate::admission::PdfClassificationPlan::Classify { .. }
    ) {
        return None;
    }
    if runtime.contains(&plan.file_identity) {
        return None;
    }
    let result = classifications.get(&plan.file_identity);
    let status = result.map(|r| r.status);
    match status {
        Some(PdfClassificationStatus::Unknown) => {
            let code = if result.and_then(|r| r.error_code.as_deref()) == Some("PARSER_TIMEOUT") {
                "PARSER_TIMEOUT"
            } else {
                "PARSER_FAILED"
            };
            Some(format!(
                "- {} | reason=parse_error | error={code}",
                plan.relative_path
            ))
        }
        Some(PdfClassificationStatus::Error) => {
            let code = if result.and_then(|r| r.error_code.as_deref()) == Some("PDF_PROTECTED") {
                "PDF_PROTECTED"
            } else {
                "PARSER_FAILED"
            };
            Some(format!(
                "- {} | reason=parse_error | error={code}",
                plan.relative_path
            ))
        }
        _ => None,
    }
}

fn classification_cache_record(
    file: &DiscoveredFileOut,
    classifier_profile_hash: &str,
    classifier_build: &str,
    status: &PdfClassificationStatus,
    page_count: Option<u64>,
    result_examined_pages: Option<u64>,
) -> Option<ClassificationCacheWriteRecord> {
    let kind = file.source_guard_kind.as_deref()?;
    let hash = file.source_guard_sha256.clone()?;
    let status_text = match status {
        PdfClassificationStatus::TextInParseWindow => "text_in_parse_window",
        PdfClassificationStatus::NoTextInParseWindow => "no_text_in_parse_window",
        _ => return None,
    };
    // spec Part 3.2: text is `1..window_pages`, no-text MUST equal
    // `window_pages`; the typed result's real counts are preserved verbatim.
    let pages = page_count?;
    let examined = result_examined_pages?;
    if pages == 0 || examined == 0 {
        return None;
    }
    Some(ClassificationCacheWriteRecord {
        file_identity: file.file_identity.clone(),
        source_version: file.source_version.clone(),
        source_guard_kind: kind.to_string(),
        source_guard_sha256: hash,
        classifier_profile_hash: classifier_profile_hash.to_string(),
        classifier_build: classifier_build.to_string(),
        status: status_text.to_string(),
        page_count: pages,
        result_examined_pages: examined,
    })
}

type ClassificationOutputs = (
    BTreeMap<String, PdfClassificationResult>,
    BTreeMap<String, PdfClassificationAuditV1>,
    HashSet<String>,
    Vec<String>,
    Vec<ClassificationCacheWriteRecord>,
    Vec<Diagnostic>,
);

impl BudgetedContextScheduler {
    // Inputs mirror the frozen classification phase and its audit accumulators.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_classifications(
        &self,
        classified: &[ClassifiedPlan],
        snapshot: &HashMap<String, &DiscoveredFileOut>,
        profile: &NormalizedScannerSettings,
        classifier_profile_hash: &str,
        classifier_build: &str,
        existed_before: &HashSet<String>,
        metrics: &mut ExecutionMetrics,
        executor: &RollingExecutor,
    ) -> Result<ClassificationOutputs, SchedulerFailure> {
        let work_deadline = self.stored_work_deadline;
        let mut results = BTreeMap::new();
        let mut audits = BTreeMap::new();
        let mut runtime_not_parsed = HashSet::new();
        let mut classification_fresh_hits = Vec::new();
        let mut any_lookup = false;
        let mut any_miss = false;

        // ---- Phase A (main thread, deterministic): freeze the task set in
        // nominal rank order. Cache lookups and the pre-verify happen here; only
        // the actual classifier invocations are parallelized in Phase B. ----
        let mut tasks: Vec<ClassificationTask> = Vec::new();
        let settings_hash = crate::store::sha256_hex(
            &serde_json::to_vec(profile)
                .map_err(|error| SchedulerFailure::internal(&error.to_string()))?,
        );
        for plan in classified {
            let nominal_charged_pages = match plan.pdf_classification {
                crate::admission::PdfClassificationPlan::Classify { charged_pages } => {
                    charged_pages
                }
                crate::admission::PdfClassificationPlan::NotClassifiedByBudget => {
                    results.insert(
                        plan.file_identity.clone(),
                        PdfClassificationResult {
                            file_identity: plan.file_identity.clone(),
                            status: PdfClassificationStatus::NotClassifiedByBudget,
                            page_count: None,
                            result_examined_pages: Some(0),
                            error_code: None,
                        },
                    );
                    audits.insert(
                        plan.file_identity.clone(),
                        PdfClassificationAuditV1 {
                            pdf_open_failure_key: None,
                            page_audit: None,
                            status: PdfClassificationStatus::NotClassifiedByBudget,
                            page_count: Nullable(None),
                            classification_cache_status: ClassificationCacheStatus::NotEligible,
                            classification_cache_miss_reason: String::new(),
                            result_examined_pages: Nullable(Some(0)),
                            run_inspected_pages: Nullable(Some(0)),
                            nominal_charged_pages: 0,
                            duration_ms: 0,
                            transport: ClassificationTransport::NotApplicable,
                            attempt_count: 0,
                            classifier_build: classifier_build.to_string(),
                            classifier_profile_hash: classifier_profile_hash.to_string(),
                        },
                    );
                    continue;
                }
                crate::admission::PdfClassificationPlan::NotPdf => continue,
            };
            // A shortened supplemental window has a distinct cache identity.
            let supplemental_hash = if nominal_charged_pages != profile.parse.pdf.max_pages {
                let mut bounded = profile.clone();
                bounded.parse.pdf.max_pages = nominal_charged_pages;
                Some(
                    crate::store::classifier_profile_hash(&bounded)
                        .map_err(|message| SchedulerFailure::internal(&message))?,
                )
            } else {
                None
            };
            let classifier_profile_hash = supplemental_hash
                .as_deref()
                .unwrap_or(classifier_profile_hash);
            let Some(file) = snapshot.get(&plan.file_identity) else {
                continue;
            };
            if self.clock.now_ms() >= work_deadline {
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
                    source_version_changed_classification(&plan.file_identity),
                );
                continue;
            }
            metrics.classification_cache_lookup_count += 1;
            any_lookup = true;
            let existed = existed_before.contains(&plan.file_identity);
            let key_fields = (
                &file.file_identity,
                &file.source_version,
                &file.source_guard_kind,
                &file.source_guard_sha256,
                &settings_hash,
                classifier_profile_hash,
                classifier_build,
            );
            let pdf_open_failure_key = crate::store::cache::domain_hash(
                b"pdf-open-failure-v1\0",
                &serde_json::to_vec(&key_fields)
                    .map_err(|error| SchedulerFailure::internal(&error.to_string()))?,
            );
            let lookup = self
                .cache
                .lookup_classification(
                    file,
                    classifier_profile_hash,
                    classifier_build,
                    &pdf_open_failure_key,
                    existed,
                )
                // spec Part 4: a cache lookup failure is run-level corruption /
                // store error, NEVER a per-file miss disposition.
                .map_err(|error| SchedulerFailure {
                    diagnostic: error.diagnostic(DiagnosticStage::Cache),
                })?;
            let (status, page_count, examined, page_audit, failure) = match lookup {
                ClassificationCacheLookup::Fresh(entry) => {
                    classification_fresh_hits.push(plan.file_identity.clone());
                    let status = match entry.status.as_str() {
                        "text_in_parse_window" => PdfClassificationStatus::TextInParseWindow,
                        _ => PdfClassificationStatus::NoTextInParseWindow,
                    };
                    let audit = entry.page_audit.unwrap_or_else(|| {
                        let pages = ai_daily_scanner_contract::pdf_sample_pages(
                            entry.page_count,
                            nominal_charged_pages,
                            &profile.classifier_policy_version,
                        );
                        ai_daily_scanner_contract::PdfPageAudit {
                            policy_version: profile.classifier_policy_version.clone(),
                            checked_pages: pages
                                .into_iter()
                                .take(entry.result_examined_pages as usize)
                                .collect(),
                            observations: None,
                        }
                    });
                    (
                        status,
                        Some(entry.page_count),
                        Some(entry.result_examined_pages),
                        Some(audit),
                        None,
                    )
                }
                ClassificationCacheLookup::PdfOpenError(failure) => {
                    if !self.guard.verify(&file.path, &guard) {
                        results.insert(
                            plan.file_identity.clone(),
                            source_version_changed_classification(&plan.file_identity),
                        );
                        continue;
                    }
                    (
                        PdfClassificationStatus::Error,
                        None,
                        None,
                        None,
                        Some(failure),
                    )
                }
                ClassificationCacheLookup::Miss(reason) => {
                    any_miss = true;
                    let remaining = work_deadline.saturating_sub(self.clock.now_ms());
                    if remaining == 0 {
                        runtime_not_parsed.insert(plan.file_identity.clone());
                        metrics.stage_deadline_exhausted_count = 1;
                        continue;
                    }
                    tasks.push(ClassificationTask {
                        file: (*file).clone(),
                        guard,
                        request: crate::parsers::classifier::ClassifyOperation {
                            request_id: next_request_id(),
                            payload: ai_daily_worker_contract::ClassifyRequest {
                                file_path: file.path.clone(),
                                source_version: file.source_version.clone(),
                                max_pages: nominal_charged_pages,
                                policy_version: profile.classifier_policy_version.clone(),
                            },
                        },
                        own_timeout_ms: profile.pdf_classification_timeout_ms,
                        classifier_profile_hash: classifier_profile_hash.to_string(),
                        pdf_open_failure_key,
                        classifier_build: classifier_build.to_string(),
                        cache_miss_reason: reason.as_str().to_string(),
                        nominal_charged_pages,
                    });
                    continue;
                }
            };
            results.insert(
                plan.file_identity.clone(),
                PdfClassificationResult {
                    file_identity: plan.file_identity.clone(),
                    status,
                    page_count,
                    result_examined_pages: examined,
                    error_code: failure.map(|cause| cause.error_code().to_string()),
                },
            );
            audits.insert(
                plan.file_identity.clone(),
                PdfClassificationAuditV1 {
                    pdf_open_failure_key: failure.map(|_| pdf_open_failure_key),
                    page_audit,
                    status,
                    page_count: Nullable(page_count),
                    classification_cache_status: ClassificationCacheStatus::Fresh,
                    classification_cache_miss_reason: String::new(),
                    result_examined_pages: Nullable(examined),
                    run_inspected_pages: Nullable(Some(0)),
                    nominal_charged_pages,
                    duration_ms: 0,
                    transport: ClassificationTransport::NotApplicable,
                    attempt_count: 0,
                    classifier_build: classifier_build.to_string(),
                    classifier_profile_hash: classifier_profile_hash.to_string(),
                },
            );
        }

        let mut cache_batch = CacheBatch::new(self);
        executor.run(
            &tasks,
            self.clock.as_ref(),
            work_deadline,
            |_| WorkLane::Pdf,
            |task| {
                let remaining = work_deadline.saturating_sub(self.clock.now_ms());
                (remaining > 0).then(|| {
                    let timeout = Duration::from_millis(task.own_timeout_ms.min(remaining));
                    self.classifier.classify_pdf(&task.request, timeout)
                })
            },
            |event| {
                match event {
                    DispatchEvent::Completed(index, execution) => self.merge_classification(
                        index,
                        &tasks[index],
                        execution,
                        &mut results,
                        &mut audits,
                        &mut runtime_not_parsed,
                        metrics,
                        &mut cache_batch,
                    ),
                    DispatchEvent::FlushCache => cache_batch.flush(),
                }
                cache_batch.flush_at()
            },
        )?;
        let (classification_cache_receipts, classification_cache_write_warnings) =
            cache_batch.finish(metrics);

        if any_lookup {
            metrics.classification_cache_all_hit =
                Some(metrics.classification_cache_all_hit.unwrap_or(true) && !any_miss);
        }
        Ok((
            results,
            audits,
            runtime_not_parsed,
            classification_fresh_hits,
            classification_cache_receipts,
            classification_cache_write_warnings,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn merge_classification<'a>(
        &self,
        index: usize,
        task: &'a ClassificationTask,
        execution: Option<crate::parsers::classifier::PdfClassifierExecution>,
        results: &mut BTreeMap<String, PdfClassificationResult>,
        audits: &mut BTreeMap<String, PdfClassificationAuditV1>,
        runtime_not_parsed: &mut HashSet<String>,
        metrics: &mut ExecutionMetrics,
        cache_batch: &mut CacheBatch<'a, ClassificationCacheWriteRecord>,
    ) {
        let identity = &task.file.file_identity;
        let Some(execution) = execution else {
            runtime_not_parsed.insert(identity.clone());
            metrics.stage_deadline_exhausted_count = 1;
            return;
        };
        metrics.classify_attempt_count = metrics
            .classify_attempt_count
            .saturating_add(execution.attempt_count);
        // spec Part 3.2: carry the typed result's REAL page counts
        // (page_count / result_examined_pages), and distinguish a classifier
        // per-file timeout (-> Timeout) from crash / transient I/O /
        // protocol failure (-> Error, retryable=true).
        let observed_pages = execution
            .outcome
            .as_ref()
            .ok()
            .and_then(|result| result.page_audit.clone());
        let open_failure = execution.outcome.as_ref().ok().and_then(|result| {
            crate::parsers::classifier::PdfOpenFailure::confirmed(result, &task.file.path)
        });
        let classification = match execution.outcome {
            Ok(result) => {
                let status = match result.status {
                    ai_daily_worker_contract::ClassifyStatus::TextInParseWindow => {
                        PdfClassificationStatus::TextInParseWindow
                    }
                    ai_daily_worker_contract::ClassifyStatus::NoTextInParseWindow => {
                        PdfClassificationStatus::NoTextInParseWindow
                    }
                    ai_daily_worker_contract::ClassifyStatus::Unknown => {
                        PdfClassificationStatus::Unknown
                    }
                    ai_daily_worker_contract::ClassifyStatus::Error => {
                        PdfClassificationStatus::Error
                    }
                };
                let error_code = result.diagnostic.as_ref().map(|diagnostic| {
                    match diagnostic.error_code.as_str() {
                        "SOURCE_VERSION_CHANGED" => "SOURCE_VERSION_CHANGED".to_string(),
                        "PARSER_TIMEOUT" => "PARSER_TIMEOUT".to_string(),
                        "PDF_PROTECTED" => "PDF_PROTECTED".to_string(),
                        _ => "PARSER_FAILED".to_string(),
                    }
                });
                PdfClassificationResult {
                    file_identity: identity.clone(),
                    status,
                    page_count: result.page_count,
                    result_examined_pages: result.result_examined_pages,
                    error_code,
                }
            }
            Err(failure) => {
                let error_code = match failure.diagnostic.error_code {
                    ErrorCode::SourceVersionChanged => "SOURCE_VERSION_CHANGED",
                    ErrorCode::ParserTimeout => "PARSER_TIMEOUT",
                    _ => "PARSER_FAILED",
                };
                PdfClassificationResult {
                    file_identity: identity.clone(),
                    status: PdfClassificationStatus::Unknown,
                    page_count: None,
                    result_examined_pages: None,
                    error_code: Some(error_code.to_string()),
                }
            }
        };
        if classification.error_code.as_deref() == Some("SOURCE_VERSION_CHANGED") {
            results.insert(
                identity.clone(),
                source_version_changed_classification(identity),
            );
            metrics.unobserved_classification_attempt_count = metrics
                .unobserved_classification_attempt_count
                .saturating_add(execution.attempt_count);
            return;
        }
        // A pre-dispatch validation/start failure did not run a classifier.
        // Keep the final file diagnostic, but do not fabricate the strict
        // PdfClassificationAuditV1 miss shape (which requires 1..3 attempts).
        if execution.attempt_count == 0 {
            results.insert(identity.clone(), classification);
            return;
        }
        if !self.guard.verify(&task.file.path, &task.guard) {
            results.insert(
                identity.clone(),
                source_version_changed_classification(identity),
            );
            // spec Part 5.3: a discarded classifier attempt has no confirmed
            // inspected pages.
            metrics.unobserved_classification_attempt_count = metrics
                .unobserved_classification_attempt_count
                .saturating_add(execution.attempt_count);
            return;
        }
        let status = classification.status;
        let page_count = classification.page_count;
        let result_examined_pages = classification.result_examined_pages;
        // spec Part 5.3: sum confirmed run-inspected pages; any attempt that
        // cannot report pages is unobserved.
        let run_inspected_pages = if execution.attempt_count == 1 {
            result_examined_pages
        } else {
            None
        };
        match run_inspected_pages {
            Some(pages) => {
                metrics.confirmed_run_inspected_pages_total = metrics
                    .confirmed_run_inspected_pages_total
                    .saturating_add(pages)
            }
            None => {
                let unobserved_attempts = if result_examined_pages.is_some() {
                    execution.attempt_count.saturating_sub(1)
                } else {
                    execution.attempt_count
                };
                metrics.unobserved_classification_attempt_count = metrics
                    .unobserved_classification_attempt_count
                    .saturating_add(unobserved_attempts)
            }
        }
        audits.insert(
            identity.clone(),
            PdfClassificationAuditV1 {
                pdf_open_failure_key: open_failure.map(|_| task.pdf_open_failure_key.clone()),
                page_audit: observed_pages,
                status,
                page_count: Nullable(page_count),
                classification_cache_status: ClassificationCacheStatus::Miss,
                classification_cache_miss_reason: task.cache_miss_reason.clone(),
                result_examined_pages: Nullable(result_examined_pages),
                run_inspected_pages: Nullable(run_inspected_pages),
                nominal_charged_pages: task.nominal_charged_pages,
                duration_ms: execution.duration_ms,
                transport: execution.transport,
                attempt_count: execution.attempt_count,
                classifier_build: task.classifier_build.clone(),
                classifier_profile_hash: task.classifier_profile_hash.clone(),
            },
        );
        results.insert(identity.clone(), classification);
        if self.clock.now_ms() < self.stored_work_deadline {
            if let Some(record) = classification_cache_record(
                &task.file,
                &task.classifier_profile_hash,
                &task.classifier_build,
                &status,
                page_count,
                result_examined_pages,
            ) {
                cache_batch.push(index, &task.file.path, "rust_pdfium_v3", record);
            }
        }
    }
}
