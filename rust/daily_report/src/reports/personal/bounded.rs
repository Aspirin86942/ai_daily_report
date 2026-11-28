use super::{
    detailed::{fragment, range_label},
    *,
};

pub(crate) fn bounded_body(
    template: &Template<'_, '_>,
    items: &[PersonalItem],
    labels: &BTreeMap<String, String>,
    link: &str,
    no_changes: bool,
) -> Result<String, ReportError> {
    let body = fragment(
        template,
        crate::reports::template_value(&json!({
            "section":"body", "items":items, "labels":labels,
            "evidence_link":link, "no_changes":no_changes,
        })),
    )?;
    if body.trim().is_empty()
        || items.iter().any(|item| {
            !body.contains(&crate::reports::markdown_inline(&item.title))
                || !body.contains(&crate::reports::markdown_lines(&item.text))
                || item.source_ids.iter().any(|id| {
                    labels.get(id).is_none_or(|label| {
                        !body.contains(&format!("[{label}]({link}#{})", label.to_ascii_lowercase()))
                    })
                })
        })
    {
        return Err(invalid("正文模板缺少完整事项或依据引用"));
    }
    Ok(body)
}

impl PersonalReport {
    pub(crate) fn prepare_bounded_attachment(
        &self,
        template: &str,
    ) -> Result<EvidenceAttachment, ReportError> {
        self.validate_content()?;
        let env = crate::reports::environment();
        let render = compile_template(&env, template)?;
        let mut markdown = fragment(
            &render,
            crate::reports::template_value(&json!({
                "section":"evidence", "date": self.date, "selection":self.selection, "gaps":self.gaps,
            })),
        )?;
        if !markdown
            .lines()
            .any(|line| line == "<a id=\"personal-evidence\"></a>")
        {
            return Err(invalid("依据模板缺少附件入口锚点"));
        }
        // UTF-8 uses up to four bytes per Unicode scalar in the final body.
        check_attachment_budget(markdown.len(), 0)?;
        let labels = material_labels(&self.sources);
        let dedup_shared = template.contains("part.previous");
        let mut seen = HashMap::new();
        let mut headings = HashMap::new();
        if dedup_shared {
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
        }
        for source in &self.sources {
            let block = self.detailed_material(
                &render,
                source,
                &labels[&source.source_id],
                &mut seen,
                &mut headings,
                super::detailed::MaterialMode::Bounded { dedup_shared },
            )?;
            check_attachment_budget(markdown.len() + 1 + block.len(), 0).map_err(|_| {
                ReportError::new(
                    "PERSONAL_REPORT_TOO_LARGE",
                    format!(
                        "完整依据超过 1,000,000 字节，超限材料为 {}（{}）",
                        source.relative_path.as_deref().unwrap_or("用户补充"),
                        labels[&source.source_id]
                    ),
                )
            })?;
            markdown.push('\n');
            markdown.push_str(&block);
        }
        let attachment = EvidenceAttachment::new(markdown);
        let (header, entry) = self.bounded_wrappers(&render, &attachment.link(&self.date))?;
        check_attachment_budget(attachment.markdown.len(), header.len() + entry.len())?;
        attachment.validate()?;
        Ok(attachment)
    }

    pub(crate) fn render_bounded_output(
        &self,
        template: &str,
        preview: bool,
    ) -> Result<String, ReportError> {
        let attachment = self
            .evidence_attachment
            .as_ref()
            .ok_or_else(|| invalid("缺少完整依据附件"))?;
        let link = if preview {
            String::new()
        } else {
            attachment.link(&self.date)
        };
        let env = crate::reports::environment();
        let render = compile_template(&env, template)?;
        let (header, entry) =
            self.bounded_wrappers(&render, if preview { "#personal-evidence" } else { &link })?;
        let body = bounded_body(
            &render,
            &self.items,
            &material_labels(&self.sources),
            &link,
            self.quality == "no_changes",
        )?;
        if body.chars().count() > MAX_BODY_CHARS {
            return Err(ReportError::new(
                "PERSONAL_REPORT_BODY_TOO_LONG",
                "正文超过 1,500 个 Unicode 字符",
            ));
        }
        let mut output = format!("{header}\n{body}\n{entry}");
        if preview {
            self.validate_output_size((output.len() + 1) as u64)?;
            output.push('\n');
            output.push_str(&attachment.markdown);
        } else {
            self.validate_output(&output)?;
        }
        Ok(output)
    }

    fn bounded_wrappers(
        &self,
        template: &Template<'_, '_>,
        link: &str,
    ) -> Result<(String, String), ReportError> {
        let header = fragment(
            template,
            context! { section => "header", date => &self.date },
        )?;
        let entry = fragment(
            template,
            context! { section => "entry", evidence_link => link },
        )?;
        validate_attachment_link(&entry, link)?;
        Ok((header, entry))
    }
}

fn check_attachment_budget(attachment: usize, wrappers: usize) -> Result<(), ReportError> {
    if attachment + wrappers + 2 + 4 * MAX_BODY_CHARS > MAX_REPORT_BYTES {
        return Err(too_large());
    }
    Ok(())
}

pub(super) fn omitted_ranges(source: &ReportSource) -> Result<Vec<String>, ReportError> {
    let raw = &source.coverage_limits["evidence_audit"]["omitted_ranges"];
    let mut parsed = Vec::new();
    for value in raw.as_array().into_iter().flatten() {
        let range = SourceRange::deserialize(value).map_err(|_| invalid("未覆盖范围格式无效"))?;
        range
            .validate()
            .map_err(|_| invalid("未覆盖范围位置无效"))?;
        parsed.push(range);
    }
    let mut groups: Vec<(Option<String>, SourceRangeUnit, Vec<SourceRange>)> = Vec::new();
    for range in parsed {
        if let Some((_, _, list)) = groups
            .iter_mut()
            .find(|(label, unit, _)| *label == range.label && *unit == range.unit)
        {
            list.push(range);
        } else {
            groups.push((range.label.clone(), range.unit, vec![range]));
        }
    }
    let mut gaps = Vec::new();
    for (_, _, mut list) in groups {
        list.sort_by_key(|range| (range.start, range.end));
        let mut merged: Vec<SourceRange> = Vec::new();
        for range in list {
            if let Some(prev) = merged.last_mut().filter(|prev| range.start <= prev.end) {
                prev.end = prev.end.max(range.end);
                if let (Some(cs), Some(ce)) = (range.column_start, range.column_end) {
                    prev.column_start = Some(prev.column_start.map_or(cs, |old| old.min(cs)));
                    prev.column_end = Some(prev.column_end.map_or(ce, |old| old.max(ce)));
                }
            } else {
                merged.push(range);
            }
        }
        for range in merged {
            gaps.push(format!(
                "解析或摘录上限导致未覆盖 · {}",
                range_label(&range)
            ));
        }
    }
    if (source.parse_truncated || source.excerpt_truncated) && gaps.is_empty() {
        gaps.push("解析或摘录达到上限，未覆盖的确切位置未能确认。".into());
    }
    Ok(gaps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        infra::json::digest,
        reports::data::{EvidenceRef, ReportSource},
        types::{new_run_id, parse_date, RunContext},
    };
    use chrono::Utc;
    use daily_parsers::contract::{SourceRange, SourceRangeUnit};

    fn col_range(row: u64, col_start: u64, col_end: u64) -> SourceRange {
        let mut range =
            SourceRange::new(SourceRangeUnit::SheetRow, row, row + 1, Some("汇总".into()));
        range.column_start = Some(col_start);
        range.column_end = Some(col_end);
        range
    }

    fn test_report(block_count: usize, omitted_rows: u64) -> (PersonalReport, String) {
        let context = RunContext::new(
            new_run_id(),
            Utc::now(),
            parse_date("2026-11-25").unwrap(),
            "isolated".into(),
        )
        .unwrap();
        let header = format!(
            "A1=员工编号 | B1=姓名 | C1=部门 | D1=备注_{}",
            "长表头字段".repeat(65)
        );
        let raw_hash = digest(b"workbook-bytes");
        let ctx_ranges = vec![col_range(1, 1, 4), col_range(1, 4, 6), col_range(1, 8, 9)];
        let mut materials = Vec::with_capacity(block_count);
        for index in 0..block_count {
            let row = (index + 2) as u64;
            let text = format!("A{row}={index} | B{row}=员工{index} | C{row}=生产部");
            let mut source = ReportSource::user(&text, context.started_at);
            source.source_kind = "file".into();
            source.source_id = format!("src_{}", digest(format!("block-{index}").as_bytes()));
            source.relative_path = Some("2026年/测试多块工作簿.xlsx".into());
            source.extension = Some(".xlsx".into());
            source.content_sha256 = Some(raw_hash.clone());
            source.coverage_limits["personal"] =
                json!({"context":[{"text":header, "ranges":ctx_ranges}]});
            if index == 0 && omitted_rows > 0 {
                let omitted: Vec<_> = (0..omitted_rows)
                    .map(|offset| col_range(50 + offset, 1 + (offset % 3), 8 + (offset % 4)))
                    .collect();
                source.parse_truncated = true;
                source.coverage_limits["evidence_audit"]["omitted_ranges"] = json!(omitted);
            }
            let evidence = vec![EvidenceRef {
                source_id: source.source_id.clone(),
                version_id: format!("scan-content:{raw_hash}"),
                extraction_id: "scan:test".into(),
                unit_id: format!("e{index}"),
                side: "current".into(),
                raw_sha256: raw_hash.clone(),
                unit_sha256: source.excerpt_sha256.clone(),
                quote_sha256: source.excerpt_sha256.clone(),
                quote: text.clone(),
                unit_char_start: 0,
                input_start: 0,
                input_end: text.chars().count(),
                ranges: vec![col_range(row, 1, 4), col_range(row, 4, 6)],
            }];
            materials.push(Material {
                source,
                text,
                evidence,
                priority: 1,
            });
        }
        let selection = Selection {
            summary: SelectionSummary {
                observed_files: 1,
                inspected_files: 1,
                selected_files: 1,
                selected_chars: materials.iter().map(|m| m.source.excerpt_chars).sum(),
                ..Default::default()
            },
            materials,
            gaps: vec![],
        };
        selection.validate().unwrap();
        (
            PersonalReport::from_selection(&context, "work", &selection),
            header,
        )
    }

    #[test]
    fn attachment_budget_reserves_maximum_utf8_body_and_exact_wrapper_bytes() {
        let wrappers = 104;
        let remaining = MAX_REPORT_BYTES - wrappers - 2 - 4 * MAX_BODY_CHARS;
        assert!(check_attachment_budget(remaining, wrappers).is_ok());
        assert_eq!(
            check_attachment_budget(remaining + 1, wrappers)
                .unwrap_err()
                .code,
            "PERSONAL_REPORT_TOO_LARGE"
        );
    }

    #[test]
    fn bounded_attachment_deduplicates_repeated_context_and_coalesces_omitted_ranges() {
        let (report, header) = test_report(300, 500);
        let attachment = report.prepare_bounded_attachment(SCAN_TEMPLATE_V7).unwrap();
        assert!(attachment.markdown.len() < MAX_REPORT_BYTES);
        let full_header = format!(
            "<pre><code>{}</code></pre>",
            crate::reports::source_verbatim(&header)
        );
        assert_eq!(attachment.markdown.matches(&full_header).count(), 1);
        assert_eq!(
            attachment
                .markdown
                .matches("[查看相同原文](#s001-p1)")
                .count(),
            299
        );
        assert!(attachment.markdown.contains("汇总 · 第 1 行，第 1–5、8 列"));
        assert_eq!(
            attachment
                .markdown
                .matches("解析或摘录上限导致未覆盖")
                .count(),
            1
        );
        assert!(attachment
            .markdown
            .contains("解析或摘录上限导致未覆盖 · 汇总 · 第 50–549 行，第 1–10 列"));

        // Older frozen templates without `part.previous` keep inline rendering.
        let (small, _) = test_report(2, 0);
        let legacy_template = SCAN_TEMPLATE_V7.replace("\r\n", "\n").replace(
            "{% if part.previous %}[查看相同原文](#{{ part.previous }})\n{% else %}<a id=\"{{ part.anchor }}\"></a>\n<pre><code>{{ part.text | source_verbatim }}</code></pre>\n{% endif %}",
            "<a id=\"{{ part.anchor }}\"></a>\n<pre><code>{{ part.text | source_verbatim }}</code></pre>\n",
        );
        let legacy = small.prepare_bounded_attachment(&legacy_template).unwrap();
        assert_eq!(legacy.markdown.matches(&full_header).count(), 2);
        assert!(!legacy.markdown.contains("[查看相同原文]"));
    }
}
