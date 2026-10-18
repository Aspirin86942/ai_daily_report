use super::{error, EvidenceRef, Prepared, ReportError, ReportSource};
use crate::{archive::config::ArchiveSettings, snapshots::digest};
use serde_json::json;
use std::collections::BTreeMap;

/// Coalesce adjacent additions in one version and diff hunk. Before/after pairs stay intact.
pub(super) fn coalesce(
    prepared: &mut Prepared,
    settings: &ArchiveSettings,
) -> Result<(), ReportError> {
    if settings.model_group_max_units == 1 {
        return Ok(());
    }
    let mut groups: Vec<(ReportSource, String)> = Vec::new();
    let mut aliases = BTreeMap::new();
    for mut source in std::mem::take(&mut prepared.sources) {
        let text = prepared
            .texts
            .remove(&source.source_id)
            .ok_or_else(|| error("ARCHIVE_EVIDENCE_MISSING"))?;
        let archive = &source.coverage_limits["archive"];
        let addition = archive["kind"] == "added" && archive["comparison_incomplete"] == false;
        // The observation heading is identical within the hunk; keep it once per group.
        let body_start = text.find('\n').map_or(0, |position| position + 1);
        let body = &text[body_start..];
        let prior = groups.last_mut().filter(|(previous, previous_text)| {
            let before = &previous.coverage_limits["archive"];
            let count = before["grouped_source_ids"].as_array().map_or(0, Vec::len);
            addition
                && before["kind"] == "added"
                && before["comparison_incomplete"] == false
                && before["change_id"] == archive["change_id"]
                && before["hunk_index"] == archive["hunk_index"]
                && before["unit_offset"]
                    .as_u64()
                    .unwrap_or(u64::MAX)
                    .checked_add(count as u64)
                    == archive["unit_offset"].as_u64()
                && count < settings.model_group_max_units
                && previous_text.len() + 1 + body.len() <= settings.model_group_max_utf8_bytes
        });
        if let Some((previous, previous_text)) = prior {
            let removed_chars = text[..body_start].chars().count();
            let original_chars = previous_text.chars().count();
            let shift = original_chars + 1;
            let mut references: Vec<EvidenceRef> =
                serde_json::from_value(archive["evidence"].clone())?;
            for reference in &mut references {
                reference.source_id = previous.source_id.clone();
                reference.input_start = reference
                    .input_start
                    .checked_sub(removed_chars)
                    .ok_or_else(|| error("ARCHIVE_EVIDENCE_MISSING"))?
                    + shift;
                reference.input_end = reference
                    .input_end
                    .checked_sub(removed_chars)
                    .ok_or_else(|| error("ARCHIVE_EVIDENCE_MISSING"))?
                    + shift;
            }
            previous_text.push('\n');
            previous_text.push_str(body);
            previous.excerpt_chars = previous_text.chars().count();
            previous.excerpt_sha256 = digest(previous_text.as_bytes());
            let metadata = &mut previous.coverage_limits["archive"];
            metadata["evidence"]
                .as_array_mut()
                .ok_or_else(|| error("ARCHIVE_EVIDENCE_MISSING"))?
                .extend(references.iter().map(|reference| json!(reference)));
            metadata["grouped_source_ids"]
                .as_array_mut()
                .unwrap()
                .push(json!(source.source_id));
            previous.coverage_limits["evidence_audit"]["content_units"]
                .as_array_mut()
                .ok_or_else(|| error("ARCHIVE_EVIDENCE_MISSING"))?
                .push(json!({"text_start":original_chars,"text_end":previous.excerpt_chars,"kind":"paragraph","ranges":[]}));
            aliases.insert(source.source_id, previous.source_id.clone());
            previous.validate()?;
        } else {
            source.coverage_limits["archive"]["grouped_source_ids"] = json!([source.source_id]);
            groups.push((source, text));
        }
    }
    for (source, text) in groups {
        prepared.texts.insert(source.source_id.clone(), text);
        prepared.sources.push(source);
    }
    for gap in &mut prepared.gaps {
        if let Some(replacement) = gap.source_id.as_ref().and_then(|id| aliases.get(id)) {
            gap.source_id = Some(replacement.clone());
        }
    }
    Ok(())
}
