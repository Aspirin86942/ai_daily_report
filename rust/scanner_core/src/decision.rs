//! Deterministic context action and priority decisions over scanner evidence.
//!
//! State matrix (spec Part 2.2): `has_error` is true ONLY for `ParseStatus::Error`;
//! `Timeout` maps to the error action / timeout count; `NotParsed` (semantic /
//! policy / runtime) maps to the Omit action with a frozen budget reason and never
//! carries a per-file Diagnostic. Ordering is the single `NominalKey`
//! implementation — a parse failure changes status/action/reason but never the
//! position, so the decision output is cache-independent.

use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheStatus, ContextAction, ContextDecision, ContextProfile, ContextProfileV2,
    Diagnostic, ParseStatus, Validate,
};

use crate::nominal::NominalKey;

/// The minimal budget profile the decision layer and renderer need. Both the
/// frozen v1 `ContextProfile` and the v2 `ContextProfileV2` implement it, so
/// the decision/scheduler path never converts or duplicates the profile.
pub trait BudgetProfile {
    fn global_max_chars(&self) -> u64;
    fn per_file_max_chars(&self) -> u64;
    fn large_file_max_bytes(&self) -> u64;
    fn profile_name(&self) -> &str;
    fn compression_policy_version(&self) -> &str;
}

impl BudgetProfile for ContextProfile {
    fn global_max_chars(&self) -> u64 {
        self.global_max_chars
    }
    fn per_file_max_chars(&self) -> u64 {
        self.per_file_max_chars
    }
    fn large_file_max_bytes(&self) -> u64 {
        self.large_file_max_bytes
    }
    fn profile_name(&self) -> &str {
        &self.profile_name
    }
    fn compression_policy_version(&self) -> &str {
        &self.compression_policy_version
    }
}

impl BudgetProfile for ContextProfileV2 {
    fn global_max_chars(&self) -> u64 {
        self.global_max_chars
    }
    fn per_file_max_chars(&self) -> u64 {
        self.per_file_max_chars
    }
    fn large_file_max_bytes(&self) -> u64 {
        self.large_file_max_bytes
    }
    fn profile_name(&self) -> &str {
        &self.profile_name
    }
    fn compression_policy_version(&self) -> &str {
        &self.compression_policy_version
    }
}

const OFFICE_OR_PDF_EXTENSIONS: &[&str] = &[
    ".doc", ".docx", ".pdf", ".ppt", ".pptx", ".xls", ".xlsm", ".xlsx",
];
const TEXT_KEEP_EXTENSIONS: &[&str] = &[".md", ".txt"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFileEvidence {
    pub file_identity: String,
    pub absolute_path: String,
    pub relative_path: String,
    pub extension: String,
    /// `None` preserves the legacy unreadable-size behavior without rereading the file.
    pub size_bytes: Option<u64>,
    pub content: String,
    pub parser_backend: String,
    pub worker_lane: AuditWorkerLane,
    pub cache_status: CacheStatus,
    pub parse_status: ParseStatus,
    pub truncated: bool,
    pub error: Option<Diagnostic>,
    /// Action reason override. NotParsed MUST carry its frozen budget reason
    /// (`semantic_file_quota_exhausted`, `pdf_*_quota_exhausted`,
    /// `global_context_budget_exceeded`, `file_size_policy`,
    /// `legacy_extension_disabled`, `runtime_deadline_exhausted`); Error may
    /// carry `profile_route_invariant` / `source_guard_unavailable` /
    /// `source_version_changed`. `None` selects the default reason
    /// (keep/compress/parse_error).
    pub reason: Option<String>,
}

impl ContextFileEvidence {
    pub fn validate(&self) -> Result<(), String> {
        let probe = ContextDecision {
            relative_path: self.relative_path.clone(),
            action: ContextAction::Keep,
            reason: "validation_probe".to_string(),
            priority: 0,
            input_chars: self.content.chars().count() as u64,
            output_chars: 0,
            truncated: self.truncated,
            error_code: String::new(),
        };
        probe.validate()?;
        if self.file_identity.is_empty()
            || self.file_identity.chars().count() > 4_096
            || !is_absolute_contract_path(&self.absolute_path)
            || !is_extension(&self.extension)
            || self.parser_backend.is_empty()
            || self.parser_backend.chars().count() > 1_024
        {
            return Err("context file evidence is invalid".to_string());
        }
        match self.parse_status {
            ParseStatus::Success if self.error.is_some() => {
                return Err("successful context evidence cannot carry an error".to_string());
            }
            ParseStatus::Error | ParseStatus::Timeout if self.error.is_none() => {
                return Err("failed context evidence requires a diagnostic".to_string());
            }
            // spec Part 2.1/2.2: NotParsed (semantic/policy/runtime) never carries
            // a fabricated per-file error; runtime NotParsed only references the
            // run-level deadline Diagnostic.
            ParseStatus::NotParsed if self.error.is_some() => {
                return Err("not-parsed evidence cannot carry a per-file diagnostic".to_string());
            }
            ParseStatus::NotParsed if self.reason.is_none() => {
                return Err("not-parsed evidence requires a budget reason".to_string());
            }
            _ => {}
        }
        if let Some(error) = &self.error {
            error.validate()?;
            if error.file_path.0.as_deref() != Some(self.absolute_path.as_str()) {
                return Err("context diagnostic path does not match its evidence".to_string());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecidedFile {
    pub evidence: ContextFileEvidence,
    pub decision: ContextDecision,
}

pub(crate) fn decide_files(
    evidence: Vec<ContextFileEvidence>,
    profile: &impl BudgetProfile,
) -> Result<Vec<DecidedFile>, String> {
    let mut decided = Vec::with_capacity(evidence.len());
    for evidence in evidence {
        evidence.validate()?;
        let key = NominalKey::new(
            &evidence.relative_path,
            &evidence.extension,
            &evidence.file_identity,
        );
        let observed_size = evidence.size_bytes.unwrap_or(0);
        let content_chars = evidence.content.chars().count() as u64;
        let (action, reason) = match evidence.parse_status {
            // spec Part 2.2: Timeout is the error action + timeout_count, never Keep.
            ParseStatus::Error | ParseStatus::Timeout => (
                ContextAction::Error,
                evidence
                    .reason
                    .clone()
                    .unwrap_or_else(|| "parse_error".to_string()),
            ),
            ParseStatus::NotParsed => (
                ContextAction::Omit,
                evidence
                    .reason
                    .clone()
                    .expect("not-parsed evidence must carry a budget reason"),
            ),
            ParseStatus::Success => {
                // spec Part 3.2: a no-text PDF metadata-only draft (body parser
                // never ran) keeps its frozen metadata_only action and
                // `pdf_no_text_in_parse_window` reason; the size/content
                // heuristics must not relabel it Keep/Compress.
                if evidence.parser_backend == "pdf_metadata_v2"
                    && evidence.reason.as_deref() == Some("pdf_no_text_in_parse_window")
                {
                    (
                        ContextAction::MetadataOnly,
                        "pdf_no_text_in_parse_window".to_string(),
                    )
                } else if observed_size > profile.large_file_max_bytes() {
                    (ContextAction::MetadataOnly, "file_size_policy".to_string())
                } else if content_chars <= profile.per_file_max_chars() && !evidence.truncated {
                    (ContextAction::Keep, "small_file_keep".to_string())
                } else {
                    (
                        ContextAction::Compress,
                        compression_reason(&evidence.extension).to_string(),
                    )
                }
            }
        };
        // spec Part 2.2: `input_chars` for no-text metadata / NotParsed / Error /
        // Timeout has no trusted body, so it uses the discovery size approximation
        // and the renderer displays a `~` marker.
        let input_chars = match action {
            ContextAction::Keep | ContextAction::Compress => content_chars,
            _ => observed_size,
        };
        // spec Part 2.2: `error_code` is fixed empty for Success and every
        // NotParsed; Error/Timeout must equal the final Diagnostic code.
        let error_code = match evidence.parse_status {
            ParseStatus::Error | ParseStatus::Timeout => evidence
                .error
                .as_ref()
                .map(|error| enum_text(&error.error_code))
                .unwrap_or_default(),
            _ => String::new(),
        };
        decided.push(DecidedFile {
            decision: ContextDecision {
                relative_path: evidence.relative_path.clone(),
                action,
                reason,
                priority: key.priority,
                input_chars,
                output_chars: 0,
                truncated: evidence.truncated,
                error_code,
            },
            evidence,
        });
    }
    // Single ordering implementation (spec Part 1.1): the full four-tuple key,
    // unchanged by parse status.
    decided.sort_by(|left, right| {
        NominalKey::new(
            &left.evidence.relative_path,
            &left.evidence.extension,
            &left.evidence.file_identity,
        )
        .cmp(&NominalKey::new(
            &right.evidence.relative_path,
            &right.evidence.extension,
            &right.evidence.file_identity,
        ))
    });
    Ok(decided)
}

fn compression_reason(extension: &str) -> &'static str {
    if extension == ".log" {
        "large_log_tail"
    } else if OFFICE_OR_PDF_EXTENSIONS.contains(&extension) {
        "large_document_summary"
    } else {
        "medium_text_compress"
    }
}

fn enum_text<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .expect("contract enum must serialize to text")
}

fn is_absolute_contract_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    let drive_rooted = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    (value.starts_with('/') || value.starts_with("\\\\") || drive_rooted)
        && !value.contains('\0')
        && value.chars().count() <= 32_767
}

fn is_extension(value: &str) -> bool {
    (2..=32).contains(&value.chars().count())
        && value.starts_with('.')
        && value.chars().skip(1).all(|character| {
            !character.is_ascii_uppercase() && !matches!(character, '\\' | '/' | ':' | '\0')
        })
}

// Keep TEXT_KEEP_EXTENSIONS referenced so the module documents the same route
// table as `nominal.rs`; it is intentionally not a second ordering implementation.
#[allow(dead_code)]
const _TEXT_KEEP_EXTENSIONS_USED: &[&str] = TEXT_KEEP_EXTENSIONS;
