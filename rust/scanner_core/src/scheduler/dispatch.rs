use super::cache_batch::nominal_values;
use super::*;

fn route_timeout_ms(route: RouteKind, profile: &NormalizedScannerSettings) -> u64 {
    let extension = match route {
        RouteKind::Pdf | RouteKind::PdfImageOcr { .. } => ".pdf",
        RouteKind::RustXls => ".xls",
        RouteKind::RustXlsx => ".xlsx",
        _ => return profile.execution.file_timeout_ms,
    };
    let timeout_ms = profile
        .execution
        .file_timeout_by_extension_ms
        .get(extension)
        .copied()
        .unwrap_or(profile.execution.file_timeout_ms);
    if matches!(route, RouteKind::PdfImageOcr { .. }) {
        timeout_ms.min(ai_daily_worker_contract::OCR_TIMEOUT_MS)
    } else {
        timeout_ms
    }
}

pub(crate) fn parse_worker_lane(lane: &str) -> AuditWorkerLane {
    match lane {
        "rust_core" => AuditWorkerLane::RustCore,
        "rust_office_process_v3" => AuditWorkerLane::RustOfficeProcessV3,
        "rust_pdf_process_v3" => AuditWorkerLane::RustPdfProcessV3,
        _ => AuditWorkerLane::NotParsed,
    }
}

// ---------------------------------------------------------------------------
// classification execution
// ---------------------------------------------------------------------------

impl BudgetedContextScheduler {
    /// Worker identity used for successful parse-cache provenance (spec
    /// Part 5.2). Light-text parses are local engine work; office routes use
    /// the office worker; PDF routes use the PDF worker.
    fn worker_identity_for(&self, route: RouteKind) -> (String, String, String) {
        match route {
            RouteKind::RustOffice
            | RouteKind::RustXlsx
            | RouteKind::RustXls
            | RouteKind::RustLegacyOffice => (
                self.stored_workers
                    .office_contract
                    .clone()
                    .unwrap_or_else(|| ai_daily_worker_contract::CONTRACT_VERSION.to_string()),
                self.stored_workers
                    .office_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                self.stored_workers
                    .office_build
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            RouteKind::Pdf | RouteKind::PdfImageOcr { .. } => (
                self.stored_workers
                    .pdf_contract
                    .clone()
                    .unwrap_or_else(|| ai_daily_worker_contract::CONTRACT_VERSION.to_string()),
                self.stored_workers
                    .pdf_version
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                self.stored_workers
                    .pdf_build
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

    // Inputs mirror the frozen parse phase and its cache/audit boundaries.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_parses(
        &self,
        admission: &[AdmissionDecision],
        snapshot: &HashMap<String, &DiscoveredFileOut>,
        classifications: &BTreeMap<String, PdfClassificationResult>,
        profile: &NormalizedScannerSettings,
        existed_before: &HashSet<String>,
        runtime_classification: &HashSet<String>,
        metrics: &mut ExecutionMetrics,
        executor: &RollingExecutor,
    ) -> Result<ParseOutputs, SchedulerFailure> {
        let mut results = HashMap::new();
        let mut runtime_not_parsed = HashSet::new();
        let classification_cache_receipts = Vec::new();
        let mut parse_profile_hashes = HashMap::new();
        let mut parse_cache_status = HashMap::new();
        let mut parse_cache_miss_reason = HashMap::new();
        let mut parse_warnings = Vec::new();
        let mut parse_fresh_hits = Vec::new();
        let mut any_lookup = false;
        let mut any_miss = false;
        let work_deadline = self.stored_work_deadline;

        // ---- Phase A (main thread, deterministic): cache lookup + task build in
        // nominal rank order. Only the actual parser invocations are
        // parallelized in Phase B. ----
        let mut admitted: Vec<ParseTask> = Vec::new();
        for decision in admission {
            if let PlanAction::Admit { route } = &decision.action {
                let Some(file) = snapshot.get(&decision.file_identity) else {
                    continue;
                };
                if runtime_classification.contains(&decision.file_identity) {
                    continue;
                }
                // spec Part 1.2/3.2: a no-text PDF admitted as a metadata-only
                // draft never starts a body parser.
                if *route == RouteKind::Pdf
                    && classifications
                        .get(&decision.file_identity)
                        .map(|r| r.status)
                        == Some(PdfClassificationStatus::NoTextInParseWindow)
                {
                    continue;
                }
                if self.clock.now_ms() >= work_deadline {
                    runtime_not_parsed.insert(decision.file_identity.clone());
                    metrics.stage_deadline_exhausted_count = 1;
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
                        parse_profile_hashes.insert(decision.file_identity.clone(), profile_hash);
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
                                evidence_audit: entry.evidence_audit,
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
                                parse_transport: ParseTransport::NotApplicable,
                                parse_attempt_count: 0,
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
                        parse_cache_miss_reason.insert(decision.file_identity.clone(), reason);
                        if self.clock.now_ms() >= work_deadline {
                            runtime_not_parsed.insert(decision.file_identity.clone());
                            metrics.stage_deadline_exhausted_count = 1;
                            continue;
                        }
                        let Some(guard) = expected_guard(file) else {
                            continue;
                        };
                        admitted.push(ParseTask {
                            file: (*file).clone(),
                            route: *route,
                            guard,
                            own_timeout_ms: route_timeout_ms(*route, profile),
                            profile_hash,
                        });
                    }
                }
            }
        }

        let mut cache_batch = CacheBatch::new(self);
        executor.run(
            &admitted,
            self.clock.as_ref(),
            work_deadline,
            |task| match task.route {
                RouteKind::LightText => WorkLane::Inline,
                RouteKind::Pdf | RouteKind::PdfImageOcr { .. } => WorkLane::Pdf,
                RouteKind::RustOffice
                | RouteKind::RustXls
                | RouteKind::RustXlsx
                | RouteKind::RustLegacyOffice => WorkLane::Office,
            },
            |task| {
                if !self.guard.verify(&task.file.path, &task.guard) {
                    return Some(source_version_changed_parse(&task.file));
                }
                let mut request = crate::parsers::scheduled_request(
                    &task.file,
                    task.route,
                    task.own_timeout_ms,
                    profile,
                    classifications,
                );
                let remaining = work_deadline.saturating_sub(self.clock.now_ms());
                if remaining == 0 {
                    return None;
                }
                request.timeout_ms = request.timeout_ms.min(remaining);
                Some(self.parser.parse(&request))
            },
            |event| {
                let DispatchEvent::Completed(index, result) = event else {
                    cache_batch.flush();
                    return None;
                };
                let task = &admitted[index];
                let file = &task.file;
                let Some(mut result) = result else {
                    runtime_not_parsed.insert(file.file_identity.clone());
                    metrics.stage_deadline_exhausted_count = 1;
                    return cache_batch.flush_at();
                };
                metrics.parse_attempt_count = metrics
                    .parse_attempt_count
                    .saturating_add(result.parse_attempt_count);
                if matches!(task.route, RouteKind::Pdf | RouteKind::PdfImageOcr { .. }) {
                    metrics.pdfplumber_invocations = metrics
                        .pdfplumber_invocations
                        .saturating_add(result.parse_attempt_count);
                }
                parse_warnings.extend(
                    result
                        .warnings
                        .iter()
                        .cloned()
                        .map(|warning| (index, warning)),
                );
                if result.parse_status == ParseStatus::Success {
                    if !self.guard.verify(&file.path, &task.guard) {
                        result = source_version_changed_after_parse(file, result);
                    } else if self.clock.now_ms() < work_deadline {
                        let (worker_contract, worker_version, worker_build) =
                            self.worker_identity_for(task.route);
                        cache_batch.push(
                            index,
                            &file.path,
                            task.route.backend(),
                            CacheWriteRecord {
                                file_identity: file.file_identity.clone(),
                                source_version: file.source_version.clone(),
                                source_guard_kind: crate::source_guard::source_guard_kind_text(
                                    task.guard.kind,
                                )
                                .to_string(),
                                source_guard_sha256: task
                                    .guard
                                    .guard_sha256
                                    .clone()
                                    .unwrap_or_default(),
                                parse_profile_hash: task.profile_hash.clone(),
                                content: result.content.clone(),
                                content_sha256: result.content_sha256.clone(),
                                evidence_audit: result.evidence_audit.clone(),
                                cache_payload_version:
                                    crate::store::cache::PARSE_CACHE_PAYLOAD_VERSION,
                                parser_backend: result.parser_backend.clone(),
                                worker_lane: result.worker_lane.clone(),
                                truncated: result.truncated,
                                worker_contract_version: worker_contract,
                                worker_version,
                                worker_build,
                            },
                        );
                    }
                }
                results.insert(file.file_identity.clone(), result);
                parse_profile_hashes.insert(file.file_identity.clone(), task.profile_hash.clone());
                cache_batch.flush_at()
            },
        )?;
        let (parse_cache_receipts, cache_write_warnings) = cache_batch.finish(metrics);
        if any_lookup {
            metrics.parse_cache_all_hit =
                Some(metrics.parse_cache_all_hit.unwrap_or(true) && !any_miss);
        }
        Ok(ParseOutputs {
            results,
            runtime_not_parsed,
            parse_cache_receipts,
            classification_cache_receipts,
            parse_profile_hashes,
            parse_cache_status,
            parse_cache_miss_reason,
            cache_write_warnings,
            parse_warnings: nominal_values(parse_warnings),
            parse_fresh_hits,
        })
    }
}
