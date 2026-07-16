use ai_daily_scanner_contract::{Diagnostic, ErrorCode, OfficeParseProfile};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Deterministic,
    EnvironmentUnavailable,
    ContractFailure,
    RecoverableParserFailure,
}

impl FailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::EnvironmentUnavailable => "environment_unavailable",
            Self::ContractFailure => "contract_failure",
            Self::RecoverableParserFailure => "recoverable_parser_failure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseFailure {
    pub class: FailureClass,
    pub diagnostic: Diagnostic,
}

impl ParseFailure {
    pub fn is_timeout(&self) -> bool {
        self.diagnostic.error_code == ErrorCode::ParserTimeout
    }
}

pub fn permits_office_fallback(failure: &ParseFailure, profile: &OfficeParseProfile) -> bool {
    if !profile.fallback_enabled {
        return false;
    }
    match failure.class {
        FailureClass::Deterministic => failure.is_timeout() && profile.fallback_after_timeout,
        FailureClass::EnvironmentUnavailable
        | FailureClass::ContractFailure
        | FailureClass::RecoverableParserFailure => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_daily_scanner_contract::{DiagnosticStage, Nullable};

    fn profile(fallback_after_timeout: bool) -> OfficeParseProfile {
        OfficeParseProfile {
            primary_backend: "rust_office_oxide_v1".to_string(),
            fallback_enabled: true,
            fallback_order: Vec::new(),
            fallback_after_timeout,
            fallback_policy_version: "hybrid_v1".to_string(),
            legacy_extensions_enabled: false,
            excel_max_sheets: 1,
            excel_max_rows: 1,
            excel_max_columns: 1,
            docx_max_paragraphs: 1,
            docx_max_tables: 1,
            docx_table_max_rows: 1,
            docx_table_max_cols: 1,
            pptx_max_slides: 1,
            pptx_include_notes: false,
            document_excerpt_max_chars: 1,
        }
    }

    fn failure(class: FailureClass, code: ErrorCode) -> ParseFailure {
        ParseFailure {
            class,
            diagnostic: Diagnostic {
                error_code: code,
                message: "synthetic failure".to_string(),
                retryable: false,
                stage: DiagnosticStage::Parse,
                file_path: Nullable(None),
                backend: Nullable(None),
            },
        }
    }

    #[test]
    fn deterministic_failures_do_not_fallback_except_explicit_timeout() {
        assert!(!permits_office_fallback(
            &failure(FailureClass::Deterministic, ErrorCode::ParserFailed),
            &profile(true)
        ));
        assert!(!permits_office_fallback(
            &failure(FailureClass::Deterministic, ErrorCode::ParserTimeout),
            &profile(false)
        ));
        assert!(permits_office_fallback(
            &failure(FailureClass::Deterministic, ErrorCode::ParserTimeout),
            &profile(true)
        ));
    }

    #[test]
    fn explicit_recoverable_classes_allow_fallback() {
        for class in [
            FailureClass::EnvironmentUnavailable,
            FailureClass::ContractFailure,
            FailureClass::RecoverableParserFailure,
        ] {
            assert!(permits_office_fallback(
                &failure(class, ErrorCode::ParserFailed),
                &profile(false)
            ));
        }
    }

    #[test]
    fn audit_names_match_the_frozen_contract() {
        assert_eq!(FailureClass::Deterministic.as_str(), "deterministic");
        assert_eq!(
            FailureClass::EnvironmentUnavailable.as_str(),
            "environment_unavailable"
        );
        assert_eq!(FailureClass::ContractFailure.as_str(), "contract_failure");
        assert_eq!(
            FailureClass::RecoverableParserFailure.as_str(),
            "recoverable_parser_failure"
        );
    }
}
