//! Confirmed quota reclamation. Every supplemental wave is frozen before I/O.
use super::*;
use crate::admission::{NotParsedReason, PdfClassificationPlan};
use crate::budget_model::SECTION_SEPARATOR_CHARS;
use ai_daily_scanner_contract::{RefillAudit, RefillRoundAudit, Validate};

fn budget_pending(action: &PlanAction) -> bool {
    matches!(
        action,
        PlanAction::NotParsed {
            reason: NotParsedReason::SemanticFileQuotaExhausted
                | NotParsedReason::PdfClassificationPageQuotaExhausted
                | NotParsedReason::PdfTextExtractionQuotaExhausted
                | NotParsedReason::GlobalContextBudgetExceeded
        }
    )
}

fn consumed(
    classified: &[ClassifiedPlan],
    admission: &[AdmissionDecision],
    classifications: &BTreeMap<String, PdfClassificationResult>,
    audits: &BTreeMap<String, PdfClassificationAuditV1>,
    outputs: &ParseOutputs,
) -> Result<(u64, u64, u64, u64), SchedulerFailure> {
    let mut candidates: HashSet<&String> = outputs.results.keys().collect();
    candidates.extend(classifications.iter().filter_map(|(id, result)| {
        (result.status != PdfClassificationStatus::NotClassifiedByBudget).then_some(id)
    }));
    let mut held_pages = 0_u64;
    let mut refunded_pages = 0_u64;
    for plan in classified {
        if let PdfClassificationPlan::Classify { charged_pages } = plan.pdf_classification {
            // Failures and unknown attempts retain their complete reservation.
            let known = audits
                .get(&plan.file_identity)
                .filter(|audit| {
                    matches!(
                        audit.status,
                        PdfClassificationStatus::TextInParseWindow
                            | PdfClassificationStatus::NoTextInParseWindow
                    )
                })
                .and_then(|audit| audit.run_inspected_pages.0);
            let held = known.unwrap_or(charged_pages);
            if held > charged_pages {
                return Err(SchedulerFailure::internal(
                    "classifier consumed more pages than its frozen reservation",
                ));
            }
            held_pages = held_pages.saturating_add(held);
            refunded_pages = refunded_pages.saturating_add(charged_pages - held);
        }
    }
    let extractions = admission
        .iter()
        .filter(|decision| {
            if !matches!(
                decision.action,
                PlanAction::Admit {
                    route: RouteKind::Pdf
                }
            ) || classifications
                .get(&decision.file_identity)
                .map(|result| result.status)
                != Some(PdfClassificationStatus::TextInParseWindow)
            {
                return false;
            }
            // A verified successful cache hit started no extraction. Missing or failed
            // execution evidence keeps the slot; a later round cannot retry that file.
            !outputs
                .results
                .get(&decision.file_identity)
                .is_some_and(|result| {
                    result.parse_status == ParseStatus::Success
                        && result.parse_attempt_count == 0
                        && result.parse_transport == ParseTransport::NotApplicable
                })
        })
        .count() as u64;
    Ok((
        candidates.len() as u64,
        held_pages,
        extractions,
        refunded_pages,
    ))
}

fn merge_parses(current: &mut ParseOutputs, next: ParseOutputs) {
    current.results.extend(next.results);
    current.runtime_not_parsed.extend(next.runtime_not_parsed);
    current
        .parse_cache_receipts
        .extend(next.parse_cache_receipts);
    current
        .classification_cache_receipts
        .extend(next.classification_cache_receipts);
    current
        .parse_profile_hashes
        .extend(next.parse_profile_hashes);
    current.parse_cache_status.extend(next.parse_cache_status);
    current
        .parse_cache_miss_reason
        .extend(next.parse_cache_miss_reason);
    current
        .cache_write_warnings
        .extend(next.cache_write_warnings);
    current.parse_warnings.extend(next.parse_warnings);
    current.parse_fresh_hits.extend(next.parse_fresh_hits);
}

impl BudgetedContextScheduler {
    #[allow(clippy::too_many_arguments)]
    fn reclaim_chars(
        &self,
        discovery: &[DiscoveredFileOut],
        work_dir: &str,
        profile: &NormalizedScannerSettings,
        classified: &[ClassifiedPlan],
        admission: &mut [AdmissionDecision],
        classifications: &BTreeMap<String, PdfClassificationResult>,
        runtime: &HashSet<String>,
        outputs: &ParseOutputs,
        model: &ContextBudgetModel,
    ) -> Result<u64, SchedulerFailure> {
        let evidence = build_evidence(
            discovery,
            work_dir,
            classified,
            admission,
            classifications,
            runtime,
            outputs,
        )?;
        let rendered =
            build_context(evidence, &profile.context, profile.report_mode).map_err(|message| {
                SchedulerFailure::new(
                    ErrorCode::BudgetModelMismatch,
                    message,
                    false,
                    DiagnosticStage::Context,
                )
            })?;
        enforce_rendered_within_reserved(&rendered, admission, model)?;
        let mut refund = 0_u64;
        for decision in admission {
            if !matches!(decision.action, PlanAction::Admit { .. }) {
                continue;
            }
            let completed = outputs
                .results
                .get(&decision.file_identity)
                .is_some_and(|result| result.parse_status == ParseStatus::Success)
                || classifications
                    .get(&decision.file_identity)
                    .is_some_and(|result| {
                        result.status == PdfClassificationStatus::NoTextInParseWindow
                    });
            if !completed
                || runtime.contains(&decision.file_identity)
                || outputs.runtime_not_parsed.contains(&decision.file_identity)
            {
                continue;
            }
            if let Some(chars) = rendered.rendered_by_identity.get(&decision.file_identity) {
                let retained = chars
                    .saturating_add(SECTION_SEPARATOR_CHARS)
                    .min(decision.reserved_chars);
                refund = refund.saturating_add(decision.reserved_chars - retained);
                decision.reserved_chars = retained;
            }
        }
        Ok(refund)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_refills(
        &self,
        discovery: &[DiscoveredFileOut],
        work_dir: &str,
        profile: &NormalizedScannerSettings,
        snapshot: &HashMap<String, &DiscoveredFileOut>,
        existed: &HashSet<String>,
        classified: &mut [ClassifiedPlan],
        admission: &mut Vec<AdmissionDecision>,
        classifications: &mut BTreeMap<String, PdfClassificationResult>,
        audits: &mut BTreeMap<String, PdfClassificationAuditV1>,
        runtime: &mut HashSet<String>,
        fresh_hits: &mut Vec<String>,
        outputs: &mut ParseOutputs,
        model: &mut ContextBudgetModel,
        classifier_hash: &str,
        classifier_build: &str,
        metrics: &mut ExecutionMetrics,
        executor: &WaveExecutor,
    ) -> Result<(), SchedulerFailure> {
        let mut ledger = RefillAudit {
            policy_version: "budget_refill_v3".into(),
            stop_reason: "round_limit".into(),
            max_rounds: 2,
            candidate_cap: profile.max_candidate_files,
            classification_page_cap: profile.max_total_pdf_classification_pages,
            extraction_cap: profile.max_pdf_text_extractions,
            context_char_cap: profile.context.global_max_chars,
            candidate_slots_used: 0,
            classification_pages_held: 0,
            extraction_slots_used: 0,
            refunded_chars: 0,
            refunded_classification_pages: 0,
            rounds: Vec::new(),
        };
        for round in 1..=2 {
            if self.clock.now_ms() >= self.stored_work_deadline
                || metrics.stage_deadline_exhausted_count > 0
            {
                ledger.stop_reason = "deadline".into();
                break;
            }
            ledger.refunded_chars += self.reclaim_chars(
                discovery,
                work_dir,
                profile,
                classified,
                admission,
                classifications,
                runtime,
                outputs,
                model,
            )?;
            let (used_candidates, held_pages, used_extractions, _) =
                consumed(classified, admission, classifications, audits, outputs)?;
            let decisions: HashMap<&str, &AdmissionDecision> = admission
                .iter()
                .map(|decision| (decision.file_identity.as_str(), decision))
                .collect();
            let mut unstarted = Vec::new();
            let mut reusable = Vec::new();
            for plan in classified.iter() {
                let id = &plan.file_identity;
                if runtime.contains(id)
                    || outputs.runtime_not_parsed.contains(id)
                    || outputs.results.contains_key(id)
                {
                    continue;
                }
                let action = decisions
                    .get(id.as_str())
                    .map(|decision| &decision.action)
                    .unwrap_or(&plan.action);
                if !budget_pending(action) {
                    continue;
                }
                if let Some(result) = classifications.get(id) {
                    if matches!(
                        result.status,
                        PdfClassificationStatus::TextInParseWindow
                            | PdfClassificationStatus::NoTextInParseWindow
                    ) {
                        reusable.push(plan.clone());
                        continue;
                    }
                    if result.status != PdfClassificationStatus::NotClassifiedByBudget {
                        continue;
                    }
                }
                if let Some(file) = snapshot.get(id) {
                    unstarted.push(plan_candidate(file));
                }
            }
            let has_pending = !unstarted.is_empty() || !reusable.is_empty();
            let mut frozen = ClassificationPlan::build_with_remaining(
                unstarted,
                profile,
                profile
                    .max_total_pdf_classification_pages
                    .saturating_sub(held_pages),
                profile.max_candidate_files.saturating_sub(used_candidates),
                true,
            );
            let retained: u64 = admission
                .iter()
                .map(|decision| decision.reserved_chars)
                .sum();
            let reusable_decisions = ContentAdmissionPlan::build_with_remaining(
                &reusable,
                profile,
                model,
                classifications,
                retained,
                profile
                    .max_pdf_text_extractions
                    .saturating_sub(used_extractions),
            );
            let reusable_ids: HashSet<_> = reusable_decisions
                .iter()
                .filter(|decision| matches!(decision.action, PlanAction::Admit { .. }))
                .map(|decision| &decision.file_identity)
                .collect();
            reusable.retain(|plan| reusable_ids.contains(&plan.file_identity));
            // Only newly admitted PDFs need classification; other dispositions
            // are retained as budget evidence and can be considered in the next wave.
            let classify_now: Vec<_> = frozen
                .iter()
                .filter(|plan| {
                    matches!(
                        plan.pdf_classification,
                        PdfClassificationPlan::Classify { .. }
                    )
                })
                .cloned()
                .collect();
            let planned_count = frozen
                .iter()
                .filter(|plan| matches!(plan.action, PlanAction::Admit { .. }))
                .count()
                + reusable.len();
            if planned_count == 0 {
                ledger.stop_reason = if has_pending {
                    "quota_exhausted"
                } else {
                    "complete"
                }
                .into();
                break;
            }
            let frozen_identity = serde_json::to_vec(
                &frozen
                    .iter()
                    .chain(reusable.iter())
                    .map(|plan| {
                        (
                            &plan.file_identity,
                            format!("{:?}", plan.action),
                            format!("{:?}", plan.pdf_classification),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .map_err(|_| SchedulerFailure::internal("refill plan encoding failed"))?;
            let before_results = outputs.results.len();
            let before_classifications = classifications
                .values()
                .filter(|result| result.status != PdfClassificationStatus::NotClassifiedByBudget)
                .count();
            let (results, next_audits, next_runtime, hits, receipts, warnings) = self
                .run_classifications(
                    &classify_now,
                    snapshot,
                    profile,
                    classifier_hash,
                    classifier_build,
                    existed,
                    metrics,
                    executor,
                )?;
            classifications.extend(results);
            audits.extend(next_audits);
            runtime.extend(next_runtime);
            fresh_hits.extend(hits);
            outputs.classification_cache_receipts.extend(receipts);
            outputs.cache_write_warnings.extend(warnings);
            let mut fixed = fixed_context_sections(
                &profile.context,
                profile.report_mode,
                discovery.len() as u64,
            );
            let frozen_ids: HashSet<_> = frozen.iter().map(|plan| &plan.file_identity).collect();
            for plan in classified
                .iter()
                .filter(|plan| !frozen_ids.contains(&plan.file_identity))
                .chain(frozen.iter())
            {
                if let Some(line) = classifier_error_line(plan, classifications, runtime) {
                    fixed.push(line);
                }
            }
            *model = ContextBudgetModel::new(&profile.context, &fixed).map_err(|error| {
                SchedulerFailure::new(
                    ErrorCode::ContextFixedSectionsOverBudget,
                    error.to_string(),
                    false,
                    DiagnosticStage::Context,
                )
            })?;
            frozen.append(&mut reusable);
            frozen.sort_by_key(|plan| {
                crate::nominal::NominalKey::new(
                    &plan.relative_path,
                    &plan.extension,
                    &plan.file_identity,
                )
            });
            let retained = admission
                .iter()
                .map(|decision| decision.reserved_chars)
                .sum();
            let next_admission = ContentAdmissionPlan::build_with_remaining(
                &frozen,
                profile,
                model,
                classifications,
                retained,
                profile
                    .max_pdf_text_extractions
                    .saturating_sub(used_extractions),
            );
            let next = self.run_parses(
                &next_admission,
                snapshot,
                classifications,
                profile,
                existed,
                runtime,
                metrics,
                executor,
            )?;
            merge_parses(outputs, next);
            let mut new_plans: HashMap<_, _> = frozen
                .into_iter()
                .map(|plan| (plan.file_identity.clone(), plan))
                .collect();
            for old in classified.iter_mut() {
                if let Some(plan) = new_plans.remove(&old.file_identity) {
                    *old = plan;
                }
            }
            let mut new_decisions: HashMap<_, _> = next_admission
                .into_iter()
                .map(|decision| (decision.file_identity.clone(), decision))
                .collect();
            for old in admission.iter_mut() {
                if let Some(decision) = new_decisions.remove(&old.file_identity) {
                    *old = decision;
                }
            }
            admission.extend(new_decisions.into_values());
            admission.sort_by_key(|decision| {
                crate::nominal::NominalKey::new(
                    &decision.relative_path,
                    &decision.extension,
                    &decision.file_identity,
                )
            });
            ledger.refunded_chars += self.reclaim_chars(
                discovery,
                work_dir,
                profile,
                classified,
                admission,
                classifications,
                runtime,
                outputs,
                model,
            )?;
            let (candidates, pages, extractions, page_refund) =
                consumed(classified, admission, classifications, audits, outputs)?;
            ledger.rounds.push(RefillRoundAudit {
                round,
                planned_source_count: planned_count as u64,
                plan_sha256: crate::store::sha256_hex(&frozen_identity),
                candidate_slots_used: candidates,
                classification_pages_held: pages,
                extraction_slots_used: extractions,
                reserved_chars: model.base_chars()
                    + admission
                        .iter()
                        .map(|decision| decision.reserved_chars)
                        .sum::<u64>(),
                refunded_chars: ledger.refunded_chars,
                refunded_classification_pages: page_refund,
            });
            if outputs.results.len() == before_results
                && classifications
                    .values()
                    .filter(|result| {
                        result.status != PdfClassificationStatus::NotClassifiedByBudget
                    })
                    .count()
                    == before_classifications
            {
                ledger.stop_reason = "no_progress".into();
                break;
            }
        }
        let (candidates, pages, extractions, page_refund) =
            consumed(classified, admission, classifications, audits, outputs)?;
        ledger.candidate_slots_used = candidates;
        ledger.classification_pages_held = pages;
        ledger.extraction_slots_used = extractions;
        ledger.refunded_classification_pages = page_refund;
        ledger
            .validate()
            .map_err(|message| SchedulerFailure::internal(&message))?;
        metrics.refill_audit = Some(ledger);
        metrics.candidate_file_count = classified
            .iter()
            .filter(|plan| matches!(plan.action, PlanAction::Admit { .. }))
            .count() as u64;
        metrics.classification_slot_count = classified
            .iter()
            .filter(|plan| {
                matches!(
                    plan.pdf_classification,
                    PdfClassificationPlan::Classify { .. }
                )
            })
            .count() as u64;
        metrics.nominal_charged_pages_total = classified
            .iter()
            .filter_map(|plan| match plan.pdf_classification {
                PdfClassificationPlan::Classify { charged_pages } => Some(charged_pages),
                _ => None,
            })
            .sum();
        metrics.admitted_file_count = admission
            .iter()
            .filter(|decision| matches!(decision.action, PlanAction::Admit { .. }))
            .count() as u64;
        metrics.extraction_slot_count = admission
            .iter()
            .filter(|decision| {
                matches!(
                    decision.action,
                    PlanAction::Admit {
                        route: RouteKind::Pdf
                    }
                ) && classifications
                    .get(&decision.file_identity)
                    .is_some_and(|result| {
                        result.status == PdfClassificationStatus::TextInParseWindow
                    })
            })
            .count() as u64;
        Ok(())
    }
}
