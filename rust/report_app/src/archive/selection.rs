//! Date-window selection; no publication-backlog query and no model calls.
use super::{
    demand::{self, MAX_FILES},
    diff,
    model_input::EvidenceRef,
    store::{ArchiveStore, Version},
};
use crate::{
    config::AppConfig,
    reports::{
        data::{CoverageGap, ReportSource},
        personal::{evidence_text, Material, Selection, MAX_INPUT_CHARS},
    },
    snapshots::digest,
    types::ReportRequest,
    ReportError,
};
use ai_daily_worker_contract::{
    archive::{ArchiveArtifact, ArchiveUnit},
    ContentUnitKind,
};
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
    time::Instant,
};

const MAX_UNITS: usize = 5_000;

struct Candidate {
    version: Version,
    before: Option<(Version, String)>,
    extraction: String,
    artifact: ArchiveArtifact,
    hunks: VecDeque<diff::Hunk>,
    uncertain: bool,
    headers: BTreeMap<String, Vec<(u64, ArchiveUnit)>>,
}

fn gap(selection: &mut Selection, code: &str, path: &str, message: &str) {
    if selection.gaps.len() < 32 {
        let mut gap = CoverageGap::new(code, "parse", None, message);
        gap.relative_path = Some(path.into());
        selection.gaps.push(gap);
    }
}

fn extraction(
    store: &ArchiveStore,
    version: &str,
    profile: &str,
    ready: &str,
) -> Result<Option<(String, String)>, ReportError> {
    Ok(store.conn.query_row(
        "SELECT e.id,e.status FROM version_extractions ve JOIN extractions e ON e.id=ve.extraction_id WHERE ve.version_id=? AND e.profile=? AND e.ready_at<=? AND e.status IN('complete','partial')",
        params![version, profile, ready], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?)
}

pub fn prepare(
    config: &AppConfig,
    request: &ReportRequest,
    deadline: Instant,
) -> Result<Selection, ReportError> {
    prepare_at(config, request, deadline, request.context.started_at)
}

pub(crate) fn prepare_at(
    config: &AppConfig,
    request: &ReportRequest,
    deadline: Instant,
    prepared_at: DateTime<Utc>,
) -> Result<Selection, ReportError> {
    let mut selected = Selection::default();
    let notes = crate::run::input::personal_notes(config, &request.context, &request.input)?;
    if !notes.is_empty() {
        selected.summary.selected_chars = notes.chars().count();
        selected.materials.push(Material {
            source: ReportSource::user(&notes, request.context.started_at),
            text: notes,
            evidence: vec![],
            priority: 0,
        });
    }
    let store = match ArchiveStore::open_collection(config, false) {
        Ok(store) => store,
        Err(e) if e.code == "ARCHIVE_NOT_INITIALIZED" => {
            selected.summary.incomplete_inventory = true;
            gap(
                &mut selected,
                "ARCHIVE_BASELINE_REQUIRED",
                "",
                "尚未建立个人处理区的采集基线。",
            );
            return Ok(selected);
        }
        Err(e) => return Err(e),
    };
    let start = request.context.window_start_utc.to_rfc3339();
    let end = request.context.window_end_utc.to_rfc3339();
    let ready = prepared_at.to_rfc3339();
    let profile = super::parse::profile(config)?;
    let uncaptured: i64 = store.conn.query_row(
        "SELECT count(DISTINCT g.path) FROM gaps g JOIN collections c ON c.id=g.collection_id
         WHERE g.code='CAPTURE_CONTENT_UNAVAILABLE' AND c.started_at>=?1 AND c.started_at<?2
         AND (g.resolved_at IS NULL OR g.resolved_at>=?2)",
        params![start, end],
        |r| r.get(0),
    )?;
    if uncaptured > 0 {
        selected.summary.incomplete_inventory = true;
        gap(&mut selected, "CAPTURE_CONTENT_UNAVAILABLE", "",
            &format!("发现 {uncaptured} 个文件变化，但未取得目标日期内的正文；不能据此判断具体完成事项。"));
    }
    let incomplete: bool = store.conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM metadata WHERE key='baseline') OR EXISTS(SELECT 1 FROM metadata WHERE key='baseline_building' OR key LIKE 'discovery_cursor:%') OR EXISTS(SELECT 1 FROM gaps g JOIN collections c ON c.id=g.collection_id WHERE c.started_at>=?1 AND c.started_at<?2 AND g.resolved_at IS NULL)",
        params![start,end], |r| r.get(0),
    )?;
    selected.summary.incomplete_inventory |= incomplete;
    if incomplete {
        gap(
            &mut selected,
            "COLLECTION_INCOMPLETE",
            "",
            "目标日期的观察或读取存在缺口。",
        );
    }
    let baseline: Option<String> = store
        .conn
        .query_row("SELECT value FROM metadata WHERE key='baseline'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if baseline.as_deref().is_none_or(|at| at > start.as_str()) {
        gap(
            &mut selected,
            "BASELINE_WINDOW_GAP",
            "",
            "观察基线晚于目标日开始时间，较早变化无法确认。",
        );
    }
    if let Some(baseline) = baseline.and_then(|at| DateTime::parse_from_rfc3339(&at).ok()) {
        let from = request
            .context
            .window_start_utc
            .max(baseline.with_timezone(&Utc));
        let until = request.context.window_end_utc.min(prepared_at);
        if from < until {
            let coverage = super::schedule::coverage(&store, from, until, prepared_at)?;
            if !coverage.planned_idle && (!coverage.completed || coverage.gaps > 0) {
                selected.summary.incomplete_inventory = true;
                gap(
                    &mut selected,
                    "ARCHIVE_COLLECTION_GAP",
                    "",
                    "目标日期存在缺少已完成采集的时间，无法确认这些时间内是否有变化。",
                );
            }
        }
    }
    let count: i64 = store.conn.query_row("SELECT count(DISTINCT document_id) FROM versions WHERE observed_at>=? AND observed_at<? AND baseline=0", params![start,end], |r| r.get(0))?;
    selected.summary.observed_files = count as u64;
    let mut candidates = Vec::new();
    let mut cursor = String::new();
    'pages: loop {
        if Instant::now() >= deadline || selected.summary.inspected_files >= MAX_FILES as u64 {
            break;
        }
        let ids = demand::pairs(&store, &start, &end, &cursor)?;
        if ids.is_empty() {
            break;
        }
        for pair in ids {
            cursor = pair.document;
            let id = pair.current;
            if Instant::now() >= deadline || selected.summary.inspected_files >= MAX_FILES as u64 {
                break 'pages;
            }
            selected.summary.inspected_files += 1;
            let version = match store.version(&id) {
                Ok(version) => version,
                Err(_) => {
                    selected.summary.omit("read_failed", &id);
                    gap(
                        &mut selected,
                        "CURRENT_CONTENT_UNAVAILABLE",
                        "",
                        "部分目标日材料已经过期或无法读取。",
                    );
                    continue;
                }
            };
            if !demand::eligible(config, &version.path) {
                selected.summary.omit("out_of_scope", &version.path);
                continue;
            }
            let Some((after, status)) = extraction(&store, &id, &profile, &ready)? else {
                selected.summary.omit("read_failed", &version.path);
                gap(
                    &mut selected,
                    "PARSE_UNAVAILABLE",
                    &version.path,
                    "目标日最后版本尚无可用解析。",
                );
                continue;
            };
            let before = match pair.previous {
                Some(id) => match (
                    store.version(&id),
                    extraction(&store, &id, &profile, &ready)?,
                ) {
                    (Ok(v), Some((e, _))) => Some((v, e)),
                    _ => None,
                },
                None => None,
            };
            let artifact: ArchiveArtifact = serde_json::from_value(store.read_manifest(&after)?)?;
            let old_partial = before
                .as_ref()
                .map(|(_, e)| {
                    store.read_manifest(e).map(|v| {
                        v["truncated"] == true
                            || v["gaps"].as_array().is_some_and(|g| !g.is_empty())
                    })
                })
                .transpose()?
                .unwrap_or(true);
            let mut uncertain = before.is_none() || status != "complete" || old_partial;
            if before.is_none() {
                selected
                    .summary
                    .omit("comparison_unavailable", &version.path);
                gap(
                    &mut selected,
                    "PREVIOUS_CONTENT_UNAVAILABLE",
                    &version.path,
                    "缺少目标日前的可用版本，本次读取不能认定为新增成果。",
                );
            } else if uncertain {
                gap(
                    &mut selected,
                    "COMPARISON_PARTIAL",
                    &version.path,
                    "部分版本解析不完整，比较仅覆盖已取得的内容。",
                );
            }
            let changes = match diff::compare_bounded(
                &store,
                before.as_ref().map(|(_, e)| e.as_str()),
                &after,
                &config.archive.table_keys,
                deadline,
                MAX_UNITS,
            ) {
                Ok(changes) => changes,
                Err(_) => {
                    selected.summary.omit("read_failed", &version.path);
                    gap(
                        &mut selected,
                        "COMPARISON_UNAVAILABLE",
                        &version.path,
                        "未能在预算内完成版本比较。",
                    );
                    continue;
                }
            };
            if changes.granularity == "bounded_unit" || artifact.units > MAX_UNITS as u64 {
                uncertain = true;
                gap(
                    &mut selected,
                    "COMPARISON_BOUNDED",
                    &version.path,
                    "每侧最多比较 5,000 个单元，其余范围未展开。",
                );
            }
            let mut hunks = VecDeque::new();
            for h in changes.hunks {
                let left = h.before.end - h.before.start;
                let right = h.after.end - h.after.start;
                // Equal-sized paired rows remain indivisible; unmatched regions remain whole.
                if matches!(
                    version.extension.as_str(),
                    ".csv" | ".tsv" | ".xlsx" | ".xls" | "csv" | "tsv" | "xlsx" | "xls"
                ) && (left == right || left == 0 || right == 0)
                {
                    for index in 0..left.max(right) {
                        hunks.push_back(diff::Hunk {
                            before: diff::Span {
                                start: h.before.start + index.min(left),
                                end: h.before.start + (index + 1).min(left),
                            },
                            after: diff::Span {
                                start: h.after.start + index.min(right),
                                end: h.after.start + (index + 1).min(right),
                            },
                        });
                    }
                } else {
                    hunks.push_back(h);
                }
            }
            if !hunks.is_empty() {
                candidates.push(Candidate {
                    version,
                    before,
                    extraction: after,
                    artifact,
                    hunks,
                    uncertain,
                    headers: BTreeMap::new(),
                });
            }
        }
    }
    candidates.sort_by(|a, b| {
        b.version
            .observed_at
            .cmp(&a.version.observed_at)
            .then(a.version.path.cmp(&b.version.path))
    });
    let mut seen = BTreeSet::new();
    let mut inspected_units = 0;
    let mut active = true;
    while active && Instant::now() < deadline && inspected_units < 10_000 {
        active = false;
        for candidate in &mut candidates {
            if Instant::now() >= deadline || inspected_units >= 10_000 {
                break;
            }
            let Some(hunk) = candidate.hunks.pop_front() else {
                continue;
            };
            active = true;
            inspected_units += 1;
            match material(&store, config, candidate, &hunk, deadline) {
                Ok(material) => {
                    let chars = material.text.chars().count();
                    if !seen.insert(material.source.excerpt_sha256.clone()) {
                        selected.summary.omit("duplicate", &candidate.version.path);
                        continue;
                    }
                    if chars > MAX_INPUT_CHARS {
                        selected.summary.omit("budget", &candidate.version.path);
                        continue;
                    }
                    // Keep bounded bodies in memory, while allowing later high-priority
                    // material to replace lower-priority material already admitted.
                    while selected.summary.selected_chars + chars > MAX_INPUT_CHARS {
                        let victim = selected
                            .materials
                            .iter()
                            .enumerate()
                            .filter(|(_, m)| m.priority > material.priority)
                            .max_by_key(|(i, m)| (m.priority, *i))
                            .map(|(i, _)| i);
                        let Some(index) = victim else {
                            break;
                        };
                        let removed = selected.materials.remove(index);
                        selected.summary.selected_chars -= removed.text.chars().count();
                        selected.summary.omit(
                            "budget",
                            removed.source.relative_path.as_deref().unwrap_or(""),
                        );
                    }
                    if selected.summary.selected_chars + material.text.chars().count()
                        > MAX_INPUT_CHARS
                    {
                        selected.summary.omit("budget", &candidate.version.path);
                    } else {
                        selected.summary.selected_chars += material.text.chars().count();
                        selected.materials.push(material);
                    }
                }
                Err(_) => {
                    selected
                        .summary
                        .omit("read_failed", &candidate.version.path);
                    gap(
                        &mut selected,
                        "MATERIAL_UNAVAILABLE",
                        &candidate.version.path,
                        "部分完整材料单元无法读取或超出预算。",
                    );
                }
            }
        }
    }
    let pending: usize = candidates.iter().map(|c| c.hunks.len()).sum();
    if pending > 0 {
        *selected
            .summary
            .omissions
            .entry("budget".into())
            .or_default() += pending as u64;
    }
    if selected.summary.inspected_files < selected.summary.observed_files {
        *selected
            .summary
            .omissions
            .entry("budget".into())
            .or_default() += selected.summary.observed_files - selected.summary.inspected_files;
    }
    selected.summary.selected_files = selected
        .materials
        .iter()
        .filter_map(|m| m.source.relative_path.as_ref())
        .collect::<BTreeSet<_>>()
        .len() as u64;
    selected.materials.sort_by_key(|m| m.priority);
    selected.validate()?;
    Ok(selected)
}

fn material(
    store: &ArchiveStore,
    config: &AppConfig,
    candidate: &mut Candidate,
    hunk: &diff::Hunk,
    deadline: Instant,
) -> Result<Material, ReportError> {
    let source_id = format!(
        "src_{}",
        super::identity(&[
            &candidate.version.id,
            &candidate.extraction,
            &hunk.after.start.to_string(),
            &hunk.before.start.to_string()
        ])
    );
    let mut evidence = Vec::new();
    for (version, extraction, span, side) in candidate
        .before
        .as_ref()
        .map(|(v, e)| (v, e, &hunk.before, "before"))
        .into_iter()
        .chain(std::iter::once((
            &candidate.version,
            &candidate.extraction,
            &hunk.after,
            if candidate.before.is_some() {
                "after"
            } else {
                "current"
            },
        )))
    {
        let dir = store.extraction_dir(extraction)?;
        let mut units = Vec::new();
        if !candidate.headers.contains_key(extraction) {
            let artifact: ArchiveArtifact =
                serde_json::from_value(store.read_manifest(extraction)?)?;
            let mut headers = Vec::new();
            for index in 1..=artifact.units.min(MAX_UNITS as u64) {
                crate::processing_cache::check_deadline(deadline)?;
                let unit = diff::unit(&dir, index)?;
                if matches!(
                    unit.kind,
                    ContentUnitKind::TableHeader | ContentUnitKind::Heading | ContentUnitKind::Unit
                ) {
                    headers.push((index, unit));
                }
            }
            candidate.headers.insert(extraction.clone(), headers);
        }
        let headers = &candidate.headers[extraction];
        for index in span.start..span.end.min(MAX_UNITS as u64 + 1) {
            crate::processing_cache::check_deadline(deadline)?;
            let unit = diff::unit(&dir, index)?;
            for kind in [
                ContentUnitKind::Heading,
                ContentUnitKind::Unit,
                ContentUnitKind::TableHeader,
            ] {
                if kind != ContentUnitKind::TableHeader
                    || matches!(
                        unit.kind,
                        ContentUnitKind::TableRow | ContentUnitKind::Total
                    )
                {
                    let label = unit
                        .ranges
                        .first()
                        .and_then(|r| r.label.clone())
                        .unwrap_or_default();
                    if let Some((_, header)) = headers.iter().rev().find(|(i, h)| {
                        *i < index
                            && h.kind == kind
                            && (kind != ContentUnitKind::TableHeader
                                || h.ranges
                                    .first()
                                    .and_then(|r| r.label.as_deref())
                                    .unwrap_or("")
                                    == label)
                    }) {
                        if !units
                            .iter()
                            .any(|u: &ArchiveUnit| u.unit_id == header.unit_id)
                        {
                            units.push(header.clone());
                        }
                    }
                }
            }
            units.push(unit);
        }
        for unit in units {
            let quote = diff::text(&dir, &unit)?;
            let offset = evidence
                .last()
                .map(|r: &EvidenceRef| r.input_end + 2)
                .unwrap_or(0);
            if offset + quote.chars().count() > MAX_INPUT_CHARS {
                return Err(super::error("MATERIAL_TOO_LARGE"));
            }
            evidence.push(EvidenceRef {
                source_id: source_id.clone(),
                version_id: version.id.clone(),
                extraction_id: extraction.clone(),
                unit_id: unit.unit_id,
                side: side.into(),
                raw_sha256: version.sha256.clone(),
                unit_sha256: unit.sha256,
                quote_sha256: digest(quote.as_bytes()),
                input_start: offset,
                input_end: offset + quote.chars().count(),
                unit_char_start: 0,
                quote,
                ranges: unit.ranges,
            });
        }
    }
    let text = evidence_text(&evidence);
    if text.is_empty() {
        return Err(super::error("MATERIAL_EMPTY"));
    }
    let path = Path::new(&candidate.version.path);
    let root = std::fs::canonicalize(&config.paths.work_dir)?;
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let relative = canonical
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| {
            format!(
                "additional/{}",
                path.file_name().unwrap_or_default().to_string_lossy()
            )
        });
    let mut source = ReportSource::user(
        &text,
        DateTime::parse_from_rfc3339(&candidate.version.observed_at)
            .map_err(|_| super::error("ARCHIVE_TIME_INVALID"))?
            .with_timezone(&Utc),
    );
    source.source_id = source_id;
    source.source_kind = "file".into();
    source.relative_path = Some(relative);
    source.extension = Some(candidate.version.extension.clone());
    source.size_bytes = candidate.version.size_bytes;
    source.content_sha256 = Some(candidate.version.sha256.clone());
    source.modified_at_utc = Some(candidate.version.modified_at.clone());
    source.parser_backend = Some(candidate.artifact.parser_backend.clone());
    source.worker_lane = Some(candidate.artifact.worker_lane.clone());
    source.parse_truncated = candidate.artifact.truncated;
    source.coverage_limits["evidence_audit"]["text_unverified"] = json!(candidate.uncertain);
    source.coverage_limits["personal"] = json!({"absolute_path":candidate.version.path});
    let priority = if ["截止", "待审批", "阻塞", "延期", "待确认"]
        .iter()
        .any(|s| text.contains(s))
    {
        1
    } else if candidate.before.is_some() {
        2
    } else {
        3
    };
    Ok(Material {
        source,
        text,
        evidence,
        priority,
    })
}
