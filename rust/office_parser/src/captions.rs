use crate::parts::{self, attr_value, unescape_general_ref, unescape_text};
use office_oxide::docx::{BlockElement, DocxDocument};
use quick_xml::events::Event;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;
use std::time::Instant;
use zip::read::ZipArchive;

pub(super) struct Caption {
    pub text: String,
    pub paragraph: Option<u64>,
}

enum Block {
    Paragraph(Option<Caption>),
    Table(Option<String>),
}

fn caption_style(doc: &DocxDocument, index: usize) -> bool {
    let Some(BlockElement::Paragraph(p)) = doc.body.elements.get(index) else {
        return false;
    };
    let mut id = p.properties.as_ref().and_then(|p| p.style_id.as_deref());
    let is_caption = |s: &str| {
        s.eq_ignore_ascii_case("caption")
            || s.eq_ignore_ascii_case("table caption")
            || matches!(s, "题注" | "表格题注" | "表题")
    };
    for _ in 0..16 {
        let Some(style_id) = id else {
            break;
        };
        if is_caption(style_id) {
            return true;
        }
        let style = doc.styles.as_ref().and_then(|s| s.styles.get(style_id));
        if style
            .and_then(|s| s.name.as_deref())
            .is_some_and(is_caption)
        {
            return true;
        }
        id = style.and_then(|s| s.based_on.as_deref());
    }
    false
}

fn table_field(s: &str) -> bool {
    let mut words = s.split_whitespace();
    words.next().is_some_and(|w| w.eq_ignore_ascii_case("SEQ"))
        && words.next().is_some_and(|w| {
            matches!(w.trim_matches('"'), "表" | "表格")
                || w.trim_matches('"').eq_ignore_ascii_case("table")
        })
}

// Only a standalone decimal ordinal can disambiguate a caption between tables.
// A chapter/section number belongs to the document's numbering scheme.
fn global_table_number(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("table")
        .or_else(|| lower.strip_prefix('表'))?
        .trim_start();
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (number, tail) = rest.split_at(digits);
    if !tail.is_empty() {
        let title = tail
            .strip_prefix([':', '：'])
            .or_else(|| tail.strip_prefix(char::is_whitespace))?
            .trim_start();
        if title.starts_with(|c: char| c.is_numeric() || "-‐‑–—－.．·/、".contains(c)) {
            return None;
        }
    }
    number.parse().ok()
}

// Match the same top-level block sequence as office_oxide. Ambiguous adjacency and
// a different block sequence cannot supply an attributed table caption.
pub(super) fn read(
    path: &Path,
    doc: &DocxDocument,
    parts: &mut parts::PartBudget,
) -> Option<(BTreeMap<usize, Caption>, bool)> {
    let mut zip = ZipArchive::new(File::open(path).ok()?).ok()?;
    let main = parts.main_part(&mut zip)?;
    let xml = parts.complete(&mut zip, main.as_str(), 8 * 1024 * 1024)?;
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    reader.config_mut().trim_text(false);
    let mut stack = Vec::new();
    let mut blocks = Vec::new();
    let mut paragraph_number = 0;
    let mut text = String::new();
    let mut field = String::new();
    let mut marked = false;
    let mut native = None;
    let mut block_depth = None;
    let mut paragraph = false;
    loop {
        if Instant::now() >= parts.deadline {
            return None;
        }
        match reader.read_event().ok()? {
            ev @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(ev, Event::Empty(_));
                let (Event::Start(e) | Event::Empty(e)) = ev else {
                    unreachable!()
                };
                let tag = e.local_name();
                let tag = tag.as_ref();
                if stack.len() >= 128 {
                    return None;
                }
                if !empty
                    && block_depth.is_none()
                    && stack.iter().any(|v: &Vec<u8>| v == b"body")
                    && matches!(tag, b"p" | b"tbl")
                {
                    block_depth = Some(stack.len());
                    paragraph = tag == b"p";
                    native = None;
                    marked = false;
                    text.clear();
                    field.clear();
                } else if let Some(depth) = block_depth {
                    if !paragraph
                        && tag == b"tblCaption"
                        && stack.len() == depth + 2
                        && stack.last().is_some_and(|s| s == b"tblPr")
                    {
                        native = attr_value(&e, b"val")
                            .ok()?
                            .filter(|s| !s.trim().is_empty() && s.chars().count() <= 512);
                    } else if paragraph && tag == b"fldSimple" {
                        marked |= attr_value(&e, b"instr")
                            .ok()?
                            .is_some_and(|s| table_field(&s));
                    }
                }
                if !empty {
                    stack.push(tag.to_vec());
                }
            }
            Event::Text(_) | Event::GeneralRef(_) if block_depth.is_none() => {}
            ev @ (Event::Text(_) | Event::GeneralRef(_)) if paragraph => {
                let tag = stack.last()?.as_slice();
                if matches!(tag, b"t" | b"instrText")
                    && !stack
                        .iter()
                        .any(|s| matches!(s.as_slice(), b"del" | b"drawing" | b"txbxContent"))
                {
                    let out = if tag == b"t" { &mut text } else { &mut field };
                    if out.len() < 2048 {
                        let part = match ev {
                            Event::Text(e) => unescape_text(&e),
                            Event::GeneralRef(e) => unescape_general_ref(&e),
                            _ => unreachable!(),
                        }
                        .ok()?;
                        out.extend(part.chars().take(513));
                    }
                }
            }
            Event::End(_) => {
                stack.pop()?;
                if block_depth == Some(stack.len()) {
                    let expected = doc.body.elements.get(blocks.len())?;
                    if paragraph != matches!(expected, BlockElement::Paragraph(_)) {
                        return None;
                    }
                    if paragraph {
                        paragraph_number += 1;
                        let text = text.trim();
                        let visible = !text.is_empty() && text.chars().count() <= 512;
                        let styled = caption_style(doc, blocks.len())
                            && !text.starts_with('图')
                            && !text.to_ascii_lowercase().starts_with("figure");
                        let caption =
                            (visible && (styled || marked || table_field(&field))).then(|| {
                                Caption {
                                    text: text.to_owned(),
                                    paragraph: Some(paragraph_number),
                                }
                            });
                        blocks.push(Block::Paragraph(caption));
                    } else {
                        blocks.push(Block::Table(native.take()));
                    }
                    block_depth = None;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if blocks.len() != doc.body.elements.len() {
        return None;
    }
    let mut captions = BTreeMap::new();
    let mut number = 0;
    for (index, block) in blocks.iter().enumerate() {
        let Block::Table(intrinsic) = block else {
            continue;
        };
        number += 1;
        if let Some(title) = intrinsic {
            captions.insert(
                number,
                Caption {
                    text: title.clone(),
                    paragraph: None,
                },
            );
            continue;
        }
        let mut candidates = [index.checked_sub(1), Some(index + 1)]
            .into_iter()
            .flatten()
            .filter_map(|i| {
                let Block::Paragraph(Some(caption)) = blocks.get(i)? else {
                    return None;
                };
                let shared = i
                    .checked_sub(1)
                    .and_then(|n| blocks.get(n))
                    .is_some_and(|b| matches!(b, Block::Table(_)))
                    && blocks
                        .get(i + 1)
                        .is_some_and(|b| matches!(b, Block::Table(_)));
                (!shared || global_table_number(&caption.text) == Some(number)).then_some(caption)
            });
        if let Some(caption) = candidates.next() {
            if candidates.next().is_none() {
                captions.insert(
                    number,
                    Caption {
                        text: caption.text.clone(),
                        paragraph: caption.paragraph,
                    },
                );
            }
        }
    }
    let linked: BTreeSet<_> = captions.values().filter_map(|c| c.paragraph).collect();
    let ambiguous = blocks.windows(3).any(|w| {
        matches!(w,
        [Block::Table(_), Block::Paragraph(Some(c)), Block::Table(_)]
            if c.paragraph.is_some_and(|n| !linked.contains(&n)))
    });
    Some((captions, ambiguous))
}
