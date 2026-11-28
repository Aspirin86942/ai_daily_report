use super::*;

#[derive(Clone, Copy)]
pub(super) enum MaterialMode {
    Detailed,
    Bounded { dedup_shared: bool },
}

impl PersonalReport {
    /// Freeze the attachment and size both Markdown files before changing this report.
    pub fn render_detailed_output(&mut self, template: &str) -> Result<String, ReportError> {
        if self.schema_version != 6 {
            return Err(invalid("详细日报需要格式 v6"));
        }
        self.validate_content()?;
        let env = crate::reports::environment();
        let template = compile_template(&env, template)?;
        let labels = material_labels(&self.sources);
        let cited: BTreeSet<_> = self
            .items
            .iter()
            .flat_map(|item| &item.source_ids)
            .collect();
        let mut order: Vec<_> = self.sources.iter().enumerate().collect();
        // Keep the first copy of shared text before any link to it, even after dropping a suffix.
        order.sort_by_key(|(index, source)| {
            let start = source.coverage_limits["personal"]["excerpt_start"]
                .as_u64()
                .or_else(|| {
                    self.evidence
                        .get(&source.source_id)?
                        .first()?
                        .ranges
                        .first()
                        .map(|r| r.start)
                });
            (
                !cited.contains(&source.source_id),
                &source.relative_path,
                start,
                *index,
            )
        });
        let mut attachment = self.detailed_frame(&template, &labels, true, "", &self.quality)?;
        let mut seen = HashMap::new();
        let mut headings = HashMap::new();
        for source in &self.sources {
            let path = source.relative_path.as_deref().unwrap_or("");
            for entry in source.coverage_limits["personal"]["context"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(text) = entry["text"].as_str() {
                    headings.insert((path, text.trim()), None);
                }
            }
        }
        let mut supplements = Vec::new();
        let mut section = None;
        let mut supplement_start = attachment.len();
        for (index, source) in order {
            let required = cited.contains(&source.source_id);
            if section != Some(required) {
                if !required {
                    supplement_start = attachment.len();
                }
                attachment.push('\n');
                attachment.push_str(&fragment(
                    &template,
                    context! { section => "heading", required },
                )?);
                section = Some(required);
            }
            if !required {
                supplements.push((attachment.len(), index));
            }
            attachment.push('\n');
            attachment.push_str(&self.detailed_material(
                &template,
                source,
                &labels[source.source_id.as_str()],
                &mut seen,
                &mut headings,
                MaterialMode::Detailed,
            )?);
        }

        // The digest has fixed width. Measure the linked body before its attachment hash exists.
        let placeholder = format!("attachments/{}.{}.md", self.date, "0".repeat(64));
        let body_len = self
            .detailed_frame(&template, &labels, false, &placeholder, &self.quality)?
            .len();
        let mut reduction = None;
        if body_len + attachment.len() > MAX_REPORT_BYTES {
            let body_len = self
                .detailed_frame(&template, &labels, false, &placeholder, "partial")?
                .len();
            let available = MAX_REPORT_BYTES
                .checked_sub(body_len)
                .ok_or_else(too_large)?;
            let &(_, last) = supplements.last().ok_or_else(too_large)?;
            // Each offset is the start of a whole source. No re-render or re-hash per removal.
            for (kept, &(offset, first)) in supplements.iter().enumerate().rev() {
                let end = if kept == 0 { supplement_start } else { offset };
                if end >= available {
                    continue;
                }
                let count = supplements.len() - kept;
                let span = if first == last {
                    self.block_scope(first)
                } else {
                    format!("{} 至 {}", self.block_scope(first), self.block_scope(last))
                };
                let message =
                    format!("附件因字节上限省略了 {count} 块补充材料，按路径排列的范围为 {span}。");
                let mut note = format!("\n{}\n", crate::reports::markdown_inline(&message));
                if end + note.len() > available {
                    note = format!(
                        "\n省略补充材料 {count} 块（{} 至 {}，按路径排列）。\n",
                        source_label(first),
                        source_label(last)
                    );
                }
                if end + note.len() <= available {
                    attachment.truncate(end);
                    attachment.push_str(&note);
                    reduction = Some(CoverageGap::new(
                        ATTACHMENT_REDUCED,
                        "publish",
                        None,
                        &message,
                    ));
                    break;
                }
            }
            if reduction.is_none() {
                return Err(too_large());
            }
        }
        let attachment = EvidenceAttachment::new(attachment);
        let quality = if reduction.is_some() {
            "partial"
        } else {
            &self.quality
        };
        let link = attachment.link(&self.date);
        let body = self.detailed_frame(&template, &labels, false, &link, quality)?;
        if body.len() + attachment.markdown.len() > MAX_REPORT_BYTES {
            return Err(too_large());
        }
        validate_attachment_link(&body, &link)?;
        self.gaps.retain(|gap| gap.code != ATTACHMENT_REDUCED);
        if let Some(gap) = reduction {
            self.gaps.push(gap);
            self.quality = "partial".into();
        }
        self.evidence_attachment = Some(attachment);
        Ok(body)
    }

    fn block_scope(&self, index: usize) -> String {
        let source = &self.sources[index];
        let path = source.relative_path.as_deref().unwrap_or("用户补充");
        let mut ranges = self
            .evidence
            .get(&source.source_id)
            .into_iter()
            .flatten()
            .flat_map(|r| &r.ranges);
        let location = match (ranges.next(), ranges.next_back()) {
            (Some(first), Some(last)) if first != last => {
                format!(" · {} 至 {}", range_label(first), range_label(last))
            }
            (Some(first), _) => format!(" · {}", range_label(first)),
            _ => String::new(),
        };
        format!("{}（{path}{location}）", source_label(index))
    }

    pub(super) fn detailed_body(&self, template: &str, link: &str) -> Result<String, ReportError> {
        let env = crate::reports::environment();
        let template = compile_template(&env, template)?;
        self.detailed_frame(
            &template,
            &material_labels(&self.sources),
            false,
            link,
            &self.quality,
        )
    }

    fn detailed_frame(
        &self,
        template: &Template<'_, '_>,
        labels: &BTreeMap<String, String>,
        evidence_only: bool,
        link: &str,
        quality: &str,
    ) -> Result<String, ReportError> {
        let mut overview = Vec::new();
        let mut remaining = MAX_OVERVIEW_CHARS;
        if !evidence_only {
            for (index, item) in self.items.iter().enumerate() {
                let chars = item.title.chars().count();
                if chars <= remaining {
                    overview.push(json!({"number":index + 1, "title":item.title, "statement_kind":item.statement_kind}));
                    remaining -= chars;
                    if remaining == 0 || overview.len() == 3 {
                        break;
                    }
                }
            }
        }
        let mut gaps = BTreeMap::new();
        for gap in &self.gaps {
            if gap.code != ATTACHMENT_REDUCED {
                *gaps.entry(&gap.message).or_insert(0usize) += 1;
            }
        }
        fragment(
            template,
            crate::reports::template_value(&json!({
                "section":"report", "evidence_only":evidence_only, "evidence_link":link, "date":self.date,
                "selection":self.selection, "items":self.items, "overview":overview, "labels":labels, "quality":quality, "gaps":gaps,
            })),
        )
    }

    pub(super) fn detailed_material<'a>(
        &'a self,
        template: &Template<'_, '_>,
        source: &'a ReportSource,
        label: &str,
        seen: &mut HashMap<&'a str, String>,
        headings: &mut HashMap<(&'a str, &'a str), Option<String>>,
        mode: MaterialMode,
    ) -> Result<String, ReportError> {
        let (complete, dedup_shared) = match mode {
            MaterialMode::Detailed => (false, true),
            MaterialMode::Bounded { dedup_shared } => (true, dedup_shared),
        };
        let refs = self
            .evidence
            .get(&source.source_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let contexts = block_context(source)?;
        let mut parts = Vec::new();
        let anchor = label.to_ascii_lowercase();
        let path = source.relative_path.as_deref().unwrap_or("");
        let mut originals = Vec::new();
        let mut add = |kind: &str, text: &'a str, ranges: &[SourceRange], shared: Option<&str>| {
            let part_anchor = format!("{anchor}-p{}", parts.len() + 1);
            let previous = if dedup_shared {
                shared.or_else(|| seen.get(text).map(String::as_str))
            } else {
                None
            };
            let printed_at = previous.unwrap_or(&part_anchor).to_owned();
            let mut range = if complete {
                compact_ranges_label(ranges)
            } else {
                ranges
                    .iter()
                    .map(range_label)
                    .collect::<Vec<_>>()
                    .join("、")
            };
            if range.is_empty() && kind == "本次读取" {
                let metadata = &source.coverage_limits["personal"];
                if let (Some(start), Some(end)) = (
                    metadata["excerpt_start"].as_u64(),
                    metadata["excerpt_end"].as_u64(),
                ) {
                    range = format!("摘录字符 [{start}, {end})");
                }
            }
            parts.push(context! {
                kind, range => &range, anchor => &part_anchor, previous,
                text => if previous.is_none() { text } else { "" },
            });
            if complete {
                originals.push((
                    part_anchor.clone(),
                    previous.map(str::to_owned),
                    text,
                    range,
                ));
            }
            if dedup_shared && previous.is_none() {
                seen.insert(text, part_anchor);
            }
            printed_at
        };
        if !contexts.is_empty() {
            let lines: BTreeSet<_> = refs
                .iter()
                .flat_map(|r| r.quote.lines().map(str::trim))
                .collect();
            for context in contexts {
                if !context.text.lines().all(|line| lines.contains(line.trim())) {
                    let shared = headings
                        .get(&(path, context.text.trim()))
                        .and_then(Option::as_deref);
                    add("表头、标题或单位", context.text, &context.ranges, shared);
                }
            }
        }
        for reference in refs {
            let kind = match reference.side.as_str() {
                "before" => "变更前",
                "after" => "变更后",
                _ => "本次读取",
            };
            let printed_at = add(kind, &reference.quote, &reference.ranges, None);
            for line in reference.quote.lines() {
                if let Some(location) = headings.get_mut(&(path, line.trim())) {
                    location.get_or_insert_with(|| printed_at.clone());
                }
            }
        }
        if source.source_kind == "user_statement" {
            add("用户陈述", &self.user_notes, &[], None);
        }
        let mut view = context! {
            section => "material", label, anchor => &anchor, path => source.relative_path.as_deref(),
            observed => &source.observed_at_utc, unverified => source.unverified(), parts,
        };
        let omitted;
        if complete {
            omitted = super::bounded::omitted_ranges(source)?;
            view = context! { omitted => &omitted, ..view };
        }
        let block = fragment(template, view)?;
        if complete
            && (!block
                .lines()
                .any(|line| line == format!("<a id=\"{anchor}\"></a>"))
                || originals.into_iter().any(|(part, previous, text, range)| {
                    let body_ok = if let Some(prev) = previous {
                        block.contains(&format!("[查看相同原文](#{prev})"))
                    } else {
                        block.contains(&format!(
                            "<a id=\"{part}\"></a>\n<pre><code>{}</code></pre>",
                            crate::reports::source_verbatim(text)
                        ))
                    };
                    !body_ok || !block.contains(&crate::reports::markdown_inline(&range))
                }))
        {
            return Err(invalid("依据模板缺少原文、位置或引用锚点"));
        }
        Ok(block)
    }
}

pub(super) fn fragment(template: &Template<'_, '_>, view: Value) -> Result<String, ReportError> {
    let text = template
        .render(view)
        .map_err(|_| invalid("个人日报排版失败"))?;
    Ok(format!(
        "{}\n",
        text.replace("\r\n", "\n").replace('\r', "\n").trim_end()
    ))
}

#[derive(Deserialize)]
pub(super) struct BlockContext<'a> {
    pub(super) text: &'a str,
    #[serde(default)]
    pub(super) ranges: Vec<SourceRange>,
}

pub(super) fn block_context(source: &ReportSource) -> Result<Vec<BlockContext<'_>>, ReportError> {
    let value = &source.coverage_limits["personal"]["context"];
    if value.is_null() {
        return Ok(Vec::new());
    }
    let contexts =
        Vec::<BlockContext<'_>>::deserialize(value).map_err(|_| invalid("材料上下文格式无效"))?;
    if contexts
        .iter()
        .any(|c| c.text.trim().is_empty() || c.ranges.iter().any(|r| r.validate().is_err()))
    {
        return Err(invalid("材料上下文的原文或位置无效"));
    }
    Ok(contexts)
}

fn merge_spans(mut spans: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    spans.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (start, end) in spans {
        if let Some(prev) = merged.last_mut().filter(|prev| start <= prev.1) {
            prev.1 = prev.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn format_positions(start: u64, end: u64) -> String {
    if end - start == 1 {
        start.to_string()
    } else {
        format!("{start}–{}", end - 1)
    }
}

fn unit_label(unit: SourceRangeUnit) -> &'static str {
    match unit {
        SourceRangeUnit::PdfPage => "页",
        SourceRangeUnit::SheetRow | SourceRangeUnit::TableRow => "行",
        SourceRangeUnit::Paragraph => "段",
        SourceRangeUnit::Slide => "张幻灯片",
        SourceRangeUnit::CsvRecord => "条记录",
        SourceRangeUnit::TextChar => "字符",
        SourceRangeUnit::Byte => "字节",
    }
}

pub(crate) fn compact_ranges_label(ranges: &[SourceRange]) -> String {
    if ranges.is_empty() {
        return String::new();
    }
    struct Group {
        label: Option<String>,
        unit: SourceRangeUnit,
        start: u64,
        end: u64,
        cols: Vec<(u64, u64)>,
    }
    let mut groups: Vec<Group> = Vec::new();
    for r in ranges {
        let label = r
            .label
            .as_deref()
            .map(|text| text.replace(['\r', '\n'], " "));
        if let Some(group) = groups
            .iter_mut()
            .find(|g| g.label == label && g.unit == r.unit && g.start == r.start && g.end == r.end)
        {
            if let (Some(cs), Some(ce)) = (r.column_start, r.column_end) {
                group.cols.push((cs, ce));
            }
        } else {
            let cols = match (r.column_start, r.column_end) {
                (Some(cs), Some(ce)) => vec![(cs, ce)],
                _ => Vec::new(),
            };
            groups.push(Group {
                label,
                unit: r.unit,
                start: r.start,
                end: r.end,
                cols,
            });
        }
    }
    for group in &mut groups {
        group.cols = merge_spans(std::mem::take(&mut group.cols));
    }
    let mut merged: Vec<Group> = Vec::new();
    for group in groups {
        if let Some(prev) = merged.last_mut().filter(|prev| {
            prev.label == group.label
                && prev.unit == group.unit
                && prev.cols == group.cols
                && group.start <= prev.end
        }) {
            prev.end = prev.end.max(group.end);
        } else {
            merged.push(group);
        }
    }
    merged
        .into_iter()
        .map(|g| {
            let unit = unit_label(g.unit);
            let mut location = match g.unit {
                SourceRangeUnit::TextChar | SourceRangeUnit::Byte => {
                    format!("{unit} [{}, {})", g.start, g.end)
                }
                _ => format!("第 {} {unit}", format_positions(g.start, g.end)),
            };
            if !g.cols.is_empty() {
                let cols = g
                    .cols
                    .into_iter()
                    .map(|(cs, ce)| format_positions(cs, ce))
                    .collect::<Vec<_>>()
                    .join("、");
                location.push_str(&format!("，第 {cols} 列"));
            }
            if let Some(label) = g.label {
                location = format!("{label} · {location}");
            }
            location
        })
        .collect::<Vec<_>>()
        .join("、")
}

pub(super) fn range_label(range: &SourceRange) -> String {
    // Stored ranges are half-open. Display page, row and column numbers inclusively.
    let unit = unit_label(range.unit);
    let mut location = match range.unit {
        SourceRangeUnit::TextChar | SourceRangeUnit::Byte => {
            format!("{unit} [{}, {})", range.start, range.end)
        }
        _ => format!("第 {} {unit}", format_positions(range.start, range.end)),
    };
    if let (Some(start), Some(end)) = (range.column_start, range.column_end) {
        location.push_str(&format!("，第 {} 列", format_positions(start, end)));
    }
    if let Some(label) = &range.label {
        location = format!("{label} · {location}");
    }
    location
}
