use pdf_oxide::{converters::ConversionOptions, document::PdfDocument, error::Error};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::Path;

fn extract(path: &Path, mode: &str) -> Result<Value, Error> {
    let doc = PdfDocument::open(path)?;
    if !doc.is_authenticated() {
        return Err(Error::EncryptedPdf);
    }
    let count = doc.page_count()?;
    if !(1..=50).contains(&count) {
        return Ok(json!({"status": "invalid_page_count", "pages": []}));
    }
    let options = ConversionOptions {
        extract_tables: mode != "extract-no-tables",
        ..Default::default()
    };
    let mut pages = Vec::with_capacity(count);
    let mut has_text = false;
    for index in 0..count {
        let text = match mode {
            "layout" => doc.to_plain_text(index, &options)?,
            "extract-no-tables" => doc.extract_text_with_options(index, &options)?,
            _ => doc.extract_text(index)?,
        };
        has_text |= !text.trim().is_empty();
        pages.push(json!({"page": index + 1, "text": text}));
    }
    Ok(json!({"status": if has_text { "text" } else { "no_text" }, "pages": pages}))
}

fn emit(output: &mut impl Write, value: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, &value)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).unwrap_or_default();
    if !matches!(mode.as_str(), "extract" | "layout" | "extract-no-tables") {
        return Err("expected extract, layout, or extract-no-tables".into());
    }
    let corpus = std::env::current_dir()?.join("corpus").canonicalize()?;
    let mut output = io::stdout().lock();
    emit(&mut output, json!({"ready": true}))?;
    for line in io::stdin().lock().lines() {
        let path: String = serde_json::from_str(&line?)?;
        let path = Path::new(&path).canonicalize()?;
        if path.parent() != Some(corpus.as_path()) {
            return Err("only staged benchmark fixtures are accepted".into());
        }
        let result = extract(&path, &mode).unwrap_or_else(|error| {
            let status = match error {
                Error::EncryptedPdf => "protected",
                _ => "parse_error",
            };
            json!({"status": status, "pages": []})
        });
        emit(&mut output, result)?;
    }
    Ok(())
}
