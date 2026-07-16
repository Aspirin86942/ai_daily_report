//! Deterministic context action and priority decisions over scanner evidence.

use ai_daily_scanner_contract::{
    AuditWorkerLane, CacheStatus, ContextAction, ContextDecision, ContextProfile, Diagnostic,
    ParseStatus, Validate,
};

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
            ParseStatus::Error | ParseStatus::Timeout | ParseStatus::NotParsed
                if self.error.is_none() =>
            {
                return Err("failed context evidence requires a diagnostic".to_string());
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
    profile: &ContextProfile,
) -> Result<Vec<DecidedFile>, String> {
    profile.validate()?;
    let mut decided = Vec::with_capacity(evidence.len());
    for evidence in evidence {
        evidence.validate()?;
        let has_error = evidence.parse_status != ParseStatus::Success;
        let priority = priority_for(&evidence.relative_path, &evidence.extension, has_error);
        let input_chars = evidence.content.chars().count() as u64;
        let observed_size = evidence.size_bytes.unwrap_or(0);
        let (action, reason) = if has_error {
            (ContextAction::Error, "parse_error")
        } else if observed_size > profile.large_file_max_bytes {
            (ContextAction::MetadataOnly, "file_size_policy")
        } else if input_chars <= profile.per_file_max_chars && !evidence.truncated {
            (ContextAction::Keep, "small_file_keep")
        } else {
            (
                ContextAction::Compress,
                compression_reason(&evidence.extension),
            )
        };
        let error_code = evidence
            .error
            .as_ref()
            .map(|error| enum_text(&error.error_code))
            .unwrap_or_default();
        decided.push(DecidedFile {
            decision: ContextDecision {
                relative_path: evidence.relative_path.clone(),
                action,
                reason: reason.to_string(),
                priority,
                input_chars,
                output_chars: 0,
                truncated: evidence.truncated,
                error_code,
            },
            evidence,
        });
    }
    decided.sort_by(|left, right| {
        left.decision
            .priority
            .cmp(&right.decision.priority)
            .then_with(|| {
                left.decision
                    .relative_path
                    .to_lowercase()
                    .cmp(&right.decision.relative_path.to_lowercase())
            })
            .then_with(|| {
                left.decision
                    .relative_path
                    .cmp(&right.decision.relative_path)
            })
            .then_with(|| {
                left.evidence
                    .file_identity
                    .cmp(&right.evidence.file_identity)
            })
    });
    Ok(decided)
}

fn priority_for(relative_path: &str, extension: &str, has_error: bool) -> u64 {
    if has_error {
        return 80;
    }
    let path_key = format!(
        "\\{}",
        relative_path
            .to_lowercase()
            .replace('/', "\\")
            .trim_matches('\\')
    );
    if path_key.contains("\\.pytest_cache\\") || path_key.contains("\\data\\benchmarks\\") {
        70
    } else if path_key.contains("\\logs\\") {
        60
    } else if OFFICE_OR_PDF_EXTENSIONS.contains(&extension) {
        20
    } else if TEXT_KEEP_EXTENSIONS.contains(&extension) {
        30
    } else {
        50
    }
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
