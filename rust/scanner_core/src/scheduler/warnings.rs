use std::collections::BTreeMap;

use ai_daily_scanner_contract::{Diagnostic, DiagnosticStage, ErrorCode, Nullable};

use crate::store::{DiagnosticSeverity, RunDiagnosticRecord};

/// spec Part 2.2 bounded warning projection: run-level warnings first, then up
/// to 256 detail rows, then a single `DIAGNOSTICS_AGGREGATED` row
/// (`stage=internal`, `retryable=any folded true`, `file_path/backend=null`).
/// Error-severity diagnostics pass through untouched (the envelope error).
pub(super) fn project_warnings(diagnostics: Vec<RunDiagnosticRecord>) -> Vec<RunDiagnosticRecord> {
    let (mut warnings, errors): (Vec<_>, Vec<_>) = diagnostics
        .into_iter()
        .partition(|record| record.severity != DiagnosticSeverity::Error);
    if warnings.len() > 256 {
        let aggregate = aggregate_warning(&warnings[256..]);
        warnings.truncate(256);
        warnings.push(aggregate);
    }
    warnings.extend(errors);
    warnings
}

fn aggregate_warning(folded: &[RunDiagnosticRecord]) -> RunDiagnosticRecord {
    let mut groups: BTreeMap<(String, String, bool), u64> = BTreeMap::new();
    let mut any_retryable = false;
    for record in folded {
        let key = (
            crate::store::inventory::enum_text(&record.diagnostic.stage),
            crate::store::inventory::enum_text(&record.diagnostic.error_code),
            record.diagnostic.retryable,
        );
        *groups.entry(key).or_insert(0) += 1;
        any_retryable |= record.diagnostic.retryable;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut other_count = 0_u64;
    for ((stage, code, retryable), count) in groups {
        let part = format!("{stage}:{code}:retryable={retryable}:{count}");
        let projected = parts.iter().map(|p| p.len() + 1).sum::<usize>() + part.len();
        if projected <= 4_096 {
            parts.push(part);
        } else {
            other_count = other_count.saturating_add(count);
        }
    }
    if other_count > 0 {
        parts.push(format!("other:{other_count}"));
    }
    let message = format!(
        "aggregated {} diagnostics: {}",
        folded.len(),
        parts.join(",")
    );
    RunDiagnosticRecord {
        severity: DiagnosticSeverity::Warning,
        diagnostic: Diagnostic {
            error_code: ErrorCode::DiagnosticsAggregated,
            message: message.chars().take(4_096).collect(),
            retryable: any_retryable,
            stage: DiagnosticStage::Internal,
            file_path: Nullable(None),
            backend: Nullable(None),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warning(error_code: ErrorCode, retryable: bool) -> RunDiagnosticRecord {
        RunDiagnosticRecord {
            severity: DiagnosticSeverity::Warning,
            diagnostic: Diagnostic {
                error_code,
                message: "synthetic".to_string(),
                retryable,
                stage: DiagnosticStage::Parse,
                file_path: Nullable(Some("C:\\f\\a.md".to_string())),
                backend: Nullable(None),
            },
        }
    }

    #[test]
    fn project_warnings_folds_overflow_into_one_aggregate() {
        let mut diagnostics: Vec<RunDiagnosticRecord> = (0..300)
            .map(|index| {
                warning(
                    if index % 2 == 0 {
                        ErrorCode::ParserFailed
                    } else {
                        ErrorCode::ParserTimeout
                    },
                    index % 3 == 0,
                )
            })
            .collect();
        let first = RunDiagnosticRecord {
            severity: DiagnosticSeverity::Error,
            diagnostic: Diagnostic {
                error_code: ErrorCode::InternalError,
                message: "primary".to_string(),
                retryable: false,
                stage: DiagnosticStage::Internal,
                file_path: Nullable(None),
                backend: Nullable(None),
            },
        };
        let mut second = first.clone();
        second.diagnostic.message = "second".to_owned();
        let retained = diagnostics[..256].to_vec();
        diagnostics.insert(5, first.clone());
        diagnostics.push(second.clone());

        let projected = project_warnings(diagnostics);
        let warnings: Vec<_> = projected
            .iter()
            .filter(|record| record.severity == DiagnosticSeverity::Warning)
            .collect();
        let errors: Vec<_> = projected
            .iter()
            .filter(|record| record.severity == DiagnosticSeverity::Error)
            .collect();
        // 256 detail + 1 DIAGNOSTICS_AGGREGATED.
        assert_eq!(warnings.len(), 257);
        assert_eq!(
            warnings
                .iter()
                .filter(|record| record.diagnostic.error_code == ErrorCode::DiagnosticsAggregated)
                .count(),
            1
        );
        let aggregate = warnings
            .iter()
            .find(|record| record.diagnostic.error_code == ErrorCode::DiagnosticsAggregated)
            .expect("aggregate");
        assert_eq!(aggregate.diagnostic.stage, DiagnosticStage::Internal);
        assert!(
            aggregate.diagnostic.retryable,
            "any folded retryable => true"
        );
        assert!(aggregate.diagnostic.file_path.0.is_none());
        assert!(aggregate.diagnostic.backend.0.is_none());
        assert_eq!(&projected[..256], &retained);
        assert_eq!(errors, [&first, &second]);
        assert_eq!(&projected[257..], &[first, second]);
    }

    #[test]
    fn project_warnings_passes_through_few_warnings() {
        let diagnostics = vec![
            warning(ErrorCode::ParserFailed, false),
            warning(ErrorCode::ParserTimeout, true),
        ];
        let projected = project_warnings(diagnostics);
        assert_eq!(projected.len(), 2);
        assert!(!projected
            .iter()
            .any(|record| record.diagnostic.error_code == ErrorCode::DiagnosticsAggregated));
    }
}
