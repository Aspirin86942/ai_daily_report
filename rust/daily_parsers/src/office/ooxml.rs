use super::parts::{attr_value, validate_xml, PartBudget};
use office_oxide::core::{
    content_types::ContentTypes,
    opc::PartName,
    relationships::{rel_types, Relationships},
};
use quick_xml::events::Event;
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Seek, Write};
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

// Copy the text parser's dependencies, including XML with nonstandard names.
// Media bytes are unused by both Markdown projections. The relationships and
// drawing markup still carry image descriptions and their source positions.
pub(super) fn bounded_document<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    budget: &mut PartBudget,
    word: bool,
    include_notes: bool,
) -> Result<Cursor<Vec<u8>>, String> {
    let mut indexes = HashMap::new();
    for (index, name) in zip.file_names().enumerate() {
        indexes
            .entry(part_key(name))
            .and_modify(|entry| *entry = None)
            .or_insert(Some(index));
    }
    let mut package = TextPackage {
        zip,
        budget,
        indexes,
        copied: HashSet::new(),
        writer: ZipWriter::new(Cursor::new(Vec::new())),
        events_left: 160_000,
    };
    let types = package
        .copy("[Content_Types].xml")?
        .ok_or("missing Office content types")?;
    let root = package.relationships("_rels/.rels")?;
    let main = if let Some(relation) = root.first_by_type(rel_types::OFFICE_DOCUMENT) {
        root.resolve_target_from_root(&relation.id)
            .map_err(|e| e.to_string())?
    } else {
        let types = ContentTypes::parse(&types).map_err(|e| e.to_string())?;
        let mime = if word {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"
        } else {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"
        };
        types
            .overrides()
            .iter()
            .find(|(_, kind)| kind.as_str() == mime)
            .map(|(part, _)| part.clone())
            .ok_or("missing Office document part")?
    };
    let body = package
        .copy(main.as_str())?
        .ok_or("missing Office document body")?;
    let links = package.relationships(&main.rels_path())?;
    package.copy_first(&main, &links, rel_types::THEME)?;
    if word {
        package.copy_first(&main, &links, rel_types::STYLES)?;
        package.copy_first(&main, &links, rel_types::NUMBERING)?;
        for id in references(&body, true)?.into_iter().flatten() {
            if let Ok(part) = links.resolve_target(&id, &main) {
                package.copy(part.as_str())?;
            }
        }
    } else {
        for (index, id) in references(&body, false)?.into_iter().enumerate() {
            let slide = match id {
                Some(id) if !id.is_empty() => links.resolve_target(&id, &main),
                _ => PartName::new(&format!("/ppt/slides/slide{}.xml", index + 1)),
            };
            if let Ok(slide) = slide {
                package.copy(slide.as_str())?;
                let links = package.relationships(&slide.rels_path())?;
                if include_notes {
                    package.copy_first(&slide, &links, rel_types::NOTES_SLIDE)?;
                }
            }
        }
    }
    package.writer.finish().map_err(|e| e.to_string())
}

fn part_key(name: &str) -> String {
    name.trim_start_matches('/')
        .replace('\\', "/")
        .to_ascii_lowercase()
}

struct TextPackage<'a, R> {
    zip: &'a mut ZipArchive<R>,
    budget: &'a mut PartBudget,
    indexes: HashMap<String, Option<usize>>,
    copied: HashSet<usize>,
    writer: ZipWriter<Cursor<Vec<u8>>>,
    events_left: usize,
}

impl<R: Read + Seek> TextPackage<'_, R> {
    fn copy(&mut self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(index) = self.indexes.get(&part_key(name)) else {
            return Ok(None);
        };
        let index = index.ok_or("ambiguous Office XML part")?;
        if !self.copied.insert(index) {
            return Ok(None);
        }
        let (xml, partial) = self.budget.read_index(self.zip, index, 16 * 1024 * 1024)?;
        if partial {
            return Err("Office text parts exceed their decompression budget".into());
        }
        let name = self
            .zip
            .name_for_index(index)
            .ok_or("Office part missing")?
            .replace('\\', "/");
        let transcoded = office_oxide::core::xml::ensure_utf8(&xml);
        validate_xml(
            transcoded.as_deref().unwrap_or(&xml),
            self.budget.deadline,
            &mut self.events_left,
        )?;
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        self.writer
            .start_file(name, options)
            .map_err(|e| e.to_string())?;
        self.writer.write_all(&xml).map_err(|e| e.to_string())?;
        Ok(Some(transcoded.unwrap_or(xml)))
    }

    fn relationships(&mut self, name: &str) -> Result<Relationships, String> {
        match self.copy(name)? {
            Some(xml) => Relationships::parse(&xml).map_err(|e| e.to_string()),
            None => Ok(Relationships::empty()),
        }
    }

    fn copy_first(
        &mut self,
        source: &PartName,
        links: &Relationships,
        kind: &str,
    ) -> Result<(), String> {
        if let Some(relation) = links.first_by_type(kind) {
            let part = source
                .resolve_relative(&relation.target)
                .map_err(|e| e.to_string())?;
            self.copy(part.as_str())?;
        }
        Ok(())
    }
}

fn references(xml: &[u8], word: bool) -> Result<Vec<Option<String>>, String> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut references = Vec::new();
    let mut in_slide_list = false;
    loop {
        match reader.read_event().map_err(|e| e.to_string())? {
            Event::Start(tag) if !word && tag.local_name().as_ref() == b"sldIdLst" => {
                references.clear();
                in_slide_list = true;
            }
            Event::End(tag) if !word && tag.local_name().as_ref() == b"sldIdLst" => {
                in_slide_list = false
            }
            Event::Start(tag) | Event::Empty(tag)
                if in_slide_list && tag.local_name().as_ref() == b"sldId"
                    || word
                        && matches!(
                            tag.local_name().as_ref(),
                            b"headerReference" | b"footerReference"
                        ) =>
            {
                // The unqualified PPTX id is numeric, and r:id is the relationship.
                let id = tag
                    .attributes()
                    .with_checks(false)
                    .flatten()
                    .find(|attr| {
                        let name = attr.key.as_ref();
                        name.ends_with(b":id")
                    })
                    .map(|attr| {
                        attr.decoded_and_normalized_value(
                            quick_xml::XmlVersion::Implicit1_0,
                            tag.decoder(),
                        )
                        .map(|id| id.into_owned())
                        .map_err(|e| e.to_string())
                    })
                    .transpose()?;
                references.push(if word && id.is_none() {
                    attr_value(&tag, b"id")?
                } else {
                    id
                });
            }
            Event::Eof => return Ok(references),
            _ => {}
        }
    }
}
