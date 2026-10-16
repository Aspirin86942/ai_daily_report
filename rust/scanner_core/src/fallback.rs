use ai_daily_scanner_contract::{Diagnostic, ErrorCode};

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
