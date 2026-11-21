use super::*;

impl PersonalReport {
    /// Freeze the attachment and size both Markdown files before changing this report.
    pub fn render_detailed_output(&mut self, template: &str) -> Result<String, ReportError> {
        if self.schema_version != 6 {
            return Err(invalid("详细日报需要格式 v6"));
        }
        self.validate_content()?;
        let env = crate::reports::environment();
        let template = env
            .template_from_str(template)
            .map_err(|_| invalid("个人日报模板无效"))?;
        let labels = self.detailed_labels();
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
                        "\n省略补充材料 {count} 块（S{:03} 至 S{:03}，按路径排列）。\n",
                        first + 1,
                        last + 1
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
        let attachment = EvidenceAttachment {
            sha256: digest(attachment.as_bytes()),
            markdown: attachment,
        };
        let quality = if reduction.is_some() {
            "partial"
        } else {
            &self.quality
        };
        let body = self.detailed_frame(
            &template,
            &labels,
            false,
            &attachment.link(&self.date),
            quality,
        )?;
        if body.len() + attachment.markdown.len() > MAX_REPORT_BYTES {
            return Err(too_large());
        }
        if !body.contains(&format!("]({})", attachment.link(&self.date))) {
            return Err(invalid("正文缺少完整依据附件链接"));
        }
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
        format!("S{:03}（{path}{location}）", index + 1)
    }

    fn detailed_labels(&self) -> BTreeMap<&str, String> {
        self.sources
            .iter()
            .enumerate()
            .map(|(i, source)| (source.source_id.as_str(), format!("S{:03}", i + 1)))
            .collect()
    }

    pub(super) fn detailed_body(&self, template: &str, link: &str) -> Result<String, ReportError> {
        let env = crate::reports::environment();
        let template = env
            .template_from_str(template)
            .map_err(|_| invalid("个人日报模板无效"))?;
        self.detailed_frame(
            &template,
            &self.detailed_labels(),
            false,
            link,
            &self.quality,
        )
    }

    fn detailed_frame(
        &self,
        template: &Template<'_, '_>,
        labels: &BTreeMap<&str, String>,
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

    fn detailed_material<'a>(
        &'a self,
        template: &Template<'_, '_>,
        source: &'a ReportSource,
        label: &str,
        seen: &mut HashMap<&'a str, String>,
        headings: &mut HashMap<(&'a str, &'a str), Option<String>>,
    ) -> Result<String, ReportError> {
        let refs = self
            .evidence
            .get(&source.source_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let contexts = block_context(source)?;
        let mut parts = Vec::new();
        let anchor = label.to_ascii_lowercase();
        let path = source.relative_path.as_deref().unwrap_or("");
        let mut add = |kind: &str, text: &'a str, ranges: &[SourceRange], shared: Option<&str>| {
            let part_anchor = format!("{anchor}-p{}", parts.len() + 1);
            let previous = shared.or_else(|| seen.get(text).map(String::as_str));
            let printed_at = previous.unwrap_or(&part_anchor).to_owned();
            let mut range = ranges
                .iter()
                .map(range_label)
                .collect::<Vec<_>>()
                .join("、");
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
                kind, range, anchor => &part_anchor, previous,
                text => if previous.is_none() { text } else { "" },
            });
            if previous.is_none() {
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
        fragment(
            template,
            context! {
                section => "material", label, anchor, path => source.relative_path.as_deref(),
                observed => &source.observed_at_utc, unverified => source.unverified(), parts,
            },
        )
    }
}

fn fragment(template: &Template<'_, '_>, view: Value) -> Result<String, ReportError> {
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

fn range_label(range: &SourceRange) -> String {
    // Stored ranges are half-open. Display page, row and column numbers inclusively.
    let positions = |start: u64, end: u64| {
        if end - start == 1 {
            start.to_string()
        } else {
            format!("{start}–{}", end - 1)
        }
    };
    let unit = match range.unit {
        SourceRangeUnit::PdfPage => "页",
        SourceRangeUnit::SheetRow | SourceRangeUnit::TableRow => "行",
        SourceRangeUnit::Paragraph => "段",
        SourceRangeUnit::Slide => "张幻灯片",
        SourceRangeUnit::CsvRecord => "条记录",
        SourceRangeUnit::TextChar => "字符",
        SourceRangeUnit::Byte => "字节",
    };
    let mut location = match range.unit {
        SourceRangeUnit::TextChar | SourceRangeUnit::Byte => {
            format!("{unit} [{}, {})", range.start, range.end)
        }
        _ => format!("第 {} {unit}", positions(range.start, range.end)),
    };
    if let (Some(start), Some(end)) = (range.column_start, range.column_end) {
        location.push_str(&format!("，第 {} 列", positions(start, end)));
    }
    if let Some(label) = &range.label {
        location = format!("{label} · {location}");
    }
    location
}
