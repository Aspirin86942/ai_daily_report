use super::*;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSummary {
    pub source_file_count: u64,
    pub success_count: u64,
    pub timeout_count: u64,
    pub included_file_count: u64,
    pub omitted_file_count: u64,
    pub error_file_count: u64,
    pub input_chars: u64,
    pub output_chars: u64,
    pub total_duration_ms: u64,
    pub discovery_duration_ms: u64,
    pub parse_duration_ms: u64,
    pub compression_duration_ms: u64,
}

impl Validate for ContextSummary {
    fn validate(&self) -> Result<(), String> {
        let classified = self
            .success_count
            .checked_add(self.timeout_count)
            .and_then(|value| value.checked_add(self.error_file_count))
            .ok_or_else(|| "summary counts overflow".to_string())?;
        if classified <= self.source_file_count {
            Ok(())
        } else {
            Err("classified file counts exceed source_file_count".to_string())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineStatus {
    Ok,
    Partial,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReuseKind {
    ContextSnapshot,
    ParseCache,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefillRoundAudit {
    pub round: u64,
    pub planned_source_count: u64,
    pub plan_sha256: String,
    pub candidate_slots_used: u64,
    pub classification_pages_held: u64,
    pub extraction_slots_used: u64,
    pub reserved_chars: u64,
    pub refunded_chars: u64,
    pub refunded_classification_pages: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefillAudit {
    pub policy_version: String,
    pub stop_reason: String,
    pub max_rounds: u64,
    pub candidate_cap: u64,
    pub classification_page_cap: u64,
    pub extraction_cap: u64,
    pub context_char_cap: u64,
    pub candidate_slots_used: u64,
    pub classification_pages_held: u64,
    pub extraction_slots_used: u64,
    pub refunded_chars: u64,
    pub refunded_classification_pages: u64,
    pub rounds: Vec<RefillRoundAudit>,
}

impl Validate for RefillAudit {
    fn validate(&self) -> Result<(), String> {
        require_const(
            &self.policy_version,
            "budget_refill_v3",
            "refill.policy_version",
        )?;
        if !matches!(
            self.stop_reason.as_str(),
            "complete" | "quota_exhausted" | "no_progress" | "deadline" | "round_limit"
        ) {
            return Err("invalid refill stop reason".into());
        }
        if self.max_rounds != 2 || self.rounds.len() > 2 {
            return Err("refill may freeze at most two supplemental rounds".into());
        }
        if self.candidate_slots_used > self.candidate_cap
            || self.classification_pages_held > self.classification_page_cap
            || self.extraction_slots_used > self.extraction_cap
        {
            return Err("refill consumption exceeds the original run quota".into());
        }
        for (index, round) in self.rounds.iter().enumerate() {
            if round.round != index as u64 + 1
                || round.planned_source_count == 0
                || round.plan_sha256.len() != 64
                || !round
                    .plan_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || round.candidate_slots_used > self.candidate_cap
                || round.classification_pages_held > self.classification_page_cap
                || round.extraction_slots_used > self.extraction_cap
                || round.reserved_chars > self.context_char_cap
            {
                return Err("invalid frozen refill round".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionMetricsV2 {
    /// Absent in legacy runs; new runs record confirmed quota reclamation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refill_audit: Option<RefillAudit>,
    pub discovery_observed_file_count: u64,
    pub source_guard_content_hash_file_count: u64,
    pub source_guard_unavailable_count: u64,
    pub source_guard_bytes_read: u64,
    pub candidate_file_count: u64,
    pub admitted_file_count: u64,
    pub classification_slot_count: u64,
    pub confirmed_run_inspected_pages_total: u64,
    pub unobserved_classification_attempt_count: u64,
    pub nominal_charged_pages_total: u64,
    pub extraction_slot_count: u64,
    pub pdfplumber_invocations: u64,
    pub snapshot_hit: bool,
    pub parse_cache_lookup_count: u64,
    pub classification_cache_lookup_count: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub parse_cache_all_hit: Nullable<bool>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub classification_cache_all_hit: Nullable<bool>,
    pub stage_deadline_exhausted_count: u64,
    pub session_restart_count: u64,
    pub session_fallback_count: u64,
    pub classify_attempt_count: u64,
    pub parse_attempt_count: u64,
    pub reserved_chars: u64,
    pub rendered_chars: u64,
    pub worker_handshake_ms: u64,
    pub discovery_ms: u64,
    pub snapshot_lookup_ms: u64,
    pub current_run_audit_write_ms: u64,
    pub terminal_precommit_ms: u64,
    pub deadline_precommit_elapsed_ms: u64,
    pub envelope_rebuild_ms: u64,
    pub terminal_rows_written: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub peak_worker_rss_bytes: Nullable<u64>,
}

impl Validate for ExecutionMetricsV2 {
    fn validate(&self) -> Result<(), String> {
        if let Some(refill) = &self.refill_audit {
            refill.validate()?;
        }
        require_range(
            self.stage_deadline_exhausted_count,
            0,
            1,
            "stage_deadline_exhausted_count",
        )?;
        for (kind, lookups, all_hit) in [
            (
                "parse",
                self.parse_cache_lookup_count,
                self.parse_cache_all_hit.0,
            ),
            (
                "classification",
                self.classification_cache_lookup_count,
                self.classification_cache_all_hit.0,
            ),
        ] {
            match (lookups, all_hit) {
                (0, Some(_)) => {
                    return Err(format!(
                        "{kind}_cache_all_hit must be null when no {kind} lookup occurred"
                    ))
                }
                (1.., None) => {
                    return Err(format!(
                        "{kind}_cache_all_hit is required after a {kind} lookup"
                    ))
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScannerEvidence {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub scan_run_id: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub context_run_id: Nullable<u64>,
    pub run_status: RunStatus,
    pub summary: ContextSummary,
    pub stage_metrics: Vec<StageMetric>,
    pub extension_metrics: Vec<ExtensionMetric>,
    pub files: Vec<FileAuditV2>,
    pub decisions: Vec<ContextDecision>,
    pub warnings: Vec<Diagnostic>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub artifact_id: Nullable<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reused_from_context_run_id: Nullable<u64>,
    pub reuse_kind: ReuseKind,
    pub execution_metrics: ExecutionMetricsV2,
}

impl Validate for ScannerEvidence {
    fn validate(&self) -> Result<(), String> {
        require_const(&self.contract, "ai_daily_context", "contract")?;
        require_range(self.protocol_version, 1, 1, "protocol_version")?;
        require_request_id(&self.request_id)?;
        require_range(self.scan_run_id, 1, u64::MAX, "scan_run_id")?;
        for (field, id) in [
            ("context_run_id", self.context_run_id.0),
            ("artifact_id", self.artifact_id.0),
            (
                "reused_from_context_run_id",
                self.reused_from_context_run_id.0,
            ),
        ] {
            if let Some(id) = id {
                require_range(id, 1, u64::MAX, field)?;
            }
        }
        self.summary.validate()?;
        if self.stage_metrics.len() > 32
            || self.extension_metrics.len() > 256
            || self.files.len() > 1_000_000
            || self.decisions.len() > 1_000_000
            || self.warnings.len() > 100_000
        {
            return Err("scanner evidence contains too many items".to_string());
        }
        for metric in &self.stage_metrics {
            metric.validate()?;
        }
        for metric in &self.extension_metrics {
            metric.validate()?;
        }
        for file in &self.files {
            file.validate()?;
        }
        for decision in &self.decisions {
            decision.validate()?;
        }
        for warning in &self.warnings {
            warning.validate()?;
        }
        self.execution_metrics.validate()?;
        match self.run_status {
            RunStatus::Success | RunStatus::Partial if self.artifact_id.0.is_none() => {
                return Err("successful scanner evidence requires artifact_id".to_string());
            }
            RunStatus::Error if self.artifact_id.0.is_some() => {
                return Err("error scanner evidence must have a null artifact_id".to_string());
            }
            _ => {}
        }
        if self.reuse_kind == ReuseKind::ContextSnapshot
            && self.reused_from_context_run_id.0.is_none()
        {
            return Err("context_snapshot reuse requires reused_from_context_run_id".to_string());
        }
        if self.reuse_kind == ReuseKind::ContextSnapshot && !self.execution_metrics.snapshot_hit {
            return Err("context_snapshot reuse requires snapshot_hit".to_string());
        }
        if self.reuse_kind != ReuseKind::ContextSnapshot
            && self.reused_from_context_run_id.0.is_some()
        {
            return Err(
                "reused_from_context_run_id is only allowed for context_snapshot reuse".to_string(),
            );
        }
        if self.reuse_kind == ReuseKind::ParseCache
            && (self.execution_metrics.parse_cache_lookup_count == 0
                || self.execution_metrics.parse_cache_all_hit.0 != Some(true)
                || self.execution_metrics.snapshot_hit)
        {
            return Err(
                "parse_cache reuse requires a parse lookup all-hit and no snapshot".to_string(),
            );
        }
        Ok(())
    }
}
