use super::{terms, Fragment};
use crate::reports::personal::PersonalReport;
use daily_parsers::contract::{SourceRange, SourceRangeUnit};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

// Keep positions in the immutable, hash-verified publication. The report may also
// contain source blocks that were omitted from the published attachment.
#[derive(Default)]
struct Section<'a> {
    start: usize,
    anchor: Option<&'a str>,
    source: Option<usize>,
    caption: Option<&'a str>,
    reference: Option<usize>,
    context: Option<&'a Value>,
}

fn anchor(line: &str) -> Option<&str> {
    let id = line.strip_prefix("<a id=\"")?.strip_suffix("\"></a>")?;
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')).then_some(id)
}

fn source_index(anchor: &str, report: &PersonalReport) -> Option<usize> {
    let label = anchor.split('-').next()?.strip_prefix('s')?;
    let number = label.parse::<usize>().ok()?;
    (number <= report.sources.len() && label == format!("{number:03}"))
        .then_some(number)?
        .checked_sub(1)
}

fn caption_kind(line: &str) -> &str {
    line.split_once(" · ").map_or(line, |(kind, _)| kind)
}

fn original_link(line: &str) -> Option<&str> {
    line.strip_prefix("[查看相同原文](#")?.strip_suffix(')')
}

fn html_text(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn markdown_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek().is_some_and(|c| c.is_ascii_punctuation()) {
            output.push(chars.next().unwrap());
        } else {
            output.push(ch);
        }
    }
    html_text(&output)
}

fn visible_contexts(report: &PersonalReport, index: usize) -> Vec<&Value> {
    let source = &report.sources[index];
    let lines: BTreeSet<_> = report
        .evidence
        .get(&source.source_id)
        .into_iter()
        .flatten()
        .flat_map(|r| r.quote.lines().map(str::trim))
        .collect();
    source.coverage_limits["personal"]["context"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| {
            c["text"]
                .as_str()
                .is_some_and(|text| !text.lines().all(|line| lines.contains(line.trim())))
        })
        .collect()
}

// Decode the frozen v6 location contract: numbered rows/pages are inclusive on
// paper; character and byte offsets retain their stored half-open interval.
fn published_range(r: &SourceRange) -> String {
    let positions = |a: u64, b: u64| {
        if b - a == 1 {
            a.to_string()
        } else {
            format!("{a}–{}", b - 1)
        }
    };
    let unit = match r.unit {
        SourceRangeUnit::PdfPage => "页",
        SourceRangeUnit::SheetRow | SourceRangeUnit::TableRow => "行",
        SourceRangeUnit::Slide => "张幻灯片",
        SourceRangeUnit::Paragraph => "段",
        SourceRangeUnit::CsvRecord => "条记录",
        SourceRangeUnit::TextChar => "字符",
        SourceRangeUnit::Byte => "字节",
    };
    let mut location = if matches!(r.unit, SourceRangeUnit::TextChar | SourceRangeUnit::Byte) {
        format!("{unit} [{}, {})", r.start, r.end)
    } else {
        format!("第 {} {unit}", positions(r.start, r.end))
    };
    if let (Some(a), Some(b)) = (r.column_start, r.column_end) {
        location.push_str(&format!("，第 {} 列", positions(a, b)));
    }
    if let Some(label) = &r.label {
        location = format!("{} · {location}", label.replace(['\r', '\n'], " "));
    }
    location
}

fn context_literal<'a>(
    context: &Value,
    displayed: &'a str,
    range: Option<&str>,
    linked: bool,
) -> Option<(&'a str, Value)> {
    let ranges = if context["ranges"].is_null() {
        Vec::new()
    } else {
        Vec::<SourceRange>::deserialize(&context["ranges"]).ok()?
    };
    let label = ranges
        .iter()
        .map(published_range)
        .collect::<Vec<_>>()
        .join("、");
    let compact = crate::reports::personal::detailed::compact_ranges_label(&ranges);
    let actual_range = range.unwrap_or_default();
    if actual_range != label && actual_range != compact {
        return None;
    }
    let raw = context["text"]
        .as_str()?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let literal = if displayed == raw {
        displayed
    } else if linked && displayed.trim() == raw.trim() {
        displayed.trim()
    } else if linked {
        // A shared heading can point into a multi-line published quote. Only lend
        // the matching heading, never the rest of that other source's evidence.
        displayed
            .lines()
            .map(str::trim)
            .find(|line| *line == raw.trim())?
    } else {
        return None;
    };
    Some((literal, json!(ranges)))
}

pub(super) fn append(report: &PersonalReport, fragments: &mut Vec<Fragment>) {
    let Some(attachment) = &report.evidence_attachment else {
        return;
    };
    let path = attachment.link(&report.date);
    let lines: Vec<_> = attachment.markdown.lines().collect();
    let mut section = Section::default();
    let mut in_code = false;
    let mut next_reference = 0;
    let mut contexts = Vec::new().into_iter();
    let mut printed = BTreeMap::new();
    for (index, &line) in lines.iter().enumerate() {
        if in_code || line.starts_with("<pre><code>") {
            in_code = !line.ends_with("</code></pre>");
            continue;
        }
        let kind = caption_kind(line);
        let is_reference = matches!(kind, "本次读取" | "变更前" | "变更后");
        let next = if let Some(id) = anchor(line) {
            let source = source_index(id, report);
            if source.is_some()
                && source == section.source
                && id.contains("-p")
                && section.caption.is_some()
            {
                section.anchor = Some(id);
                continue;
            }
            next_reference = 0;
            contexts = source
                .map(|i| visible_contexts(report, i))
                .unwrap_or_default()
                .into_iter();
            Section {
                start: index,
                anchor: Some(id),
                source,
                ..Default::default()
            }
        } else if section.source.is_some()
            && (is_reference || matches!(kind, "表头、标题或单位" | "用户陈述"))
            && lines[index + 1..]
                .iter()
                .find(|line| !line.trim().is_empty())
                .is_some_and(|line| {
                    anchor(line).is_some()
                        || original_link(line).is_some()
                        || line.starts_with("文件版本：")
                })
        {
            // Repeated originals link to a previous part and have no part anchor of their own.
            Section {
                start: index,
                anchor: section.anchor.map(|a| a.split('-').next().unwrap_or(a)),
                source: section.source,
                caption: Some(line),
                reference: is_reference.then(|| {
                    let ordinal = next_reference;
                    next_reference += 1;
                    ordinal
                }),
                context: if kind == "表头、标题或单位" {
                    contexts.next()
                } else {
                    None
                },
            }
        } else if line.starts_with("## ")
            || line.starts_with("附件因字节上限省略了")
            || line.starts_with("省略补充材料 ")
        {
            Section {
                start: index,
                source: (line == "## 用户补充原文")
                    .then(|| {
                        report
                            .sources
                            .iter()
                            .position(|source| source.source_id == "user_input")
                    })
                    .flatten(),
                ..Default::default()
            }
        } else {
            continue;
        };
        section.append(&lines[..index], report, &path, &mut printed, fragments);
        section = next;
    }
    section.append(&lines, report, &path, &mut printed, fragments);
}

impl Section<'_> {
    fn append(
        &self,
        lines: &[&str],
        report: &PersonalReport,
        path: &str,
        printed: &mut BTreeMap<String, String>,
        fragments: &mut Vec<Fragment>,
    ) {
        let slice = &lines[self.start..];
        let Some(last) = slice.iter().rposition(|line| !line.trim().is_empty()) else {
            return;
        };
        let slice = &slice[..=last];
        let text = slice
            .iter()
            .copied()
            .filter(|line| anchor(line).is_none())
            .collect::<Vec<_>>()
            .join("\n");
        if text.trim().is_empty() {
            return;
        }
        let mut range = self
            .caption
            .and_then(|line| line.split_once(" · ").map(|(_, r)| markdown_text(r)));
        let mut source_ranges = None;
        let source = self.source.map(|i| &report.sources[i]);
        let reference = source
            .and_then(|s| report.evidence.get(&s.source_id))
            .and_then(|refs| refs.get(self.reference?));
        let literal_block = text.split_once("<pre><code>").and_then(|(before, rest)| {
            rest.rsplit_once("</code></pre>")
                .map(|(literal, after)| (before, literal, after))
        });
        let target = slice.iter().find_map(|line| original_link(line));
        let mut original = None;
        let mut literals = String::new();
        let body = if let Some((before, literal, after)) = literal_block {
            let literal = html_text(literal);
            if let Some(r) =
                reference.filter(|r| r.quote.replace("\r\n", "\n").replace('\r', "\n") == literal)
            {
                source_ranges = Some(json!(r.ranges));
            } else if let Some(c) = self.context {
                source_ranges =
                    context_literal(c, &literal, range.as_deref(), false).map(|(_, ranges)| ranges);
            }
            let before = markdown_text(before);
            let after = markdown_text(after);
            for piece in [&before, &literal, &after] {
                terms::append_literal(&mut literals, piece);
            }
            let body = format!("{before}{literal}{after}");
            if let Some(id) = self.anchor {
                printed.insert(id.to_owned(), literal);
            }
            body
        } else if let Some((target, quote, ranges)) = target.and_then(|id| {
            let displayed = printed.get(id)?.as_str();
            let (quote, ranges) = if let Some(r) = reference {
                (r.quote.replace("\r\n", "\n").replace('\r', "\n") == displayed)
                    .then(|| (displayed, json!(r.ranges)))?
            } else {
                context_literal(self.context?, displayed, range.as_deref(), true)?
            };
            Some((id, quote, ranges))
        }) {
            source_ranges = Some(ranges);
            original = Some(format!("{path}#{target}"));
            let caption = markdown_text(&text);
            terms::append_literal(&mut literals, &caption);
            terms::append_literal(&mut literals, quote);
            format!("{caption}\n{quote}")
        } else {
            if report.schema_version == 5 {
                if let Some(r) = reference {
                    let unit = self
                        .caption
                        .and_then(|line| line.split_once(" · "))
                        .map(|(_, unit)| unit);
                    let hash_line = format!("内容指纹：\x60{}\x60", r.quote_sha256);
                    if unit == Some(&crate::reports::evidence_text(&r.unit_id))
                        && slice.contains(&hash_line.as_str())
                    {
                        source_ranges = Some(json!(r.ranges));
                    }
                }
                range = slice.iter().find_map(|line| {
                    line.strip_prefix("文件版本：")?
                        .split_once("位置：")
                        .map(|(_, r)| markdown_text(r))
                });
            }
            let visible = markdown_text(&text);
            terms::append_literal(&mut literals, &visible);
            visible
        };
        let label = self
            .source
            .map(|i| format!("S{:03}", i + 1))
            .unwrap_or_default();
        let title = source.map_or_else(
            || "完整依据".to_owned(),
            |source| {
                format!(
                    "{label} · {}",
                    source.relative_path.as_deref().unwrap_or("用户补充原文")
                )
            },
        );
        let link = self
            .anchor
            .map_or_else(|| path.to_owned(), |a| format!("{path}#{a}"));
        terms::append_literal(&mut literals, &label);
        if let Some(path) = source.and_then(|s| s.relative_path.as_deref()) {
            terms::append_literal(&mut literals, path);
        }
        fragments.push(Fragment { title, body, sources: label, literals, location: Some(json!({
            "attachment":path, "anchor":self.anchor, "link":link,
            "line_start":self.start + 1, "line_end":self.start + slice.len(),
            "source_id":source.map(|s| &s.source_id), "relative_path":source.and_then(|s| s.relative_path.as_deref()),
            "range":range, "source_ranges":source_ranges, "original_link":original,
        })) });
    }
}
