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
        let mut seen = HashMap::new();
        let mut headings = HashMap::new();
        for source in &self.sources {
            let block = self.detailed_material(
                &render,
                source,
                &labels[&source.source_id],
                &mut seen,
                &mut headings,
                true,
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
    let mut gaps = Vec::new();
    for value in raw.as_array().into_iter().flatten() {
        let range = SourceRange::deserialize(value).map_err(|_| invalid("未覆盖范围格式无效"))?;
        range
            .validate()
            .map_err(|_| invalid("未覆盖范围位置无效"))?;
        gaps.push(format!(
            "解析或摘录上限导致未覆盖 · {}",
            range_label(&range)
        ));
    }
    if (source.parse_truncated || source.excerpt_truncated) && gaps.is_empty() {
        gaps.push("解析或摘录达到上限，未覆盖的确切位置未能确认。".into());
    }
    Ok(gaps)
}

#[cfg(test)]
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
