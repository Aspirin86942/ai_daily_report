//! Explicit, hash-bound OCR import. Parsing failures never dispatch an OCR engine.
use super::files::{read_bytes, relative_path, write_immutable};
use crate::{
    config::AppConfig,
    snapshots::{canonical_json, digest, reject_redirects, valid_hash},
    types::new_run_id,
    ReportError,
};
use ai_daily_scanner_contract::{AuditWorkerLane, ParseStatus};
use ai_daily_scanner_core::{
    process::{run_process, ProcessError, ProcessSpec},
    sources::ScanSource,
};
use ai_daily_worker_contract::{
    AuditTextBuilder, ClassifyRequest, ClassifyResult, ClassifyStatus, ContentUnitKind,
    SourceRange, SourceRangeUnit, WorkerHello, WorkerKind, WorkerOperation, WorkerRequest,
    WorkerResponse, WorkerResponseStatus, CONTRACT, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant, UNIX_EPOCH},
};

fn error(code: &'static str) -> ReportError {
    ReportError::new(code, "OCR 材料、原文件身份或执行限制不符合要求")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OcrLimits {
    pub enabled: bool,
    pub max_files: usize,
    pub max_pages_per_file: usize,
    pub max_total_pages: usize,
    pub max_output_chars: usize,
    pub max_runtime_seconds: u64,
    pub max_file_size_bytes: u64,
}
impl Default for OcrLimits {
    fn default() -> Self {
        Self {
            enabled: false,
            max_files: 4,
            max_pages_per_file: 5,
            max_total_pages: 10,
            max_output_chars: 6000,
            max_runtime_seconds: 60,
            max_file_size_bytes: 50 * 1024 * 1024,
        }
    }
}
impl OcrLimits {
    pub fn validate(&self) -> Result<(), ReportError> {
        if !(1..=1000).contains(&self.max_files)
            || !(1..=10000).contains(&self.max_pages_per_file)
            || !(1..=10000).contains(&self.max_total_pages)
            || !(1..=100000).contains(&self.max_output_chars)
            || !(1..=3600).contains(&self.max_runtime_seconds)
            || !(1..=4 * 1024 * 1024 * 1024).contains(&self.max_file_size_bytes)
        {
            return Err(error("OCR_LIMITS_INVALID"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrPage {
    pub page: u64,
    pub text: String,
    pub text_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrResult {
    pub content_sha256: String,
    pub pages: Vec<OcrPage>,
    pub text_unverified: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestDocument {
    source_id: String,
    content_sha256: String,
    pages_total: u64,
    pages: Vec<OcrPage>,
}
#[derive(Debug, Clone)]
pub struct OcrDocument {
    pub content_sha256: String,
    pub pages_total: u64,
    pub pages: Vec<OcrPage>,
    pub relative_path: String,
}
pub type OcrManifest = BTreeMap<String, OcrDocument>;

fn check_pages(pages: &[OcrPage], total: u64, limits: &OcrLimits) -> Result<(), ReportError> {
    let mut seen = BTreeSet::new();
    if pages.is_empty() || pages.len() > limits.max_pages_per_file || total == 0 {
        return Err(error("OCR_PAGE_OUT_OF_RANGE"));
    }
    let mut chars = 0usize;
    for page in pages {
        if page.page == 0 || page.page > total || !seen.insert(page.page) {
            return Err(error("OCR_PAGE_OUT_OF_RANGE"));
        }
        chars = chars
            .checked_add(page.text.chars().count())
            .ok_or_else(|| error("OCR_OUTPUT_LIMIT"))?;
        if !valid_hash(&page.text_sha256) {
            return Err(error("OCR_TEXT_CHANGED"));
        }
    }
    if chars > limits.max_output_chars {
        return Err(error("OCR_OUTPUT_LIMIT"));
    }
    if pages.iter().all(|p| p.text.trim().is_empty()) {
        return Err(error("OCR_EMPTY"));
    }
    Ok(())
}

/// Classifies through the same contained PDFium worker used by the scanner.
pub fn pdf_identity(
    config: &AppConfig,
    path: &Path,
    limits: &OcrLimits,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
) -> Result<(String, u64), ReportError> {
    limits.validate()?;
    let path = fs::canonicalize(path)?;
    reject_redirects(&path, "OCR_SOURCE_CHANGED")?;
    let before = fs::metadata(&path)?;
    let bytes = read_bytes(&path, limits.max_file_size_bytes)?;
    if !bytes.starts_with(b"%PDF-") {
        return Err(error("OCR_INVALID_SOURCE"));
    }
    let hash = digest(&bytes);
    let ns = before
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| error("OCR_SOURCE_CHANGED"))?
        .as_nanos();
    let id = new_run_id();
    let id = format!(
        "{}-{}-{}-{}-{}",
        &id[..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..]
    );
    let payload = ClassifyRequest {
        file_path: path.to_string_lossy().into_owned(),
        source_version: ai_daily_discovery::build_source_version(ns, before.len()),
        max_pages: 1,
        policy_version: "pdf_text_presence_v2".into(),
    };
    let request = WorkerRequest {
        contract: CONTRACT.into(),
        protocol_version: PROTOCOL_VERSION,
        request_id: id.clone(),
        operation: WorkerOperation::PdfClassify,
        payload: serde_json::to_value(payload)?,
    };
    let mut spec = ProcessSpec::new(
        config.pdf_worker_path.clone(),
        deadline.saturating_duration_since(Instant::now()),
    );
    spec.args.push("session".into());
    spec.stdin = serde_json::to_vec(&request)?;
    spec.stdin.push(b'\n');
    spec.capture_limit = 512 * 1024;
    spec.memory_limit_bytes = Some(512 * 1024 * 1024);
    spec.cancel = Some(cancel);
    let result = run_process(&spec).map_err(process_error)?;
    if result.exit_code != 0 {
        return Err(error("OCR_PDF_IDENTITY_FAILED"));
    }
    let lines: Vec<_> = result
        .stdout
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty() && *l != b"\r")
        .collect();
    if lines.len() != 2 {
        return Err(error("OCR_PDF_IDENTITY_FAILED"));
    }
    let hello: WorkerHello =
        serde_json::from_slice(lines[0]).map_err(|_| error("OCR_PDF_IDENTITY_FAILED"))?;
    hello
        .validate()
        .map_err(|_| error("OCR_PDF_IDENTITY_FAILED"))?;
    if hello.worker_kind != WorkerKind::Pdf
        || hello.worker_build != ai_daily_scanner_core::ENGINE_BUILD_IDENTITY
        || !hello
            .supported_operations
            .contains(&WorkerOperation::PdfClassify)
    {
        return Err(error("OCR_WORKER_MISMATCH"));
    }
    let response: WorkerResponse =
        serde_json::from_slice(lines[1]).map_err(|_| error("OCR_PDF_IDENTITY_FAILED"))?;
    response
        .validate()
        .map_err(|_| error("OCR_PDF_IDENTITY_FAILED"))?;
    if response.request_id != id
        || response.operation != WorkerOperation::PdfClassify
        || response.status != WorkerResponseStatus::Ok
    {
        return Err(error("OCR_PDF_IDENTITY_FAILED"));
    }
    let classification: ClassifyResult = serde_json::from_value(response.result.unwrap())
        .map_err(|_| error("OCR_PDF_IDENTITY_FAILED"))?;
    classification
        .validate_for_max_pages(1)
        .map_err(|_| error("OCR_PDF_IDENTITY_FAILED"))?;
    if classification.status == ClassifyStatus::Error {
        return Err(error("OCR_PDF_IDENTITY_FAILED"));
    }
    let count = classification
        .page_count
        .filter(|n| *n > 0)
        .ok_or_else(|| error("OCR_PDF_IDENTITY_FAILED"))?;
    let after = fs::metadata(&path)?;
    if before.len() != after.len()
        || before.modified()? != after.modified()?
        || digest(&read_bytes(&path, limits.max_file_size_bytes)?) != hash
    {
        return Err(error("OCR_SOURCE_CHANGED"));
    }
    Ok((hash, count))
}
fn process_error(e: ProcessError) -> ReportError {
    error(match e {
        ProcessError::Cancelled => "OCR_CANCELLED",
        ProcessError::TimedOut => "OCR_TIMEOUT",
        ProcessError::OutputTooLarge => "OCR_OUTPUT_LIMIT",
        _ => "OCR_PROCESS_FAILED",
    })
}

pub fn load_manifest(
    config: &AppConfig,
    path: &Path,
    limits: &OcrLimits,
) -> Result<OcrManifest, ReportError> {
    limits.validate()?;
    let bytes = read_bytes(path, 4 * 1024 * 1024)?;
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
    let raw: Vec<ManifestDocument> =
        serde_json::from_slice(bytes).map_err(|_| error("OCR_INVALID_MANIFEST"))?;
    if raw.len() > 1000 {
        return Err(error("OCR_MANIFEST_LIMIT"));
    }
    let mut result = OcrManifest::new();
    for doc in raw {
        if !doc.source_id.strip_prefix("src_").is_some_and(valid_hash)
            || !valid_hash(&doc.content_sha256)
            || result.contains_key(&doc.source_id)
        {
            return Err(error("OCR_INVALID_MANIFEST"));
        }
        let mut pages = Vec::new();
        for page in doc.pages {
            let text_path = Path::new(&page.text);
            let text_path = if text_path.is_absolute() {
                text_path.to_owned()
            } else {
                path.parent().unwrap().join(text_path)
            };
            let bytes = read_bytes(&text_path, limits.max_output_chars as u64 * 4 + 3)?;
            if digest(&bytes) != page.text_sha256 {
                return Err(error("OCR_TEXT_CHANGED"));
            }
            let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
            let text = String::from_utf8(bytes.to_vec()).map_err(|_| error("OCR_INVALID_TEXT"))?;
            pages.push(OcrPage {
                page: page.page,
                text,
                text_sha256: page.text_sha256,
            });
        }
        check_pages(&pages, doc.pages_total, limits)?;
        pages.sort_by_key(|p| p.page);
        result.insert(
            doc.source_id,
            OcrDocument {
                content_sha256: doc.content_sha256,
                pages_total: doc.pages_total,
                pages,
                relative_path: String::new(),
            },
        );
    }
    if result.is_empty() {
        return Ok(result);
    }
    let root = fs::canonicalize(&config.paths.work_dir)?;
    reject_redirects(&config.paths.work_dir, "OCR_SOURCE_CHANGED")?;
    let deadline = Instant::now() + Duration::from_secs(limits.max_runtime_seconds);
    for (count, entry) in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .enumerate()
    {
        if count >= 100000 || Instant::now() >= deadline {
            return Err(error("OCR_SOURCE_LOOKUP_LIMIT"));
        }
        let entry = entry.map_err(|_| error("OCR_SOURCE_CHANGED"))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(&root)
            .map_err(|_| error("OCR_SOURCE_CHANGED"))?
            .to_string_lossy()
            .replace('\\', "/");
        let id = format!("src_{}", digest(relative.as_bytes()));
        if let Some(doc) = result.get_mut(&id) {
            let source = relative_path(&root, &relative)?;
            let (hash, total) = pdf_identity(
                config,
                &source,
                limits,
                Arc::new(AtomicBool::new(false)),
                deadline,
            )?;
            if hash != doc.content_sha256 || total != doc.pages_total {
                return Err(error("OCR_SOURCE_CHANGED"));
            }
            doc.relative_path = relative;
        }
    }
    if result.values().any(|doc| doc.relative_path.is_empty()) {
        return Err(error("OCR_SOURCE_NOT_FOUND"));
    }
    Ok(result)
}

pub fn overlay(
    source: &mut ScanSource,
    doc: &OcrDocument,
    config: &AppConfig,
    limits: &OcrLimits,
) -> Result<(), ReportError> {
    let path = relative_path(&config.paths.work_dir, &doc.relative_path)?;
    if source.relative_path != doc.relative_path
        || fs::metadata(&path)?.len() != source.size_bytes
        || source
            .content_sha256
            .as_ref()
            .is_some_and(|hash| hash != &doc.content_sha256)
        || digest(&read_bytes(&path, limits.max_file_size_bytes)?) != doc.content_sha256
    {
        return Err(error("OCR_SOURCE_CHANGED"));
    }
    check_pages(&doc.pages, doc.pages_total, limits)?;
    if !source.excerpt.trim().is_empty() {
        return Ok(());
    }
    let mut builder = AuditTextBuilder::new(limits.max_output_chars as u64, "pdf");
    for page in &doc.pages {
        let range = SourceRange::new(
            SourceRangeUnit::PdfPage,
            page.page,
            page.page + 1,
            Some("unverified_ocr".into()),
        );
        builder.read(range.clone());
        builder.push(
            &format!("## Page {}", page.page),
            ContentUnitKind::Heading,
            vec![range.clone()],
        );
        for line in page.text.lines() {
            builder.push(line, ContentUnitKind::Paragraph, vec![range.clone()]);
        }
    }
    let (text, audit, truncated) = builder.finish();
    let mut audit = serde_json::to_value(audit)?;
    audit["primary_status"] = json!("usable_excerpt");
    audit["primary_reason"] = json!(null);
    audit["text_unverified"] = json!(true);
    audit["parser_version"] = json!("local_ocr_import_v3");
    audit["secondary_tags"] = json!(["OCR_TEXT_UNVERIFIED"]);
    source.coverage_limits["evidence_audit"] = audit;
    source.coverage_limits["ocr"] = json!({"engine":"local_import","pages_read":doc.pages.len(),"pages_total":doc.pages_total,"pages":doc.pages.iter().map(|p|p.page).collect::<Vec<_>>(),"text_unverified":true,"text_sha256":doc.pages.iter().map(|p|&p.text_sha256).collect::<Vec<_>>()});
    source.excerpt_sha256 = digest(text.as_bytes());
    source.excerpt = text;
    source.content_sha256 = Some(doc.content_sha256.clone());
    source.extension = ".pdf".into();
    source.parser_backend = "local_ocr_import_v3".into();
    source.worker_lane = AuditWorkerLane::LocalOcrPreflight;
    source.parse_status = ParseStatus::Success;
    source.parse_truncated = (doc.pages.len() as u64) < doc.pages_total;
    source.excerpt_truncated = truncated;
    source.reason = Some("local_ocr_text_requires_verification".into());
    Ok(())
}

pub struct OcrEngine {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub limits: OcrLimits,
    pub cancel: Arc<AtomicBool>,
    deadline: Instant,
    used_files: usize,
    used_pages: usize,
}
impl OcrEngine {
    pub fn new(
        program: PathBuf,
        args: Vec<OsString>,
        limits: OcrLimits,
        cancel: Arc<AtomicBool>,
    ) -> Result<Self, ReportError> {
        limits.validate()?;
        if !limits.enabled {
            return Err(error("OCR_DISABLED"));
        }
        if !program.is_absolute() || !program.is_file() {
            return Err(error("OCR_ENGINE_REQUIRED"));
        }
        reject_redirects(&program, "OCR_ENGINE_INVALID")?;
        let deadline = Instant::now() + Duration::from_secs(limits.max_runtime_seconds);
        Ok(Self {
            program,
            args,
            limits,
            cancel,
            deadline,
            used_files: 0,
            used_pages: 0,
        })
    }
    pub fn recognize(
        &mut self,
        config: &AppConfig,
        file: &Path,
        pages: &[u64],
    ) -> Result<OcrDocument, ReportError> {
        if self.used_files >= self.limits.max_files
            || self.used_pages + pages.len() > self.limits.max_total_pages
        {
            return Err(error("OCR_BUDGET_EXHAUSTED"));
        }
        let path = relative_path(
            &config.paths.work_dir,
            file.to_str().ok_or_else(|| error("OCR_INVALID_SOURCE"))?,
        )?;
        let (hash, total) = pdf_identity(
            config,
            &path,
            &self.limits,
            self.cancel.clone(),
            self.deadline,
        )?;
        if pages.is_empty()
            || pages.len() > self.limits.max_pages_per_file
            || pages.iter().copied().collect::<BTreeSet<_>>().len() != pages.len()
            || pages.iter().any(|p| *p == 0 || *p > total)
        {
            return Err(error("OCR_PAGE_OUT_OF_RANGE"));
        }
        self.used_files += 1;
        self.used_pages += pages.len();
        let mut spec = ProcessSpec::new(
            self.program.clone(),
            self.deadline.saturating_duration_since(Instant::now()),
        );
        spec.args = self.args.clone();
        spec.memory_limit_bytes = Some(512 * 1024 * 1024);
        spec.cancel = Some(self.cancel.clone());
        spec.capture_limit = self.limits.max_output_chars * 6 + pages.len() * 256 + 4096;
        spec.stdin = canonical_json(
            &json!({"file_path":path,"pages":pages,"content_sha256":hash,"max_output_chars":self.limits.max_output_chars,"text_unverified":true}),
        )?;
        if spec.stdin.len() > 256 * 1024 {
            return Err(error("OCR_INPUT_LIMIT"));
        }
        let output = run_process(&spec).map_err(process_error)?;
        if output.exit_code != 0 {
            return Err(error("OCR_PROCESS_FAILED"));
        }
        let result: OcrResult =
            serde_json::from_slice(&output.stdout).map_err(|_| error("OCR_RESPONSE_INVALID"))?;
        check_pages(&result.pages, total, &self.limits)?;
        if !result.text_unverified
            || result.content_sha256 != hash
            || result.pages.iter().map(|p| p.page).collect::<BTreeSet<_>>()
                != pages.iter().copied().collect()
            || result
                .pages
                .iter()
                .any(|p| digest(p.text.as_bytes()) != p.text_sha256)
        {
            return Err(error("OCR_RESPONSE_INVALID"));
        }
        if digest(&read_bytes(&path, self.limits.max_file_size_bytes)?) != hash {
            return Err(error("OCR_SOURCE_CHANGED"));
        }
        let mut pages = result.pages;
        pages.sort_by_key(|p| p.page);
        Ok(OcrDocument {
            content_sha256: hash,
            pages_total: total,
            pages,
            relative_path: file.to_string_lossy().replace('\\', "/"),
        })
    }
}

pub fn export_document(dir: &Path, doc: &OcrDocument) -> Result<PathBuf, ReportError> {
    let mut pages = Vec::new();
    for page in &doc.pages {
        let name = format!("page-{}-{}.txt", page.page, page.text_sha256);
        write_immutable(&dir.join(&name), page.text.as_bytes())?;
        pages.push(json!({"page":page.page,"text":name,"text_sha256":page.text_sha256}));
    }
    let manifest = dir.join("ocr-manifest.json");
    write_immutable(
        &manifest,
        &canonical_json(
            &json!([{"source_id":format!("src_{}",digest(doc.relative_path.as_bytes())),"content_sha256":doc.content_sha256,"pages_total":doc.pages_total,"pages":pages}]),
        )?,
    )?;
    Ok(manifest)
}
