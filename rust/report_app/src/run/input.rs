use super::{hash, App};
use crate::{
    config::ModelPricing,
    model::{
        plan::{self, DailyPlan},
        recovery::RecoveryInputs,
        slicing::{union_chars, TextRange},
        StepResult,
    },
    reports::{
        data::{CoverageGap, ReportSource},
        DAILY_TEMPLATE_V3,
    },
    snapshots::reject_redirects,
    types::{ReportRequest, RunContext},
    ReportError,
};
use ai_daily_scanner_contract::EngineStatus;
use ai_daily_scanner_core::ContextResult;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenInput {
    pub version: u32,
    pub context: RunContext,
    pub quality: String,
    pub sources: Vec<ReportSource>,
    pub gaps: Vec<CoverageGap>,
    pub plan: DailyPlan,
    pub template: String,
    pub root_alias: String,
    pub notes: String,
    pub missed_dates: Vec<String>,
    pub input_fingerprint: String,
    pub recovery_fingerprint: String,
    pub model_pricing: Option<ModelPricing>,
    pub recovery_inputs: Option<RecoveryInputs>,
}

impl FrozenInput {
    pub fn publication_fingerprint(&self) -> Result<String, ReportError> {
        hash(
            &json!({"input":self.input_fingerprint,"template":self.template,"notes":self.notes,"missed_dates":self.missed_dates,"root_alias":self.root_alias}),
        )
    }
}

fn source_gap(source: &ReportSource, code: &str, stage: &str, message: &str) -> CoverageGap {
    CoverageGap::new(code, stage, Some(&source.source_id), message)
}

fn scan_sources(
    scan: &ContextResult,
) -> Result<(Vec<ReportSource>, Vec<CoverageGap>), ReportError> {
    let mut sources = Vec::new();
    let mut gaps = Vec::new();
    for warning in &scan.envelope.warnings {
        let code = serde_json::to_value(warning.error_code)?
            .as_str()
            .unwrap()
            .to_owned();
        if code != "WORKER_RSS_UNAVAILABLE" {
            gaps.push(CoverageGap::new(
                &code,
                "discovery",
                None,
                "扫描存在未覆盖的范围，请结合其他材料确认。",
            ));
        }
    }
    if scan.envelope.status == EngineStatus::Error {
        gaps.push(CoverageGap::new(
            "SCAN_UNAVAILABLE",
            "discovery",
            None,
            "扫描未能完成，文件内容覆盖无法确认。",
        ));
    } else if scan.envelope.status == EngineStatus::Partial && gaps.is_empty() {
        gaps.push(CoverageGap::new(
            "SCAN_PARTIAL",
            "discovery",
            None,
            "扫描存在未完成的采集范围。",
        ));
    }
    let native: BTreeMap<_, _> = scan
        .evidence
        .as_ref()
        .map(|e| {
            e.files
                .iter()
                .map(|f| (f.relative_path.replace('\\', "/"), f))
                .collect()
        })
        .unwrap_or_default();
    for raw in &scan.sources {
        let mut source = ReportSource::from_scan(raw)?;
        let mut audit = source.coverage_limits["evidence_audit"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let mut tags: BTreeSet<String> = audit
            .get("secondary_tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let has_hash = source.content_sha256.is_some();
        let status = if source.parse_status == "not_parsed" {
            "not_parsed"
        } else if source.parse_status != "success" || source.excerpt_chars > 0 && !has_hash {
            "parse_failed"
        } else if source.excerpt_chars == 0 || audit.get("usable_chars") == Some(&json!(0)) {
            "empty_excerpt"
        } else {
            "usable_excerpt"
        };
        audit.insert("primary_status".into(), json!(status));
        if status == "parse_failed" || status == "not_parsed" {
            tags.insert("SOURCE_UNAVAILABLE".into());
            gaps.push(source_gap(
                &source,
                "SOURCE_UNAVAILABLE",
                "parse",
                "文件未能成功读取或无法确认其稳定版本。",
            ));
        }
        if source.parse_truncated || source.extension.as_deref() == Some(".csv") {
            tags.insert("PARSE_BOUNDED".into());
            gaps.push(source_gap(
                &source,
                "PARSE_BOUNDED",
                "parse",
                "只提取了限定页数、行数或字符范围，结论限于已读取部分。",
            ));
        }
        if source.excerpt_truncated || status == "empty_excerpt" {
            tags.insert("EXCERPT_BOUNDED".into());
            gaps.push(source_gap(
                &source,
                "EXCERPT_BOUNDED",
                "excerpt",
                "此来源的摘录未覆盖全部内容，或没有取得有效文字。",
            ));
        }
        if source.unverified() {
            tags.insert("OCR_TEXT_UNVERIFIED".into());
            audit.insert("text_unverified".into(), json!(true));
            gaps.push(source_gap(
                &source,
                "OCR_TEXT_UNVERIFIED",
                "parse",
                "识别文字尚未核实，数字和表格对应关系需对照原文件确认。",
            ));
        }
        if let Some(history) = source.coverage_limits.get("history") {
            if history["prior_content_sha256"].is_null() {
                gaps.push(source_gap(
                    &source,
                    "HISTORICAL_CONTENT_UNKNOWN",
                    "parse",
                    "旧报告缺少原文件内容哈希，本次依据冻结时可取得的版本。",
                ));
            } else if history["current_version_changed"] == true {
                gaps.push(source_gap(
                    &source,
                    "HISTORICAL_CONTENT_CHANGED",
                    "parse",
                    "当前文件与旧报告记录的内容不同，无法据此还原历史版本。",
                ));
            }
        }
        if let Some(file) = native.get(source.relative_path.as_deref().unwrap_or("")) {
            if let Some(classification) = &file.pdf_classification.0 {
                let classification = serde_json::to_value(classification)?;
                if !classification["page_audit"].is_null() {
                    audit.insert(
                        "pdf_page_audit".into(),
                        classification["page_audit"].clone(),
                    );
                    if classification["page_audit"]["observations"]
                        .as_array()
                        .is_some_and(|pages| pages.iter().any(|p| p["has_image"] == true))
                    {
                        tags.insert("PDF_OBSERVED_IMAGE".into());
                    }
                    if classification["status"] == "no_text_in_parse_window" && !source.unverified()
                    {
                        audit.insert("primary_reason".into(), json!("pdf_window_no_text"));
                        let ranges:Vec<_>=classification["page_audit"]["checked_pages"].as_array().into_iter().flatten().filter_map(Value::as_u64).map(|page|json!({"unit":"pdf_page","start":page,"end":page+1,"label":"classification"})).collect();
                        audit.insert("read_ranges".into(), json!(ranges));
                    }
                }
            }
            if source.extension.as_deref() == Some(".pdf") {
                if let Some(diagnostic) = &file.final_diagnostic.0 {
                    let code = serde_json::to_value(diagnostic.error_code)?;
                    tags.insert(
                        if code == "PDF_PROTECTED" {
                            "PDF_PROTECTED"
                        } else if code == "PARSER_TIMEOUT" {
                            "PDF_PROBE_TIMEOUT"
                        } else {
                            "PDF_PARSE_FAILED"
                        }
                        .into(),
                    );
                }
            }
        }
        audit.insert("secondary_tags".into(), json!(tags));
        source.coverage_limits["evidence_audit"] = Value::Object(audit);
        sources.push(source);
    }
    sources.sort_by(|a, b| a.source_id.cmp(&b.source_id));
    gaps.sort_by(|a, b| {
        (&a.code, &a.source_id, &a.relative_path, &a.stage).cmp(&(
            &b.code,
            &b.source_id,
            &b.relative_path,
            &b.stage,
        ))
    });
    gaps.dedup_by(|a, b| {
        a.code == b.code
            && a.source_id == b.source_id
            && a.relative_path == b.relative_path
            && a.stage == b.stage
    });
    Ok((sources, gaps))
}

fn notes(app: &App, context: &RunContext) -> Result<String, ReportError> {
    let path = app
        .config
        .report
        .notes_dir
        .join(format!("{}.md", context.target_date));
    reject_redirects(&path, "NOTES_PATH_REDIRECTED")?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(ReportError::new(
            "NOTES_SIZE_EXCEEDED",
            "补充说明超过 64 KiB",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ReportError::new("NOTES_ENCODING_INVALID", "补充说明须使用 UTF-8 编码"))?;
    Ok(text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n"))
}

pub(super) fn prepare(
    app: &App,
    request: &ReportRequest,
    scan: &ContextResult,
) -> Result<FrozenInput, ReportError> {
    let (sources, gaps) = scan_sources(scan)?;
    let texts: BTreeMap<_, _> = scan
        .sources
        .iter()
        .map(|s| (s.source_id.clone(), s.excerpt.clone()))
        .collect();
    prepare_sources(app, request, sources, gaps, texts, None)
}

pub fn prepare_archive(app: &App, request: &ReportRequest) -> Result<FrozenInput, ReportError> {
    let prepared = crate::archive::model_input::prepare(&app.config, request)?;
    let target = json!({"archive_format":4,"archive_changes":prepared.changes,"continued":prepared.continued});
    prepare_sources(
        app,
        request,
        prepared.sources,
        prepared.gaps,
        prepared.texts,
        Some(target),
    )
}

fn prepare_sources(
    app: &App,
    request: &ReportRequest,
    mut sources: Vec<ReportSource>,
    mut gaps: Vec<CoverageGap>,
    texts: BTreeMap<String, String>,
    archive: Option<Value>,
) -> Result<FrozenInput, ReportError> {
    let mut inputs = Vec::new();
    for source in &sources {
        if let Some(input) = source.input(
            texts
                .get(&source.source_id)
                .map(|text| text.as_str())
                .unwrap_or(""),
        )? {
            inputs.push(input);
        }
    }
    if !request.input.trim().is_empty() {
        if request.input.len() > 65536 {
            return Err(ReportError::new(
                "USER_INPUT_TOO_LARGE",
                "工作补充超过 64 KiB",
            ));
        }
        let source = ReportSource::user(&request.input, request.context.started_at);
        inputs.push(source.input(&request.input)?.unwrap());
        sources.push(source);
    }
    let version = if archive.is_some() { 4 } else { 3 };
    let mut target = json!({"date":request.context.target_date,"window_start_utc":request.context.window_start_utc,"window_end_utc":request.context.window_end_utc});
    if let Some(archive) = archive {
        target
            .as_object_mut()
            .unwrap()
            .extend(archive.as_object().unwrap().clone());
    }
    let mut plan = plan::prepare(
        &app.config.llm,
        target,
        &inputs,
        serde_json::to_value(&gaps)?,
    )?;
    if version == 4 {
        for source in &sources {
            if source.coverage_limits["archive"].is_object() {
                plan.source_map[&source.source_id]["archive"] =
                    source.coverage_limits["archive"].clone();
            }
        }
        for batch in &mut plan.batches {
            batch.source_map = plan.source_map.clone();
        }
    }
    let mut ranges: BTreeMap<String, Vec<TextRange>> = BTreeMap::new();
    for batch in &plan.batches {
        for entry in &batch.entries {
            ranges
                .entry(entry.source_id.clone())
                .or_default()
                .extend(entry.text_ranges.iter().cloned());
        }
    }
    for source in &mut sources {
        source.model_chars = union_chars(
            ranges
                .get(&source.source_id)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        );
        source.model_truncated = source.model_chars < source.excerpt_chars;
        if source.model_truncated {
            gaps.push(source_gap(
                source,
                "MODEL_INPUT_BOUNDED",
                "model",
                "部分摘录未纳入模型输入，结论不覆盖这些内容。",
            ));
        }
    }
    let quality = if inputs.is_empty() {
        if sources.is_empty() && gaps.is_empty() {
            "no_changes"
        } else if version == 4 {
            "partial"
        } else {
            "unavailable"
        }
    } else if !gaps.is_empty() {
        "partial"
    } else {
        "complete"
    };
    let mut stable = serde_json::to_value(&sources)?;
    for source in stable.as_array_mut().unwrap() {
        for key in ["observed_at_utc", "snapshot_ref", "reason", "worker_lane"] {
            source.as_object_mut().unwrap().remove(key);
        }
    }
    let input_fingerprint = hash(
        &json!({"version":version,"scope_id":request.context.scope_id,"sources":stable,"plan":plan,"config_fingerprint":request.context.config_fingerprint}),
    )?;
    Ok(FrozenInput {
        version,
        context: request.context.clone(),
        quality: quality.into(),
        sources,
        gaps,
        plan,
        template: if version == 4 {
            crate::reports::DAILY_TEMPLATE_V4.into()
        } else {
            DAILY_TEMPLATE_V3.into()
        },
        root_alias: app.config.report.root_alias.clone(),
        notes: notes(app, &request.context)?,
        missed_dates: app.store.missed_dates(request.context.target_date)?,
        input_fingerprint,
        recovery_fingerprint: app.fingerprint(true)?,
        model_pricing: app.config.recovery.model_pricing.clone(),
        recovery_inputs: None,
    })
}

pub(super) fn apply_coverage(
    input: &mut FrozenInput,
    steps: &[StepResult],
    calls: &[Value],
) -> Result<(), ReportError> {
    let mut success: BTreeMap<String, Vec<TextRange>> = BTreeMap::new();
    let mut detailed: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut attempted: BTreeMap<String, Vec<TextRange>> = BTreeMap::new();
    let mut unknown = BTreeSet::new();
    for step in steps {
        for entry in &step.effective.entries {
            success
                .entry(entry.source_id.clone())
                .or_default()
                .extend(entry.text_ranges.iter().cloned());
            detailed.entry(entry.source_id.clone()).or_default().extend(entry.text_ranges.iter().map(|r|json!({"start":r.start,"end":r.end,"logical_step":step.effective.logical_step,"revision":step.effective.revision})));
        }
    }
    for call in calls {
        let usage: Value = serde_json::from_str(call["usage_json"].as_str().unwrap_or("{}"))?;
        if let Some(ranges) = usage["reserved_budget"]["request_audit"]["source_ranges"].as_object()
        {
            for (id, ranges) in ranges {
                let ranges: Vec<TextRange> = serde_json::from_value(ranges.clone())?;
                attempted.entry(id.clone()).or_default().extend(ranges);
                if matches!(call["status"].as_str(), Some("unknown" | "reserved")) {
                    unknown.insert(id.clone());
                }
            }
        }
    }
    let effective_hash = hash(&serde_json::to_value(
        steps.iter().map(|s| &s.effective).collect::<Vec<_>>(),
    )?)?;
    for source in &mut input.sources {
        let ranges = success.remove(&source.source_id).unwrap_or_default();
        let attempted = attempted.remove(&source.source_id).unwrap_or_default();
        if ranges
            .iter()
            .chain(&attempted)
            .any(|r| r.end > source.excerpt_chars || r.end <= r.start)
        {
            return Err(ReportError::new(
                "MODEL_SOURCE_RANGE_MISMATCH",
                "调用范围超过冻结摘录",
            ));
        }
        let audit = &mut source.coverage_limits["evidence_audit"];
        if !audit.is_object() {
            *audit = json!({"version":1,"primary_status":"usable_excerpt","secondary_tags":[]});
        }
        audit["planned_model_chars"] = json!(source.model_chars);
        audit["attempted_model_chars"] = json!(union_chars(&attempted));
        audit["model_ranges"] = json!(detailed.remove(&source.source_id).unwrap_or_default());
        audit["sent_unknown"] = json!(unknown.contains(&source.source_id));
        audit["effective_plan_sha256"] = json!(effective_hash);
        source.model_chars = union_chars(&ranges);
        source.model_truncated = source.model_chars < source.excerpt_chars;
        if source.model_truncated {
            let tags = audit["secondary_tags"]
                .as_array_mut()
                .ok_or_else(|| ReportError::new("REPORT_SOURCE_INVALID", "来源覆盖标识无效"))?;
            if !tags.contains(&json!("MODEL_INPUT_BOUNDED")) {
                tags.push(json!("MODEL_INPUT_BOUNDED"));
            }
            if !input.gaps.iter().any(|g| {
                g.code == "MODEL_INPUT_BOUNDED" && g.source_id.as_deref() == Some(&source.source_id)
            }) {
                input.gaps.push(source_gap(
                    source,
                    "MODEL_INPUT_BOUNDED",
                    "model",
                    "实际成功批次只处理了部分摘录，其余范围未成功用于成稿。",
                ));
            }
        }
    }
    if !input.gaps.is_empty() {
        input.quality = "partial".into();
    }
    Ok(())
}
