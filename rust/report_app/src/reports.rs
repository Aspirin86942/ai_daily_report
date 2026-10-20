use crate::model::contracts::CitedText;
use crate::ReportError;
use chrono::{DateTime, FixedOffset};
use minijinja::{context, Environment, Error, ErrorKind, Value};
use serde_json::{json, Value as JsonValue};
use std::collections::{BTreeMap, BTreeSet};
pub mod data;
pub mod legacy;
pub mod period;
pub mod personal;

pub const DAILY_TEMPLATE_V3: &str =
    include_str!("../../../templates/evidence_report_template_v3.md");

pub const DAILY_TEMPLATE_V4: &str =
    include_str!("../../../templates/evidence_report_template_v4.md");

pub fn render_daily(report: &data::DailyReport, template: &str) -> Result<String, ReportError> {
    report.validate()?;
    let mut value = serde_json::to_value(report)?;
    let labels = report.source_labels();
    let gaps_grouped = grouped_gaps(report, &labels);
    let sources_usable = report
        .sources
        .iter()
        .filter(|source| source.usable())
        .count();
    let facts_pending = report.facts.iter().filter(|fact| fact.pending).count();
    value["speed"] = json!({
        "sources_total": report.sources.len(),
        "sources_usable": sources_usable,
        "sources_failed": report.sources.len() - sources_usable,
        "facts_total": report.facts.len(),
        "facts_confirmed": report.facts.len() - facts_pending,
        "facts_pending": facts_pending,
        "gaps_total": report.gaps.len(),
        "gaps_codes": gaps_grouped.len(),
        "quality": report.quality,
    });
    let mut matters_view = Vec::new();
    let mut overview_view = Vec::new();
    let mut pending_view = Vec::new();
    let mut toc = Vec::new();
    if let Some(content) = &report.content {
        for (index, matter) in content.matters.iter().enumerate() {
            let anchor = format!("m{}", index + 1);
            if content.matters.len() >= 10 {
                toc.push(json!({"anchor": anchor, "title": matter.title}));
            }
            let mut seen = BTreeSet::new();
            let details: Vec<_> = matter
                .details
                .iter()
                .filter(|detail| seen.insert((&detail.fact_ids, &detail.text)))
                .map(|detail| cited_view(detail, &labels, true))
                .collect();
            matters_view.push(json!({
                "anchor": anchor,
                "title": matter.title,
                "background": matter.background.as_ref().map(|item| cited_view(item, &labels, true)),
                "details": details,
                "status": matter.status.as_ref().map(|item| cited_view(item, &labels, false)),
            }));
        }
        overview_view = content
            .overview
            .iter()
            .map(|item| cited_view(item, &labels, false))
            .collect();
        pending_view = content
            .pending
            .iter()
            .map(|item| cited_view(item, &labels, false))
            .collect();
    }
    value["source_labels"] = serde_json::to_value(labels)?;
    value["gaps_grouped"] = json!(gaps_grouped);
    value["toc"] = json!(toc);
    value["matters_view"] = json!(matters_view);
    value["overview_view"] = json!(overview_view);
    value["pending_view"] = json!(pending_view);
    value["historical_reconstruction"] = JsonValue::Bool(report.is_historical()?);
    let body = render(template, &value, &report.generated_at)?;
    Ok(format!(
        "{}\n",
        body.replace("\r\n", "\n").replace('\r', "\n").trim_end()
    ))
}

fn grouped_gaps(report: &data::DailyReport, labels: &BTreeMap<String, String>) -> Vec<JsonValue> {
    struct Group<'a> {
        gap: &'a data::CoverageGap,
        labels: Vec<&'a str>,
        count: usize,
    }

    let mut groups: Vec<Group<'_>> = Vec::new();
    let mut indices = BTreeMap::new();
    for gap in &report.gaps {
        let index = *indices.entry(&gap.code).or_insert_with(|| {
            groups.push(Group {
                gap,
                labels: Vec::new(),
                count: 0,
            });
            groups.len() - 1
        });
        let group = &mut groups[index];
        group.count += 1;
        if let Some(label) = gap.source_id.as_ref().map(|id| labels[id].as_str()) {
            if !group.labels.contains(&label) {
                group.labels.push(label);
            }
        }
    }
    groups
        .into_iter()
        .map(|group| {
            let label_summary = if group.labels.len() > 6 {
                format!("{} 等 {} 个来源", group.labels[0], group.labels.len())
            } else {
                group.labels.join("、")
            };
            json!({
                "code": group.gap.code,
                "message": group.gap.message,
                "label_summary": label_summary,
                "count": group.count,
            })
        })
        .collect()
}

fn cited_view(item: &CitedText, labels: &BTreeMap<String, String>, split: bool) -> JsonValue {
    let paragraphs = if split {
        split_paragraphs(&item.text)
    } else {
        vec![item.text.as_str()]
    };
    let refs = item
        .source_ids
        .iter()
        .map(|id| {
            let label = &labels[id];
            format!("[{label}](#{})", label.to_ascii_lowercase())
        })
        .collect::<Vec<_>>()
        .join("、");
    json!({"paragraphs": paragraphs, "refs": refs})
}

fn split_paragraphs(text: &str) -> Vec<&str> {
    let chars = text.chars().count();
    if chars <= 240 {
        return vec![text];
    }
    let cuts: Vec<_> = text
        .char_indices()
        .enumerate()
        .filter_map(|(char_index, (byte_index, character))| {
            let end = byte_index + character.len_utf8();
            (matches!(character, '。' | '；') && end < text.len()).then_some((end, char_index + 1))
        })
        .collect();
    let count = chars.div_ceil(240).min(3).min(cuts.len() + 1);
    let mut paragraphs = Vec::with_capacity(count);
    let mut start = 0;
    let mut next_cut = 0;
    for part in 1..count {
        let target = chars * part / count;
        let last_cut = cuts.len() - (count - part);
        let selected = (next_cut..=last_cut)
            .min_by_key(|&index| cuts[index].1.abs_diff(target))
            .unwrap();
        let end = cuts[selected].0;
        paragraphs.push(&text[start..end]);
        start = end;
        next_cut = selected + 1;
    }
    paragraphs.push(&text[start..]);
    paragraphs
}

pub fn evidence_text(value: &str) -> String {
    let escaped = value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let mut output = String::with_capacity(escaped.len());
    for (index, ch) in escaped.chars().enumerate() {
        if "\\`*_|".contains(ch) || ch == '#' && index == 0 {
            output.push('\\');
        }
        output.push(if ch == '\r' || ch == '\n' { ' ' } else { ch });
    }
    output
}

pub fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.add_filter("evidence_text", |value: String| evidence_text(&value));
    env.add_filter("shanghai_time", |value: String| -> Result<String, Error> {
        let timestamp = DateTime::parse_from_rfc3339(&value)
            .map_err(|_| Error::new(ErrorKind::InvalidOperation, "invalid report timestamp"))?;
        Ok(timestamp
            .with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap())
            .format("%Y-%m-%d %H:%M:%S +08:00")
            .to_string())
    });
    // Only the two map methods used by the frozen historical template are needed.
    env.set_unknown_method_callback(|state, value, method, args| match method {
        "items" if args.is_empty() => state.apply_filter("items", std::slice::from_ref(value)),
        "get" if args.len() == 1 || args.len() == 2 => {
            let item = value.get_item(&args[0])?;
            Ok(if item.is_undefined() {
                args.get(1).cloned().unwrap_or(Value::from(()))
            } else {
                item
            })
        }
        _ => Err(Error::new(ErrorKind::UnknownMethod, method.to_owned())),
    });
    env
}

pub fn render(template: &str, report: &JsonValue, now: &str) -> Result<String, ReportError> {
    environment()
        .render_str(
            template,
            context! { report => template_value(report), now => now },
        )
        .map_err(|error| ReportError::new("TEMPLATE_INVALID", error.to_string()))
}

// serde_json's arbitrary_precision feature serializes numbers through a private
// map. Convert actual JSON values directly so that numeric report fields remain
// numbers in the template rather than exposing that internal representation.
pub(crate) fn template_value(value: &JsonValue) -> Value {
    match value {
        JsonValue::Null => Value::from(()),
        JsonValue::Bool(value) => Value::from(*value),
        JsonValue::String(value) => Value::from(value.clone()),
        JsonValue::Number(value) => {
            if let Some(number) = value.as_i64() {
                Value::from(number)
            } else if let Some(number) = value.as_u64() {
                Value::from(number)
            } else if value.is_f64() {
                Value::from(value.as_f64().expect("finite JSON number"))
            } else {
                Value::from(value.to_string())
            }
        }
        JsonValue::Array(values) => {
            Value::from(values.iter().map(template_value).collect::<Vec<_>>())
        }
        JsonValue::Object(values) => Value::from_pairs(
            values
                .iter()
                .map(|(key, value)| (key.clone(), template_value(value))),
        ),
    }
}
