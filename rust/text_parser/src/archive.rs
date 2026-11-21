use crate::parse_csv;
use ai_daily_worker_contract::archive::{ArchiveArtifact, ArchiveTextRequest, ArchiveWriter};
use ai_daily_worker_contract::{ContentUnitKind, SourceRange, SourceRangeUnit};
use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant};

pub fn parse_archive_text(request: &ArchiveTextRequest) -> Result<ArchiveArtifact, &'static str> {
    request.validate().map_err(|_| "PARSER_INVALID_PAYLOAD")?;
    if std::fs::metadata(&request.file_path)
        .map_err(|_| "SOURCE_UNAVAILABLE")?
        .len()
        > request.max_input_bytes
    {
        return Err("FILE_TOO_LARGE");
    }
    let started = Instant::now();
    let deadline = started + Duration::from_millis(request.timeout_ms);
    let csv = request.file_type == ".csv";
    let mut encodings = if csv {
        Vec::new()
    } else {
        vec![encoding_rs::UTF_8]
    };
    if csv {
        for name in &request.csv_encodings {
            let encoding = if name == "gb18030" {
                encoding_rs::GB18030
            } else {
                encoding_rs::UTF_8
            };
            if !encodings.contains(&encoding) {
                encodings.push(encoding);
            }
        }
    }
    for (attempt, encoding) in encodings.into_iter().enumerate() {
        if attempt > 0 {
            // Only files created in this request's private staging directory.
            for name in ["body.md", "units.jsonl", "units.idx"] {
                std::fs::remove_file(request.output_dir.join(name))
                    .map_err(|_| "ARCHIVE_OUTPUT_FAILED")?;
            }
        }
        let mut writer = ArchiveWriter::new(&request.output_dir, request.max_output_bytes)
            .map_err(|_| "ARCHIVE_OUTPUT_FAILED")?;
        let mut decoder = encoding.new_decoder_with_bom_removal();
        let mut file = File::open(&request.file_path).map_err(|_| "SOURCE_UNAVAILABLE")?;
        let mut bytes = [0; 65536];
        let mut record = String::new();
        let mut quoted = false;
        let mut row = 1;
        let mut offset = 0;
        let mut failed = false;
        let mut stopped = false;
        loop {
            if Instant::now() >= deadline {
                return Err("PARSE_DEADLINE");
            }
            let size = file.read(&mut bytes).map_err(|_| "SOURCE_UNAVAILABLE")?;
            let mut decoded = String::with_capacity(512 * 1024);
            let (result, consumed) = decoder.decode_to_string_without_replacement(
                &bytes[..size],
                &mut decoded,
                size == 0,
            );
            if result != encoding_rs::DecoderResult::InputEmpty || consumed != size {
                failed = true;
                break;
            }
            for character in decoded.chars() {
                record.push(character);
                if csv && character == '"' {
                    quoted = !quoted;
                }
                if (character == '\n' && (!csv || !quoted)) || (!csv && record.len() >= 256 * 1024)
                {
                    if !archive_text_record(&mut writer, &record, csv, row, offset) {
                        stopped = true;
                        break;
                    }
                    offset += record.chars().count() as u64;
                    row += 1;
                    record.clear();
                }
                if csv && record.len() >= 8 * 1024 * 1024 {
                    writer.gap("CSV_RECORD_COARSE");
                    let range = SourceRange::new(
                        SourceRangeUnit::CsvRecord,
                        row,
                        row + 1,
                        Some("raw_csv_fragment".into()),
                    );
                    if !writer.push(&record, ContentUnitKind::Text, vec![range]) {
                        stopped = true;
                        break;
                    }
                    record.clear();
                }
            }
            if stopped || size == 0 {
                break;
            }
        }
        if failed {
            drop(writer);
            continue;
        }
        if csv && quoted {
            return Err("CSV_INVALID");
        }
        if !record.is_empty() && !stopped {
            archive_text_record(&mut writer, &record, csv, row, offset);
        }
        let mut artifact = writer.finish();
        if artifact.error.is_some() {
            return Err("ARCHIVE_OUTPUT_FAILED");
        }
        artifact.raw_sha256 = request.raw_sha256.clone();
        artifact.parser_backend = "light_text_v2".into();
        artifact.worker_lane = "rust_text_archive_process_v4".into();
        artifact.duration_ms = started.elapsed().as_millis() as u64;
        if artifact.usable_chars == 0 {
            artifact.gaps.push("NO_USABLE_TEXT".into());
        }
        return Ok(artifact);
    }
    Err("TEXT_DECODE_FAILED")
}

fn archive_text_record(
    writer: &mut ArchiveWriter,
    text: &str,
    csv: bool,
    row: u64,
    offset: u64,
) -> bool {
    if csv {
        let range = SourceRange::new(SourceRangeUnit::CsvRecord, row, row + 1, None);
        match parse_csv(text, 8 * 1024 * 1024, false) {
            Ok(rows) => {
                for cells in rows {
                    let escaped = cells
                        .iter()
                        .map(|v| {
                            v.replace('\\', "\\\\")
                                .replace('|', "\\|")
                                .replace('\r', "")
                                .replace('\n', "<br>")
                        })
                        .collect::<Vec<_>>();
                    if !writer.push(
                        &format!("| {} |", escaped.join(" | ")),
                        if row == 1 {
                            ContentUnitKind::TableHeader
                        } else {
                            ContentUnitKind::TableRow
                        },
                        vec![range.clone()],
                    ) {
                        return false;
                    }
                }
            }
            Err(_) => {
                writer.gap("CSV_RECORD_INVALID");
                return writer.push(text, ContentUnitKind::Text, vec![range]);
            }
        }
        true
    } else {
        let end = offset + text.chars().count() as u64;
        if end == offset {
            return true;
        }
        writer.push(
            text,
            if text.trim_start().starts_with('#') {
                ContentUnitKind::Heading
            } else {
                ContentUnitKind::Text
            },
            vec![SourceRange::new(
                SourceRangeUnit::TextChar,
                offset,
                end,
                None,
            )],
        )
    }
}
