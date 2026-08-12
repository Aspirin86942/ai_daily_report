//! Deterministic context budget model (spec Part 1.3).
//!
//! The model is the SINGLE owner of the semantic character budget. It shares
//! the exact Unicode scalar counter with the renderer (`count_chars`), freezes
//! the omitted-summary reservation ahead of admission, and prices every
//! admitted file with a worst-case section delta derived only from normalized
//! route/parser limits — never from cache or parse results. A rendered section
//! that exceeds its reserved delta is a non-retryable `BUDGET_MODEL_MISMATCH`
//! internal Error (never a panic, never a silent Omit, never a truncation of
//! other files).

use ai_daily_scanner_contract::{ContextProfile, NormalizedScannerSettings};

pub const BUDGET_MODEL_MISMATCH_CODE: &str = "BUDGET_MODEL_MISMATCH";
pub const CONTEXT_FIXED_SECTIONS_OVER_BUDGET_CODE: &str = "CONTEXT_FIXED_SECTIONS_OVER_BUDGET";

pub const OMITTED_SUMMARY_RESERVATION_CAP: u64 = 12_000;
pub const OMITTED_RESERVATION_FRACTION_PERCENT: u64 = 20;

/// u64::MAX is 18446744073709551615 (20 digits).
pub const MAX_U64_DIGITS: u64 = 20;
/// Covers every frozen v2 reason literal (the longest is
/// `pdf_classification_page_quota_exhausted`, 36 chars) plus margin.
pub const MAX_SECTION_REASON_CHARS: u64 = 64;
/// Covers action / parser backend / worker lane / error code literals.
pub const MAX_SECTION_TEXT_CHARS: u64 = 64;
/// Contract `require_extension` bound.
pub const MAX_EXTENSION_CHARS: u64 = 32;
/// Markdown sections are joined with a blank line (`\n\n`).
pub const SECTION_SEPARATOR_CHARS: u64 = 2;
/// Longest allowed omitted detail row reason (frozen NotParsed reasons / codes).
pub const MAX_OMITTED_REASON_CHARS: u64 = 64;

/// Shared Unicode scalar count (== `str::chars().count()`). The renderer and
/// the budget model MUST use the same function; bytes or token estimates would
/// break the rendered <= reserved invariant.
pub fn count_chars(value: &str) -> u64 {
    value.chars().count() as u64
}

/// `omitted_summary_reservation = min(12_000, floor(global_max_chars * 20%))`
/// (spec Part 1.3). This reservation is exclusive to the omitted summary and
/// cannot be borrowed by body admission, nor is unused space refilled.
pub fn omitted_summary_reservation(global_max_chars: u64) -> u64 {
    let fraction = global_max_chars.saturating_mul(OMITTED_RESERVATION_FRACTION_PERCENT) / 100;
    fraction.min(OMITTED_SUMMARY_RESERVATION_CAP)
}

/// Worst-case length of one omitted detail row for a file:
/// `- {path} | action=omit | reason={max_reason} | input_chars=~{max_digits}`.
pub fn max_omitted_row_chars(relative_path: &str) -> u64 {
    let head = "- ";
    let middle = " | action=omit | reason=";
    let tail = " | input_chars=";
    count_chars(head)
        + count_chars(relative_path)
        + count_chars(middle)
        + MAX_OMITTED_REASON_CHARS
        + count_chars(tail)
        + MAX_U64_DIGITS
        + 1 // the `~` marker for size-approximated input chars
}

#[derive(Debug, Clone, PartialEq, Eq, Copy, Hash)]
pub enum BudgetError {
    ContextFixedSectionsOverBudget,
    BudgetModelMismatch,
}

impl BudgetError {
    pub const fn error_code(self) -> &'static str {
        match self {
            Self::ContextFixedSectionsOverBudget => CONTEXT_FIXED_SECTIONS_OVER_BUDGET_CODE,
            Self::BudgetModelMismatch => BUDGET_MODEL_MISMATCH_CODE,
        }
    }

    pub fn message(self) -> String {
        match self {
            Self::ContextFixedSectionsOverBudget => {
                "fixed context sections exceed the global character budget".to_string()
            }
            Self::BudgetModelMismatch => {
                "rendered context exceeded the reserved budget delta".to_string()
            }
        }
    }
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.error_code(), self.message())
    }
}

impl std::error::Error for BudgetError {}

/// Build the non-retryable `BUDGET_MODEL_MISMATCH` internal error string for
/// callers that keep the legacy `Result<_, String>` seam (the current
/// compressor).
pub fn budget_model_mismatch(context: &str) -> String {
    format!("{BUDGET_MODEL_MISMATCH_CODE}: {context}")
}

/// A parser route with the section-size caps the budget model prices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteKind {
    LightText,
    RustOffice,
    RustXlsx,
    Pdf,
    PythonOffice,
    PythonSharepointText,
}

impl RouteKind {
    pub const fn backend(self) -> &'static str {
        match self {
            Self::LightText => "light_text_v2",
            Self::RustOffice => "rust_office_oxide_v2",
            Self::RustXlsx => "rust_xlsx_bounded_v2",
            Self::Pdf => "python_pdf_text_v2",
            Self::PythonOffice => "python_office_v2",
            Self::PythonSharepointText => "python_sharepoint_text_v2",
        }
    }

    pub const fn worker_lane(self) -> &'static str {
        match self {
            Self::LightText => "rust_core",
            Self::RustOffice | Self::RustXlsx => "rust_office_process_v2",
            Self::Pdf | Self::PythonOffice | Self::PythonSharepointText => {
                "python_document_process_v2"
            }
        }
    }

    /// Maximum content a route can produce under the normalized parser limits.
    pub fn max_excerpt_chars(self, profile: &NormalizedScannerSettings) -> u64 {
        match self {
            Self::LightText => profile
                .parse
                .text
                .max_chars
                .min(profile.parse.text.excerpt_max_chars),
            Self::RustOffice | Self::RustXlsx | Self::PythonOffice | Self::PythonSharepointText => {
                profile.parse.office.document_excerpt_max_chars
            }
            Self::Pdf => profile.parse.pdf.excerpt_max_chars,
        }
    }
}

/// Per-file route hint consumed by [`ContextBudgetModel::reserved_delta`].
///
/// Carries the per-file path/extension plus the route caps. All section-size
/// maxima are derived from these normalized values, never from cache or parse
/// results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteHint {
    pub relative_path: String,
    pub extension: String,
    pub backend: String,
    pub worker_lane: String,
    pub max_excerpt_chars: u64,
}

/// A candidate file for the omitted-summary detail-slot pre-selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmittedCandidate {
    pub file_identity: String,
    pub relative_path: String,
    pub extension: String,
}

/// Identifies one pre-selected omitted detail slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotKey {
    pub file_identity: String,
    pub relative_path: String,
    pub extension: String,
}

/// Frozen plan for the omitted summary (spec Part 1.3).
///
/// The reservation is `min(12_000, floor(global_max_chars * 20%))`. The plan
/// reserves the mandatory header + the single catch-all aggregate row first,
/// then pre-selects detail slots by nominal rank using each file's worst-case
/// `max_omitted_row_chars`; the next row that does not fit stops the list.
/// After the detail slots, aggregate rows render in `(reason, extension)`
/// canonical order and overflow groups fold into the catch-all. The renderer
/// checks the whole section against the reservation, so the plan never needs
/// the final global-budget reasons to be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmittedSummaryPlan {
    pub reservation: u64,
    pub detail_slots: Vec<SlotKey>,
}

impl OmittedSummaryPlan {
    pub fn build(files: &[OmittedCandidate], global_max_chars: u64) -> Self {
        let reservation = omitted_summary_reservation(global_max_chars);
        let fixed = omitted_summary_fixed_chars();
        // Detail slots are pre-selected by nominal rank (single ordering
        // implementation), regardless of the caller's input order.
        let mut ordered: Vec<&OmittedCandidate> = files.iter().collect();
        ordered.sort_by_key(|file| {
            crate::nominal::NominalKey::new(
                &file.relative_path,
                &file.extension,
                &file.file_identity,
            )
        });
        let mut used = fixed;
        let mut detail_slots = Vec::new();
        for file in ordered {
            let row_cost = max_omitted_row_chars(&file.relative_path) + 1;
            if used.saturating_add(row_cost) <= reservation {
                detail_slots.push(SlotKey {
                    file_identity: file.file_identity.clone(),
                    relative_path: file.relative_path.clone(),
                    extension: file.extension.clone(),
                });
                used += row_cost;
            } else {
                break;
            }
        }
        OmittedSummaryPlan {
            reservation,
            detail_slots,
        }
    }
}

/// Cost of the mandatory header (`## 省略文件摘要` + count line) and the
/// single catch-all aggregate row, plus their newline separators and the
/// `\n\n` section separator.
fn omitted_summary_fixed_chars() -> u64 {
    let header = format!(
        "## 省略文件摘要\n- 省略文件数: {}\n",
        "9".repeat(MAX_U64_DIGITS as usize)
    );
    let catch_all = format!(
        "- 其他 | action=omit | count={}\n",
        "9".repeat(MAX_U64_DIGITS as usize)
    );
    count_chars(&header) + count_chars(&catch_all) + SECTION_SEPARATOR_CHARS
}

/// Deterministic semantic context budget (spec Part 1.3).
///
/// `base_chars = exact(header + fixed_sections + preexisting_bounded_error
/// sections) + omitted_summary_reservation`. `base_chars > global_max_chars`
/// fails closed with the non-retryable `CONTEXT_FIXED_SECTIONS_OVER_BUDGET`.
///
/// `reserved_delta` prices the worst of the success/metadata/bounded-error
/// section sizes. The admission plan walks files by nominal rank, admits a file
/// when `base_chars + sum(admitted.reserved_delta) <= global_max_chars`, and
/// never backfills.
#[derive(Debug, Clone)]
pub struct ContextBudgetModel {
    global_max_chars: u64,
    per_file_max_chars: u64,
    base_chars: u64,
    omitted_summary_reservation: u64,
}

impl ContextBudgetModel {
    /// `fixed_sections` are the pre-rendered sections that exist before any
    /// admitted file (header, run summary, notices, `## 文件证据`, and any
    /// preexisting bounded error sections frozen before `ContentAdmissionPlan`).
    pub fn new(profile: &ContextProfile, fixed_sections: &[String]) -> Result<Self, BudgetError> {
        let exact = fixed_sections
            .iter()
            .map(|section| count_chars(section))
            .sum::<u64>()
            + fixed_sections.len() as u64 * SECTION_SEPARATOR_CHARS;
        let omitted = omitted_summary_reservation(profile.global_max_chars);
        let base = exact
            .checked_add(omitted)
            .ok_or(BudgetError::BudgetModelMismatch)?;
        if base > profile.global_max_chars {
            return Err(BudgetError::ContextFixedSectionsOverBudget);
        }
        Ok(ContextBudgetModel {
            global_max_chars: profile.global_max_chars,
            per_file_max_chars: profile.per_file_max_chars,
            base_chars: base,
            omitted_summary_reservation: omitted,
        })
    }

    pub fn global_max_chars(&self) -> u64 {
        self.global_max_chars
    }

    pub fn per_file_max_chars(&self) -> u64 {
        self.per_file_max_chars
    }

    pub fn base_chars(&self) -> u64 {
        self.base_chars
    }

    pub fn omitted_summary_reservation(&self) -> u64 {
        self.omitted_summary_reservation
    }

    /// Worst-case reserved chars for a file section:
    /// `max(success_section_max, metadata_section_max, bounded_error_section_max)`.
    /// All maxima come from the normalized route/parser limits in `route`, so
    /// the charge is identical for cache hit and miss. `size_bytes` is the
    /// discovery size (used only for the size/`~input` lines; the formula is
    /// conservative at max digits).
    pub fn reserved_delta(&self, route: &RouteHint, _size_bytes: Option<u64>) -> u64 {
        let path_chars = count_chars(&route.relative_path);
        let body_max = self.per_file_max_chars.min(route.max_excerpt_chars);
        let success = success_section_max(path_chars, body_max);
        let metadata = metadata_section_max(path_chars);
        let error = bounded_error_section_max(path_chars);
        success.max(metadata).max(error)
    }

    /// Semantic admission check: `base_chars + running + delta <= global_max_chars`.
    pub fn admits(&self, running: u64, delta: u64) -> bool {
        self.base_chars
            .saturating_add(running)
            .saturating_add(delta)
            <= self.global_max_chars
    }

    /// Invariant: the actually rendered section must not exceed its reserved
    /// chars. A violation is a non-retryable `BUDGET_MODEL_MISMATCH` internal
    /// Error — the renderer must not panic, silently Omit, or truncate others.
    pub fn check_rendered_within_reserved(
        &self,
        rendered_chars: u64,
        reserved_chars: u64,
    ) -> Result<(), BudgetError> {
        if rendered_chars <= reserved_chars {
            Ok(())
        } else {
            Err(BudgetError::BudgetModelMismatch)
        }
    }
}

fn key_value_line(prefix: &str, value_chars: u64) -> u64 {
    count_chars(prefix) + value_chars + 1 // trailing "\n"
}

fn heading_chars(path_chars: u64) -> u64 {
    path_chars + count_chars("### ") + 1
}

/// Max success section: heading, action/reason/backend/lane lines, input and
/// output char counts, the truncation notice, fences, the full body at the
/// per-file budget, and the inter-section separator.
fn success_section_max(path_chars: u64, body_chars: u64) -> u64 {
    heading_chars(path_chars)
        + key_value_line("- action: ", MAX_SECTION_TEXT_CHARS)
        + key_value_line("- reason: ", MAX_SECTION_REASON_CHARS)
        + key_value_line("- parser_backend: ", MAX_SECTION_TEXT_CHARS)
        + key_value_line("- worker_lane: ", MAX_SECTION_TEXT_CHARS)
        + key_value_line("- input_chars: ", MAX_U64_DIGITS + 1) // "~" prefix
        + key_value_line("- output_chars: ", MAX_U64_DIGITS)
        + count_chars("\n- 内容已按单文件预算或解析预算截断\n")
        + count_chars("```text\n")
        + body_chars
        + count_chars("\n```\n")
        + SECTION_SEPARATOR_CHARS
}

/// Max metadata-only section: heading, action/reason/backend/lane/file_type/
/// size_bytes/input_chars lines, the fixed body note, and the separator.
fn metadata_section_max(path_chars: u64) -> u64 {
    heading_chars(path_chars)
        + key_value_line("- action: ", "metadata_only".chars().count() as u64)
        + key_value_line("- reason: ", MAX_SECTION_REASON_CHARS)
        + key_value_line("- parser_backend: ", MAX_SECTION_TEXT_CHARS)
        + key_value_line("- worker_lane: ", MAX_SECTION_TEXT_CHARS)
        + key_value_line("- file_type: ", MAX_EXTENSION_CHARS)
        + key_value_line("- size_bytes: ", MAX_U64_DIGITS)
        + key_value_line("- input_chars: ", MAX_U64_DIGITS + 1)
        + count_chars("- body: omitted_by_metadata_only_policy\n")
        + SECTION_SEPARATOR_CHARS
}

/// Max bounded error line: path + reason + error code (contract-bounded, the
/// final Diagnostic message stays in audit, never in `file_context`).
fn bounded_error_section_max(path_chars: u64) -> u64 {
    let prefix = "- ";
    let middle = " | reason=";
    let tail = " | error=";
    count_chars(prefix)
        + path_chars
        + count_chars(middle)
        + MAX_SECTION_REASON_CHARS
        + count_chars(tail)
        + MAX_SECTION_TEXT_CHARS
        + 1 // trailing "\n"
        + SECTION_SEPARATOR_CHARS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_chars_counts_unicode_scalars() {
        assert_eq!(count_chars("甲乙丙"), 3);
        assert_eq!(count_chars("a😀b"), 3);
        assert_eq!(count_chars(""), 0);
    }

    #[test]
    fn error_codes_are_the_frozen_literals() {
        assert_eq!(
            BudgetError::BudgetModelMismatch.error_code(),
            "BUDGET_MODEL_MISMATCH"
        );
        assert_eq!(
            BudgetError::ContextFixedSectionsOverBudget.error_code(),
            "CONTEXT_FIXED_SECTIONS_OVER_BUDGET"
        );
    }

    #[test]
    fn route_kind_backends_and_lanes_are_stable() {
        assert_eq!(RouteKind::Pdf.backend(), "python_pdf_text_v2");
        assert_eq!(
            RouteKind::RustOffice.worker_lane(),
            "rust_office_process_v2"
        );
        assert_eq!(
            RouteKind::PythonSharepointText.backend(),
            "python_sharepoint_text_v2"
        );
    }
}
