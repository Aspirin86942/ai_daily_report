//! Count equations common to a finalization batch and its persisted audit.
use super::super::{
    ContextAction, ContextDecision, ContextSummary, ParseStatus, StageMetric, StageName,
};

#[derive(Default)]
pub(crate) struct RelationCounts {
    successes: u64,
    errors: u64,
    timeouts: u64,
    included: u64,
    omitted: u64,
    decision_errors: u64,
    input_chars: u64,
}

impl RelationCounts {
    pub(crate) fn file(&mut self, status: ParseStatus) {
        match status {
            ParseStatus::Success => self.successes += 1,
            ParseStatus::Error => self.errors += 1,
            ParseStatus::Timeout => self.timeouts += 1,
            ParseStatus::NotParsed => {}
        }
    }

    pub(crate) fn decision(&mut self, decision: &ContextDecision) -> Result<(), &'static str> {
        match decision.action {
            ContextAction::Keep | ContextAction::Compress | ContextAction::MetadataOnly => {
                self.included += 1
            }
            ContextAction::Omit => self.omitted += 1,
            ContextAction::Error => self.decision_errors += 1,
        }
        self.input_chars = self
            .input_chars
            .checked_add(decision.input_chars)
            .ok_or("decision input count overflows")?;
        Ok(())
    }

    pub(crate) fn validate(
        &self,
        summary: &ContextSummary,
        text: &str,
        file_count: usize,
        decision_count: usize,
        stages: &[StageMetric],
        duration_overflow: &'static str,
    ) -> Result<(), &'static str> {
        let not_parsed = (file_count as u64)
            .checked_sub(self.successes)
            .and_then(|n| n.checked_sub(self.timeouts))
            .and_then(|n| n.checked_sub(self.errors))
            .ok_or("file status counts overflow")?;
        if summary.source_file_count != file_count as u64
            || file_count != decision_count
            || summary.success_count != self.successes
            || summary.timeout_count != self.timeouts
            || summary.error_file_count != self.errors
            || summary.included_file_count != self.included
            || summary.omitted_file_count != self.omitted
            || self.included != self.successes
            || self.omitted != not_parsed
            || self.decision_errors != self.errors + self.timeouts
            || self.included + self.omitted + self.decision_errors != decision_count as u64
            || summary.input_chars != self.input_chars
            || summary.output_chars != text.chars().count() as u64
        {
            return Err("context summary disagrees with file or decision rows");
        }
        validate_stages(summary, stages, decision_count, duration_overflow)
    }
}

fn validate_stages(
    summary: &ContextSummary,
    stages: &[StageMetric],
    decisions: usize,
    duration_overflow: &'static str,
) -> Result<(), &'static str> {
    let mut seen = std::collections::HashSet::new();
    if stages.len() != 4
        || !stages.iter().all(|metric| {
            seen.insert(metric.stage)
                && match metric.stage {
                    StageName::Discovery => {
                        metric.item_count == summary.source_file_count
                            && metric.duration_ms == summary.discovery_duration_ms
                    }
                    StageName::Cache => metric.item_count == summary.source_file_count,
                    StageName::Parse => metric.duration_ms == summary.parse_duration_ms,
                    StageName::Context => {
                        metric.item_count == decisions as u64
                            && metric.duration_ms == summary.compression_duration_ms
                    }
                }
        })
    {
        return Err("context summary disagrees with stage metrics");
    }
    let duration = stages
        .iter()
        .try_fold(0_u64, |sum, metric| sum.checked_add(metric.duration_ms))
        .ok_or(duration_overflow)?;
    if duration > summary.total_duration_ms {
        return Err("total duration is shorter than its stages");
    }
    Ok(())
}
