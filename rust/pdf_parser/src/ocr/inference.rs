use super::OcrPage;
use crate::PdfError;
use rusto_core::{
    config::InitializeConfig,
    engine::RustO,
    types::{DetectTextResult, OcrRunOptions},
};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::Path,
};

#[derive(serde::Deserialize)]
struct ModelFile {
    name: String,
    size: usize,
    sha256: String,
}

fn verified_model(path: &Path, size: usize, digest: &str) -> Result<File, PdfError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, 1);
    let mut file = options.open(path).map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => PdfError::OcrModelMissing,
        _ => PdfError::OcrUnavailable,
    })?;
    if file.metadata().map_err(|_| PdfError::OcrUnavailable)?.len() != size as u64 {
        return Err(PdfError::OcrModelMismatch);
    }
    let mut bytes = Vec::with_capacity(size);
    (&mut file)
        .take(size as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| PdfError::OcrUnavailable)?;
    if bytes.len() != size || crate::sha256(&bytes) != digest {
        return Err(PdfError::OcrModelMismatch);
    }
    Ok(file)
}

pub(super) fn load(models: &Path) -> Result<RustO, PdfError> {
    let mut inputs: serde_json::Value =
        serde_json::from_str(crate::BUILD_INPUTS).expect("validated build manifest");
    let files: Vec<ModelFile> = serde_json::from_value(inputs["image_ocr"]["files"].take())
        .map_err(|_| PdfError::OcrModelMismatch)?;
    // Retain the read handles while RustO opens the same immutable files by path.
    let open = |name: &str| -> Result<_, PdfError> {
        let asset = files
            .iter()
            .find(|f| f.name == name)
            .ok_or(PdfError::OcrModelMismatch)?;
        let path = models.join(&asset.name);
        let handle = verified_model(&path, asset.size, &asset.sha256)?;
        Ok((path, handle))
    };
    let (detector, _detector_file) = open("det.mnn")?;
    let (recognizer, _recognizer_file) = open("rec.mnn")?;
    let (dictionary, _dictionary_file) = open("dict.txt")?;
    let mut config = InitializeConfig::ppv6(detector, recognizer, dictionary);
    config.det.limit_side_len = 960;
    config.det.limit_type = "max".into();
    config.det.max_candidates = 128;
    config.det.engine_cfg.intra_op_num_threads = 1;
    config.rec.engine_cfg.intra_op_num_threads = 1;
    config.rec.rec_workers = Some(1);
    config.rec.max_rec_workers = 1;
    config.rec.rec_batch_num = 1;
    RustO::initialize(config).map_err(|_| PdfError::OcrFailed)
}

pub(super) fn recognize(
    engine: &mut RustO,
    bgra: &[u8],
    width: i32,
    height: i32,
    limit: u64,
) -> Result<OcrPage, PdfError> {
    let rgb: Vec<_> = bgra
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0]])
        .collect();
    let detected = engine
        .detect_text_rgb(
            &rgb,
            width as u32,
            height as u32,
            width as usize * 3,
            &OcrRunOptions::default(),
        )
        .map_err(|_| PdfError::OcrFailed)?;
    let DetectTextResult::Structured(lines) = detected else {
        return Err(PdfError::OcrFailed);
    };
    let mut page = OcrPage {
        lines: Vec::new(),
        truncated: lines.len() >= 128,
    };
    let mut remaining = limit as usize;
    for line in lines {
        let text = line.text.trim();
        let size = text.chars().count();
        if size > remaining {
            page.truncated = true;
        } else if size > 0 {
            page.lines.push(text.to_owned());
            remaining -= size;
        }
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_files_require_both_size_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("model.mnn");
        let expected = crate::sha256(b"known");
        assert!(matches!(
            verified_model(&model, 5, &expected),
            Err(PdfError::OcrModelMissing)
        ));
        std::fs::write(&model, b"shorter").unwrap();
        assert!(matches!(
            verified_model(&model, 5, &expected),
            Err(PdfError::OcrModelMismatch)
        ));
        std::fs::write(&model, b"other").unwrap();
        assert!(matches!(
            verified_model(&model, 5, &expected),
            Err(PdfError::OcrModelMismatch)
        ));
        std::fs::write(&model, b"known").unwrap();
        assert!(verified_model(&model, 5, &expected).is_ok());
    }
}
