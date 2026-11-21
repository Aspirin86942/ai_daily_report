use super::*;

impl ActiveBuild<'_> {
    /// Spec Part 5.4 snapshot-hit finalization: the current run reuses the selected
    /// artifact (reference + `snapshot_hit`/`reused_from` written by the store),
    /// current rows are rebuilt from artifact rows with `snapshot` semantics
    /// (`parse_cache_status=snapshot`, `cache_miss_reason=''`, `parse_duration_ms=0`),
    /// and the current summary durations are THIS run's measured values — never the
    /// source run's old timings.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn finalize_snapshot_hit(
        &mut self,
        discovery: &DiscoveryReport,
        inventory: Vec<InventoryRecord>,
        context_profile_hash: &str,
        classifier_identity: &ClassifierIdentity,
        hit: SnapshotHit,
        artifact: ArtifactDraft,
        mut pages: std::collections::BTreeMap<String, ai_daily_scanner_contract::PdfPageAudit>,
        discovery_duration_ms: u64,
        inventory_prepare_duration_ms: u64,
        snapshot_lookup_ms: u64,
    ) -> ScannerOperation<ContextEnvelope> {
        let mut file_results = snapshot_file_results(&artifact, Some(classifier_identity));
        for record in &mut file_results {
            if let Some(audit) = record.pdf_classification.as_mut() {
                audit.page_audit = pages.remove(&record.file_identity);
            }
        }
        let decisions: Vec<ContextDecisionRecord> = artifact
            .decision_rows
            .iter()
            .map(|row| ContextDecisionRecord {
                file_identity: row.file_identity.clone(),
                decision: ContextDecision {
                    relative_path: row.relative_path.clone(),
                    action: row.action,
                    reason: row.reason.clone(),
                    priority: row.priority,
                    input_chars: row.input_chars,
                    output_chars: row.output_chars,
                    truncated: row.truncated,
                    error_code: row.error_code.clone(),
                },
            })
            .collect();
        let compression_duration_ms = snapshot_lookup_ms;
        let summary = ContextSummary {
            source_file_count: artifact.semantic_summary.source_file_count,
            success_count: artifact.semantic_summary.success_count,
            timeout_count: artifact.semantic_summary.timeout_count,
            included_file_count: artifact.semantic_summary.included_file_count,
            omitted_file_count: artifact.semantic_summary.omitted_file_count,
            error_file_count: artifact.semantic_summary.error_file_count,
            input_chars: artifact.semantic_summary.input_chars,
            output_chars: artifact.semantic_summary.output_chars,
            total_duration_ms: elapsed_ms(self.started_at),
            discovery_duration_ms,
            parse_duration_ms: 0,
            compression_duration_ms,
        };
        let stage_metrics = vec![
            StageMetric {
                stage: StageName::Discovery,
                item_count: discovery.files.len() as u64,
                duration_ms: discovery_duration_ms,
            },
            StageMetric {
                stage: StageName::Cache,
                item_count: discovery.files.len() as u64,
                duration_ms: inventory_prepare_duration_ms.saturating_add(snapshot_lookup_ms),
            },
            StageMetric {
                stage: StageName::Parse,
                item_count: 0,
                duration_ms: 0,
            },
            StageMetric {
                stage: StageName::Context,
                item_count: decisions.len() as u64,
                duration_ms: compression_duration_ms,
            },
        ];
        let extension_metrics =
            match crate::context_audit::extension_metrics(&inventory, &file_results) {
                Ok(metrics) => metrics,
                Err(message) => {
                    self.heartbeat.stop();
                    return self.run.error_output(
                        internal_error(message, DiagnosticStage::Internal),
                        Vec::new(),
                        ContextSummary::default(),
                    );
                }
            };
        let peak_worker_rss_bytes = self.rss_tracker.peak_worker_rss_bytes();
        let warnings: Vec<Diagnostic> = peak_worker_rss_bytes
            .is_none()
            .then(worker_rss_unavailable_diagnostic)
            .into_iter()
            .collect();
        let envelope = ContextEnvelope {
            contract: "ai_daily_context".to_string(),
            protocol_version: 1,
            request_id: self.run.request.request_id.clone(),
            engine_version: self.run.version.engine_version.clone(),
            engine_build: self.run.version.engine_build.clone(),
            status: EngineStatus::Ok,
            file_context: artifact.final_context.clone(),
            summary: summary.clone(),
            scan_run_id: Nullable(Some(self.run.active.scan_run_id())),
            context_run_id: Nullable(Some(self.run.active.context_run_id())),
            warnings: warnings.clone(),
            error: Nullable(None),
        };
        let envelope_json = match canonical_envelope_json(&envelope) {
            Ok(json) => json,
            Err(error) => {
                self.heartbeat.stop();
                return self.run.error_output(
                    error.diagnostic(DiagnosticStage::Internal),
                    Vec::new(),
                    summary,
                );
            }
        };
        let context = ContextRunRecord {
            context_profile_hash: context_profile_hash.to_string(),
            status: RunStatus::Success,
            final_context: artifact.final_context.clone(),
            context_sha256: artifact.context_sha256.clone(),
            summary,
            decisions,
        };
        let batch = FinalizationBatch {
            status: RunStatus::Success,
            envelope_json,
            inventory,
            file_results,
            diagnostics: warnings
                .into_iter()
                .map(persistence::warning_record)
                .collect(),
            stage_metrics,
            extension_metrics,
            context: Some(context),
            artifact: None,
            snapshot_key: None,
            snapshot_hit: Some(SnapshotHitRef {
                artifact_id: hit.artifact_id,
                reused_from_context_run_id: hit.source_context_run_id,
            }),
            execution_metrics: Some(assemble_snapshot_execution_metrics(
                discovery.files.len() as u64,
                self.run.timing.source_guards.metrics(),
                artifact.semantic_summary.reserved_chars,
                artifact.semantic_summary.rendered_chars,
                self.handshake_ms,
                discovery_duration_ms,
                snapshot_lookup_ms,
                elapsed_ms(self.started_at),
                peak_worker_rss_bytes,
            )),
        };
        self.heartbeat.stop();
        self.commit_outcome(&batch, envelope, Vec::new(), discovery_duration_ms)
    }
}
