use crate::contract::{AuditTextBuilder, ContentUnitKind, SourceRange, SourceRangeUnit};
use crate::office::parts::{self, attr_value, unescape_general_ref, unescape_text};
use office_oxide::core::opc::PartName;
use office_oxide::core::relationships::{rel_types, Relationships, TargetMode};
use office_oxide::pptx::{GraphicContent, Shape};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::time::Instant;
use zip::read::ZipArchive;

const CHART: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/chart";
const DRAWING: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
const SLIDE: &[u8] = b"http://schemas.openxmlformats.org/presentationml/2006/main";
const LINK: &[u8] = b"http://schemas.openxmlformats.org/officeDocument/2006/relationships";

pub(super) fn may_have_chart(shapes: &[Shape]) -> bool {
    shapes.iter().any(|s| match s {
        Shape::Group(group) => may_have_chart(&group.children),
        Shape::GraphicFrame(frame) => matches!(frame.content, GraphicContent::Unknown),
        _ => false,
    })
}

pub(super) struct SlideCharts<'a, R> {
    zip: &'a mut ZipArchive<R>,
    parts: parts::PartBudget,
    presentation: PartName,
    links: Relationships,
}

fn target(links: &Relationships, id: &str, kind: &str, source: &PartName) -> Option<PartName> {
    let rel = links.get_by_id(id)?;
    if rel.target_mode != TargetMode::Internal
        || rel.rel_type != kind
        || rel.target.contains(':')
        || rel.target.starts_with("//")
    {
        return None;
    }
    links.resolve_target(id, source).ok()
}

impl<'a, R: Read + Seek> SlideCharts<'a, R> {
    pub fn open(zip: &'a mut ZipArchive<R>, mut parts: parts::PartBudget) -> Option<Self> {
        let presentation = parts.main_part(zip)?;
        let xml = parts.complete(zip, &presentation.rels_path(), 1024 * 1024)?;
        let links = Relationships::parse(&xml).ok()?;
        Some(Self {
            zip,
            parts,
            presentation,
            links,
        })
    }

    pub fn append(
        &mut self,
        slide_id: &str,
        number: u64,
        out: &mut AuditTextBuilder,
    ) -> Option<bool> {
        let slide = target(&self.links, slide_id, rel_types::SLIDE, &self.presentation)?;
        let xml = self
            .parts
            .complete(self.zip, slide.as_str(), 4 * 1024 * 1024)?;
        let refs = chart_frames(&xml, self.parts.deadline)?;
        if refs.is_empty() {
            return Some(false);
        }
        let xml = self
            .parts
            .complete(self.zip, &slide.rels_path(), 1024 * 1024)?;
        let links = Relationships::parse(&xml).ok()?;
        let mut limited = false;
        for (frame, id) in refs {
            let Some(part) = target(&links, &id, rel_types::CHART, &slide) else {
                limited = true;
                continue;
            };
            let Some(data) = self
                .parts
                .complete(self.zip, part.as_str(), 4 * 1024 * 1024)
            else {
                limited = true;
                continue;
            };
            let label = format!("{}#{} → {}", slide.as_str(), frame, part.as_str());
            if label.chars().count() > 512 {
                limited = true;
                continue;
            }
            let range = SourceRange::new(SourceRangeUnit::Slide, number, number + 1, Some(label));
            out.read(range.clone());
            limited |= render_chart(&data, self.parts.deadline, out, &range).unwrap_or(true);
        }
        Some(limited)
    }
}

// Follow only the chart in a slide graphic frame (including nested shape groups).
fn chart_frames(xml: &[u8], deadline: Instant) -> Option<Vec<(String, String)>> {
    let mut reader = quick_xml::NsReader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut result = Vec::new();
    let mut in_tree = false;
    let mut frame = None;
    let mut chart_data = false;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        let (ns, event) = reader.read_resolved_event().ok()?;
        let namespace = match ns {
            ResolveResult::Bound(s) => s.0,
            _ => b"",
        };
        match event {
            Event::Start(e) => match (namespace, e.local_name().as_ref()) {
                (SLIDE, b"spTree") => in_tree = true,
                (SLIDE, b"graphicFrame") if in_tree => frame = Some(String::new()),
                (SLIDE, b"cNvPr") if frame.is_some() => frame = attr_value(&e, b"id").ok()?,
                (DRAWING, b"graphicData") if frame.is_some() => {
                    chart_data = attr_value(&e, b"uri")
                        .ok()?
                        .is_some_and(|s| s.as_bytes() == CHART)
                }
                (CHART, b"chart") if chart_data => {
                    for a in e.attributes() {
                        let a = a.ok()?;
                        let (ns, name) = reader.resolver().resolve_attribute(a.key);
                        if name.as_ref() == b"id"
                            && matches!(ns, ResolveResult::Bound(s) if s.as_ref() == LINK)
                        {
                            let id = a
                                .decoded_and_normalized_value(
                                    quick_xml::XmlVersion::Implicit1_0,
                                    e.decoder(),
                                )
                                .ok()?;
                            result.push((frame.as_ref()?.clone(), id.into_owned()));
                        }
                    }
                }
                _ => {}
            },
            Event::End(e) => match (namespace, e.local_name().as_ref()) {
                (DRAWING, b"graphicData") => chart_data = false,
                (SLIDE, b"graphicFrame") => frame = None,
                (SLIDE, b"spTree") => in_tree = false,
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Some(result)
}

#[derive(Default)]
struct ChartValue {
    raw: String,
    format: Option<String>,
}

#[derive(Default)]
struct Series {
    name: String,
    formats: BTreeMap<String, String>,
    points: BTreeMap<u32, BTreeMap<String, ChartValue>>,
}

// Only the fixed-decimal percentage subset is presented as a display value.
// Scale the original decimal text exactly and round ties away from zero.
fn simple_percentage(raw: &str, format: &str) -> Option<String> {
    let places = match format.strip_suffix('%')? {
        "0" => 0,
        pattern => {
            let decimals = pattern.strip_prefix("0.")?;
            if decimals.is_empty() || decimals.len() > 9 || !decimals.bytes().all(|b| b == b'0') {
                return None;
            }
            decimals.len()
        }
    };
    let raw = raw.trim();
    let (mantissa, exponent) = if let Some((value, exp)) = raw.split_once(['e', 'E']) {
        (value, exp.parse::<i32>().ok()?)
    } else {
        (raw, 0)
    };
    let negative = mantissa.starts_with('-');
    let unsigned = mantissa.strip_prefix(['-', '+']).unwrap_or(mantissa);
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let coefficient = whole
        .bytes()
        .chain(fraction.bytes())
        .try_fold(0u128, |n, b| {
            n.checked_mul(10)?
                .checked_add(u128::from(char::from(b).to_digit(10)?))
        })?;
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    let shift = exponent
        .checked_add(places as i32 + 2)?
        .checked_sub(i32::try_from(fraction.len()).ok()?)?;
    let scale = 10u128.checked_pow(shift.unsigned_abs())?;
    let rounded = if shift < 0 {
        coefficient / scale + u128::from(coefficient % scale >= scale / 2)
    } else {
        coefficient.checked_mul(scale)?
    };
    let sign = if negative && coefficient != 0 {
        "-"
    } else {
        ""
    };
    let scale = 10u128.pow(places as u32);
    Some(if places == 0 {
        format!("{sign}{rounded}%")
    } else {
        format!("{sign}{}.{:0places$}%", rounded / scale, rounded % scale)
    })
}

impl Series {
    fn emit(
        &self,
        plot: &str,
        ordinal: usize,
        out: &mut AuditTextBuilder,
        range: &SourceRange,
    ) -> bool {
        let prefix = if self.name.is_empty() {
            format!("图表 {plot}，系列 {ordinal}")
        } else {
            format!("图表 {plot}，系列 {}", self.name)
        };
        if self.points.is_empty() {
            out.push(&prefix, ContentUnitKind::Slide, vec![range.clone()]);
            return true;
        }
        let mut limited = false;
        for (index, fields) in &self.points {
            let mut line = format!("{prefix}，点 {index}");
            for (key, label) in [
                ("cat", "类别"),
                ("xVal", "X"),
                ("val", "缓存值"),
                ("yVal", "Y"),
                ("bubbleSize", "气泡大小"),
            ] {
                if let Some(v) = fields.get(key) {
                    line.push_str(&format!("，{label} {}", v.raw));
                    let cached_format = self.formats.get(key);
                    if let Some(format) = cached_format {
                        line.push_str(&format!("，缓存格式码 {format}"));
                    }
                    if let Some(format) = v.format.as_ref().filter(|f| Some(*f) != cached_format) {
                        line.push_str(&format!("，数据点格式码 {format}"));
                    }
                    if let Some(display) = v
                        .format
                        .as_ref()
                        .or(cached_format)
                        .and_then(|f| simple_percentage(&v.raw, f))
                    {
                        line.push_str(&format!("，按格式码显示值 {display}"));
                    }
                }
            }
            if !out.push(&line, ContentUnitKind::Slide, vec![range.clone()]) {
                limited = true;
                break;
            }
        }
        limited
    }
}

// Use actual pt/@idx for every role. An absent category or value stays absent.
// Formula references, linked workbooks, axis ticks and data labels are outside these paths.
fn render_chart(
    xml: &[u8],
    deadline: Instant,
    out: &mut AuditTextBuilder,
    range: &SourceRange,
) -> Option<bool> {
    let mut reader = quick_xml::NsReader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut path: Vec<String> = Vec::new();
    let mut series = Series::default();
    let mut point = None;
    let mut point_format = None;
    let mut value = String::new();
    let mut ordinal = 0;
    let mut limited = false;
    loop {
        if Instant::now() >= deadline {
            return Some(true);
        }
        let (ns, event) = reader.read_resolved_event().ok()?;
        let chart = matches!(ns, ResolveResult::Bound(s) if s.as_ref() == CHART);
        match event {
            Event::Start(e) => {
                if path.len() >= 64 {
                    return None;
                }
                let tag = if chart {
                    std::str::from_utf8(e.local_name().as_ref())
                        .ok()?
                        .to_owned()
                } else {
                    String::new()
                };
                if tag == "pt" {
                    point_format = attr_value(&e, b"formatCode").ok()?;
                    point = attr_value(&e, b"idx")
                        .ok()?
                        .and_then(|v| v.parse::<u32>().ok());
                }
                if matches!(tag.as_str(), "v" | "formatCode") {
                    value.clear();
                }
                path.push(tag);
            }
            ev @ (Event::Text(_) | Event::GeneralRef(_))
                if path
                    .last()
                    .is_some_and(|v| matches!(v.as_str(), "v" | "formatCode")) =>
            {
                let text = match ev {
                    Event::Text(e) => unescape_text(&e),
                    Event::GeneralRef(e) => unescape_general_ref(&e),
                    _ => unreachable!(),
                }
                .ok()?;
                value.push_str(&text);
            }
            Event::End(_) => {
                let tags: Vec<_> = path.iter().map(String::as_str).collect();
                if let ["chartSpace", "chart", "plotArea", plot, "ser", tail @ ..] = tags.as_slice()
                {
                    if plot.ends_with("Chart") {
                        match tail {
                            [] => {
                                ordinal += 1;
                                limited |= series.emit(plot, ordinal, out, range);
                                series = Series::default();
                            }
                            ["tx", "v"] | ["tx", "strRef", "strCache", "pt", "v"] => {
                                series.name = value.clone()
                            }
                            [role, "numRef", "numCache", "formatCode"]
                            | [role, "numLit", "formatCode"]
                                if matches!(
                                    *role,
                                    "cat" | "val" | "xVal" | "yVal" | "bubbleSize"
                                ) =>
                            {
                                series
                                    .formats
                                    .insert((*role).into(), std::mem::take(&mut value));
                            }
                            [role, trail @ ..]
                                if matches!(
                                    *role,
                                    "cat" | "val" | "xVal" | "yVal" | "bubbleSize"
                                ) && matches!(
                                    trail,
                                    ["numRef", "numCache", "pt", "v"]
                                        | ["strRef", "strCache", "pt", "v"]
                                        | ["numLit" | "strLit", "pt", "v"]
                                        | ["multiLvlStrRef", "multiLvlStrCache", "lvl", "pt", "v"]
                                ) =>
                            {
                                if let Some(index) = point {
                                    if series.points.len() < 4096
                                        || series.points.contains_key(&index)
                                    {
                                        let field = series
                                            .points
                                            .entry(index)
                                            .or_default()
                                            .entry((*role).into())
                                            .or_default();
                                        if !field.raw.is_empty() {
                                            field.raw.push_str(" / ");
                                        }
                                        field.raw.push_str(&value);
                                        field.format = point_format.take();
                                    } else {
                                        limited = true;
                                    }
                                } else {
                                    limited = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if path.pop()?.as_str() == "pt" {
                    point = None;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Some(limited || ordinal == 0)
}
