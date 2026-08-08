//! Two-phase deterministic admission plans (spec Part 1.2).
//!
//! Phase A `ClassificationPlan` is frozen BEFORE any PDF classification I/O:
//! no-I/O policy/invariant rejects first, then `max_candidate_files` candidate
//! slots are assigned by nominal rank, and every candidate PDF reserves a full
//! `pdf_max_pages` charge (cache hit and miss pay the same nominal charge).
//!
//! Phase B `ContentAdmissionPlan` is frozen AFTER classification but BEFORE any
//! body parse I/O: a single pass by nominal rank admits files whose
//! `reserved_delta` fits, a text PDF additionally needs an extraction slot, and
//! a no-text PDF becomes a metadata-only draft. There is NO backfill.
//!
//! Once both plans are frozen, the cache only decides reuse-vs-execute; it can
//! never change ParseStatus/action/reason/order/slots.

use crate::budget_model::{ContextBudgetModel, RouteHint, RouteKind};
use ai_daily_scanner_contract::{NormalizedScannerProfileV2, PdfClassificationStatus, SourceGuardKind};
use std::collections::BTreeMap;

/// Frozen semantic/policy NotParsed reasons (spec Part 2.1). Omit action, no
/// error Diagnostic, snapshot-eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotParsedReason {
    SemanticFileQuotaExhausted,
    PdfClassificationPageQuotaExhausted,
    PdfTextExtractionQuotaExhausted,
    GlobalContextBudgetExceeded,
    FileSizePolicy,
    LegacyExtensionDisabled,
}

impl NotParsedReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SemanticFileQuotaExhausted => "semantic_file_quota_exhausted",
            Self::PdfClassificationPageQuotaExhausted => "pdf_classification_page_quota_exhausted",
            Self::PdfTextExtractionQuotaExhausted => "pdf_text_extraction_quota_exhausted",
            Self::GlobalContextBudgetExceeded => "global_context_budget_exceeded",
            Self::FileSizePolicy => "file_size_policy",
            Self::LegacyExtensionDisabled => "legacy_extension_disabled",
        }
    }
}

/// Frozen no-I/O invariant rejects (spec Part 2.1). Error action, must carry a
/// Diagnostic, never snapshot-eligible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
    ProfileRouteInvariant,
    SourceGuardUnavailable,
}

impl RejectReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProfileRouteInvariant => "profile_route_invariant",
            Self::SourceGuardUnavailable => "source_guard_unavailable",
        }
    }
}

/// Per-file plan action produced by the admission plans and consumed by the
/// scheduler (T2-4), which maps it to the ParseStatus/action matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    /// Admitted: normal parse, or a no-text metadata-only draft.
    Admit { route: RouteKind },
    /// NotParsed with a semantic/policy reason (omit, no error Diagnostic).
    NotParsed { reason: NotParsedReason },
    /// Invariant or source-guard reject (Error, must carry a Diagnostic).
    Reject { reason: RejectReason },
    /// PDF classifier produced unknown/error -> becomes Timeout/Error.
    ClassifierFailed { status: PdfClassificationStatus },
}

/// A discovered file as seen by the admission plans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanCandidate {
    pub file_identity: String,
    pub relative_path: String,
    pub extension: String,
    pub size_bytes: u64,
    pub source_guard_kind: SourceGuardKind,
}

/// Frozen stage-A output for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedPlan {
    pub file_identity: String,
    pub relative_path: String,
    pub extension: String,
    pub priority: u64,
    pub action: PlanAction,
    pub pdf_classification: PdfClassificationPlan,
}

/// Stage-A PDF classification intent. Frozen before any classification I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfClassificationPlan {
    NotPdf,
    /// Candidate PDF that received a full page reservation; the actual
    /// text/no-text/unknown/error result is merged in before stage B.
    Classify { charged_pages: u64 },
    /// Candidate PDF that did not get a page reservation.
    NotClassifiedByBudget,
}

/// Typed classifier result merged between stage A and stage B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfClassificationResult {
    pub file_identity: String,
    pub status: PdfClassificationStatus,
    pub page_count: Option<u64>,
    pub result_examined_pages: Option<u64>,
    pub error_code: Option<String>,
}

/// Frozen stage-B output for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionDecision {
    pub file_identity: String,
    pub relative_path: String,
    pub extension: String,
    pub priority: u64,
    pub action: PlanAction,
    /// Reserved chars for admitted files (0 for every non-admitted file).
    pub reserved_chars: u64,
}

/// Stage A: frozen before any PDF classification I/O (spec Part 1.2).
pub struct ClassificationPlan;

impl ClassificationPlan {
    pub fn build(
        files: Vec<PlanCandidate>,
        profile: &NormalizedScannerProfileV2,
        page_budget: u64,
    ) -> Vec<ClassifiedPlan> {
        let mut classified = Vec::with_capacity(files.len());
        let mut candidates = Vec::new();

        // 1) No-I/O policy/invariant rejects first; remaining files get a route.
        for file in files {
            let decision = classify_candidate_v2(&file, profile);
            let priority = crate::nominal::NominalKey::new(
                &file.relative_path,
                &file.extension,
                &file.file_identity,
            )
            .priority;
            match decision {
                RouteDecision::PolicyNotParsed(reason) => {
                    classified.push(ClassifiedPlan {
                        file_identity: file.file_identity,
                        relative_path: file.relative_path,
                        extension: file.extension,
                        priority,
                        action: PlanAction::NotParsed { reason },
                        pdf_classification: PdfClassificationPlan::NotPdf,
                    });
                }
                RouteDecision::InvariantReject(reason) => {
                    classified.push(ClassifiedPlan {
                        file_identity: file.file_identity,
                        relative_path: file.relative_path,
                        extension: file.extension,
                        priority,
                        action: PlanAction::Reject { reason },
                        pdf_classification: PdfClassificationPlan::NotPdf,
                    });
                }
                RouteDecision::Route(route) => {
                    if file.source_guard_kind == SourceGuardKind::Unavailable {
                        classified.push(ClassifiedPlan {
                            file_identity: file.file_identity,
                            relative_path: file.relative_path,
                            extension: file.extension,
                            priority,
                            action: PlanAction::Reject {
                                reason: RejectReason::SourceGuardUnavailable,
                            },
                            pdf_classification: PdfClassificationPlan::NotPdf,
                        });
                    } else {
                        candidates.push((file, route));
                    }
                }
            }
        }

        // 2) Candidate slots by nominal rank; PDFs reserve full pdf_max_pages.
        candidates.sort_by_key(|(file, _)| {
            crate::nominal::NominalKey::new(&file.relative_path, &file.extension, &file.file_identity)
        });
        let mut candidate_slots = profile.max_candidate_files;
        let mut page_remaining = page_budget;
        for (file, route) in candidates {
            let priority = crate::nominal::NominalKey::new(
                &file.relative_path,
                &file.extension,
                &file.file_identity,
            )
            .priority;
            if candidate_slots == 0 {
                classified.push(ClassifiedPlan {
                    file_identity: file.file_identity,
                    relative_path: file.relative_path,
                    extension: file.extension,
                    priority,
                    action: PlanAction::NotParsed {
                        reason: NotParsedReason::SemanticFileQuotaExhausted,
                    },
                    pdf_classification: PdfClassificationPlan::NotPdf,
                });
                continue;
            }
            candidate_slots -= 1;
            if route == RouteKind::Pdf {
                let charge = profile.parse.pdf.max_pages;
                if charge > page_remaining {
                    classified.push(ClassifiedPlan {
                        file_identity: file.file_identity,
                        relative_path: file.relative_path,
                        extension: file.extension,
                        priority,
                        action: PlanAction::NotParsed {
                            reason: NotParsedReason::PdfClassificationPageQuotaExhausted,
                        },
                        pdf_classification: PdfClassificationPlan::NotClassifiedByBudget,
                    });
                } else {
                    page_remaining -= charge;
                    classified.push(ClassifiedPlan {
                        file_identity: file.file_identity,
                        relative_path: file.relative_path,
                        extension: file.extension,
                        priority,
                        action: PlanAction::Admit { route },
                        pdf_classification: PdfClassificationPlan::Classify {
                            charged_pages: charge,
                        },
                    });
                }
            } else {
                classified.push(ClassifiedPlan {
                    file_identity: file.file_identity,
                    relative_path: file.relative_path,
                    extension: file.extension,
                    priority,
                    action: PlanAction::Admit { route },
                    pdf_classification: PdfClassificationPlan::NotPdf,
                });
            }
        }

        // Single ordering implementation: the whole plan is nominal-ordered.
        classified.sort_by_key(|plan| {
            crate::nominal::NominalKey::new(&plan.relative_path, &plan.extension, &plan.file_identity)
        });
        classified
    }
}

/// Stage B: frozen after classification, before any body parse I/O.
pub struct ContentAdmissionPlan;

impl ContentAdmissionPlan {
    /// `classifications` maps `file_identity` -> classifier result for every
    /// PDF that carries `PdfClassificationPlan::Classify`.
    pub fn build(
        classified: &[ClassifiedPlan],
        profile: &NormalizedScannerProfileV2,
        model: &ContextBudgetModel,
        classifications: &BTreeMap<String, PdfClassificationResult>,
    ) -> Vec<AdmissionDecision> {
        let mut decisions = Vec::with_capacity(classified.len());
        let mut running = 0_u64;
        let mut extraction_slots = profile.max_pdf_text_extractions;

        for plan in classified {
            let (action, reserved) = match &plan.action {
                PlanAction::Admit { route } => {
                    let hint = route_hint(plan, *route, profile);
                    let delta = model.reserved_delta(&hint, None);
                    let outcome = match plan.pdf_classification {
                        PdfClassificationPlan::NotPdf => {
                            admit_plain(model, delta, &mut running, *route)
                        }
                        PdfClassificationPlan::Classify { .. } => admit_pdf(
                            plan,
                            *route,
                            model,
                            classifications,
                            delta,
                            &mut running,
                            &mut extraction_slots,
                        ),
                        PdfClassificationPlan::NotClassifiedByBudget => {
                            // A budget-excluded PDF is frozen as NotParsed in
                            // stage A and can never reach an Admit branch; fail
                            // closed instead of panicking if the invariant breaks.
                            PlanAction::NotParsed {
                                reason: NotParsedReason::GlobalContextBudgetExceeded,
                            }
                        }
                    };
                    let reserved = if matches!(outcome, PlanAction::Admit { .. }) {
                        delta
                    } else {
                        0
                    };
                    (outcome, reserved)
                }
                _ => (plan.action.clone(), 0),
            };
            decisions.push(AdmissionDecision {
                file_identity: plan.file_identity.clone(),
                relative_path: plan.relative_path.clone(),
                extension: plan.extension.clone(),
                priority: plan.priority,
                action,
                reserved_chars: reserved,
            });
        }
        decisions
    }
}

/// Plain file (non-PDF or not-yet-classified route): admit if `reserved_delta`
/// fits, otherwise `NotParsed/global_context_budget_exceeded`. The single pass
/// continues to later (possibly smaller) files; there is NO backfill.
fn admit_plain(
    model: &ContextBudgetModel,
    delta: u64,
    running: &mut u64,
    route: RouteKind,
) -> PlanAction {
    if model.admits(*running, delta) {
        *running = running.saturating_add(delta);
        PlanAction::Admit { route }
    } else {
        PlanAction::NotParsed {
            reason: NotParsedReason::GlobalContextBudgetExceeded,
        }
    }
}

/// PDF admission using the merged classifier result (spec Part 1.2 stage B 3-4).
fn admit_pdf(
    plan: &ClassifiedPlan,
    route: RouteKind,
    model: &ContextBudgetModel,
    classifications: &BTreeMap<String, PdfClassificationResult>,
    delta: u64,
    running: &mut u64,
    extraction_slots: &mut u64,
) -> PlanAction {
    let status = classifications
        .get(&plan.file_identity)
        .map(|result| result.status);
    match status {
        Some(PdfClassificationStatus::NoTextInParseWindow) => {
            // no-text admitted -> successful metadata-only draft, no extraction
            // slot; if the metadata section cannot fit it is a global-budget Omit.
            admit_plain(model, delta, running, route)
        }
        Some(PdfClassificationStatus::TextInParseWindow) => {
            if !model.admits(*running, delta) {
                // budget reason does NOT consume an extraction slot
                PlanAction::NotParsed {
                    reason: NotParsedReason::GlobalContextBudgetExceeded,
                }
            } else if *extraction_slots == 0 {
                PlanAction::NotParsed {
                    reason: NotParsedReason::PdfTextExtractionQuotaExhausted,
                }
            } else {
                *extraction_slots -= 1;
                *running = running.saturating_add(delta);
                PlanAction::Admit { route }
            }
        }
        Some(PdfClassificationStatus::Unknown) => PlanAction::ClassifierFailed {
            status: PdfClassificationStatus::Unknown,
        },
        Some(PdfClassificationStatus::Error) => PlanAction::ClassifierFailed {
            status: PdfClassificationStatus::Error,
        },
        Some(PdfClassificationStatus::NotClassifiedByBudget) | None => {
            // A Classify plan must have a result; a missing/not-classified
            // result fails closed toward not admitting.
            PlanAction::ClassifierFailed {
                status: PdfClassificationStatus::Unknown,
            }
        }
    }
}

fn route_hint(
    plan: &ClassifiedPlan,
    route: RouteKind,
    profile: &NormalizedScannerProfileV2,
) -> RouteHint {
    RouteHint {
        relative_path: plan.relative_path.clone(),
        extension: plan.extension.clone(),
        backend: route.backend().to_string(),
        worker_lane: route.worker_lane().to_string(),
        max_excerpt_chars: route.max_excerpt_chars(profile),
    }
}

/// v2 route classification (mirror of the v1 `classify_candidate` semantics,
/// frozen for the admission plans).
enum RouteDecision {
    Route(RouteKind),
    PolicyNotParsed(NotParsedReason),
    InvariantReject(RejectReason),
}

fn classify_candidate_v2(
    file: &PlanCandidate,
    profile: &NormalizedScannerProfileV2,
) -> RouteDecision {
    if file.size_bytes > profile.execution.max_file_size_bytes {
        return RouteDecision::PolicyNotParsed(NotParsedReason::FileSizePolicy);
    }
    let route = match file.extension.as_str() {
        ".txt" | ".md" | ".csv" | ".json" | ".log" => Some(RouteKind::LightText),
        ".xlsx" => office_route(&profile.parse.office.primary_backend, RouteKind::RustXlsx),
        ".docx" | ".pptx" => {
            office_route(&profile.parse.office.primary_backend, RouteKind::RustOffice)
        }
        ".xls" => Some(RouteKind::PythonOffice),
        ".pdf" if profile.parse.pdf.backend == "pdf_text_v1" => Some(RouteKind::Pdf),
        ".pdf" => None,
        ".doc" | ".ppt" if profile.parse.office.legacy_extensions_enabled => {
            Some(RouteKind::PythonSharepointText)
        }
        ".doc" | ".ppt" => {
            return RouteDecision::PolicyNotParsed(NotParsedReason::LegacyExtensionDisabled);
        }
        _ => None,
    };
    match route {
        Some(route) => RouteDecision::Route(route),
        None => RouteDecision::InvariantReject(RejectReason::ProfileRouteInvariant),
    }
}

fn office_route(primary_backend: &str, rust_route: RouteKind) -> Option<RouteKind> {
    match primary_backend {
        "rust_office_oxide_v1" => Some(rust_route),
        "python_office_v1" => Some(RouteKind::PythonOffice),
        _ => None,
    }
}
