//! Adapt the native scanner's bounded semantic excerpts to the personal report.
//! Scan identities describe a frozen observation, never an archive version.
use super::{input, App};
use crate::{
    archive::model_input::{permitted, EvidenceRef},
    reports::{
        data::ReportSource,
        personal::{Material, Selection, MAX_INPUT_CHARS},
    },
    snapshots::digest,
    types::ReportRequest,
    ReportError,
};
use ai_daily_scanner_contract::EngineStatus;
use serde_json::json;
use std::collections::BTreeMap;

pub fn prepare(app: &App, request: &ReportRequest) -> Result<Selection, ReportError> {
    let mut selection = Selection::default();
    let notes = input::personal_notes(&app.config, &request.context, &request.input)?;
    if !notes.is_empty() {
        selection.summary.selected_chars = notes.chars().count();
        selection.materials.push(Material {
            source: ReportSource::user(&notes, request.context.started_at),
            text: notes,
            evidence: vec![],
            priority: 0,
        });
    }
    let scan = app.scan(request)?;
    let (sources, gaps) = input::scan_sources(&scan)?;
    selection.gaps = gaps;
    selection.summary.incomplete_inventory = scan.envelope.status != EngineStatus::Ok;
    selection.summary.observed_files = sources.len() as u64;
    let raw: BTreeMap<_, _> = scan
        .sources
        .iter()
        .map(|s| (s.source_id.as_str(), s))
        .collect();
    for mut source in sources {
        selection.summary.inspected_files += 1;
        let path = source
            .relative_path
            .as_deref()
            .unwrap_or_default()
            .to_owned();
        let absolute = app.config.paths.work_dir.join(&path);
        if !permitted(&app.config, &absolute) {
            selection.summary.omit("out_of_scope", &path);
            continue;
        }
        if !source.usable() {
            selection.summary.omit(
                if source.parse_status == "success" {
                    "no_usable_text"
                } else {
                    "read_failed"
                },
                &path,
            );
            continue;
        }
        let original = raw[source.source_id.as_str()];
        let text = original.excerpt.clone();
        let chars = text.chars().count();
        if selection.summary.selected_chars + chars > MAX_INPUT_CHARS {
            // Keep the complete scanner excerpt; never cut a sentence or table row here.
            selection.summary.omit("budget", &path);
            continue;
        }
        let hash = source
            .content_sha256
            .clone()
            .ok_or_else(|| ReportError::new("SCAN_SOURCE_INVALID", "扫描来源缺少文件内容指纹"))?;
        let quote_sha256 = digest(text.as_bytes());
        let ranges = original.coverage_limits["evidence_audit"]["excerpt_ranges"].clone();
        let ranges = if ranges.is_null() {
            vec![]
        } else {
            serde_json::from_value(ranges)?
        };
        source.coverage_limits["personal"] = json!({
            "absolute_path":absolute, "input_mode":"scan", "selection_basis":"modified_at",
        });
        let evidence = vec![EvidenceRef {
            source_id: source.source_id.clone(),
            version_id: format!("scan-content:{hash}"),
            extraction_id: format!("scan:{}:{}", request.context.run_id, source.source_id),
            unit_id: format!("scan-excerpt:{quote_sha256}"),
            side: "current".into(),
            raw_sha256: hash,
            unit_sha256: quote_sha256.clone(),
            quote: text.clone(),
            quote_sha256,
            unit_char_start: 0,
            input_start: 0,
            input_end: chars,
            ranges,
        }];
        selection.summary.selected_files += 1;
        selection.summary.selected_chars += chars;
        selection.materials.push(Material {
            source,
            text,
            evidence,
            priority: 1,
        });
    }
    selection.validate()?;
    Ok(selection)
}
