use super::{
    cache::{CacheHits, CacheQuery, CacheValue, PdfNegative, PdfObservation, PdfWindow, ScanCache},
    evidence::char_budget,
    parse::{self, Candidate, Completed, Outcome, PdfScan, PDF},
    pdf::{PdfActivity, PdfClient},
    sha256_hex, snapshot_limits, snapshot_reference, SnapshotManifest, SnapshotRow,
    SNAPSHOT_POLICY,
};
use crate::ReportError;
use daily_parsers::{
    contract::{ClassifyResult, ClassifyStatus, ParserBackend, OCR_PAGES_PER_FILE},
    pdf_wire::PdfNegativeKind,
    settings::{NormalizedScannerSettings, ParseStatus, RefillRoundAudit},
};
use std::{
    collections::VecDeque,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::{mpsc, Arc, Mutex},
    time::Instant,
};

pub(super) const REFILL_ROUNDS: u64 = 2;
const PDF_PAGE_QUOTA: &str = "pdf_classification_page_quota_exhausted";

#[cfg(test)]
thread_local! {
    pub(super) static SCAN_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

const MIB: u64 = 1024 * 1024;
const WORKING_SET_LIMIT: u64 = 52 * MIB;
const PDF_WORKING_SET: u64 = 20 * MIB + MIB / 2;

pub(super) struct Scheduler<'a> {
    root: &'a Path,
    profile: &'a NormalizedScannerSettings,
    deadline: Instant,
    pdf_clients: [Mutex<PdfClient>; 2],
    pub pdf_activity: Arc<PdfActivity>,
    pool: Option<rayon::ThreadPool>,
    page_slots_left: u64,
    extraction_slots_left: u64,
    pub hits: CacheHits,
    pub rounds: Vec<RefillRoundAudit>,
    pub pages_refunded: u64,
    pub lookup_count: u64,
    pub pdf_lookup_count: u64,
}

impl<'a> Scheduler<'a> {
    pub fn new(
        root: &'a Path,
        profile: &'a NormalizedScannerSettings,
        deadline: Instant,
        pdf_path: &Path,
    ) -> Self {
        let pdf_activity = Arc::new(PdfActivity::default());
        Self {
            root,
            profile,
            deadline,
            pdf_clients: std::array::from_fn(|_| {
                Mutex::new(PdfClient::new(
                    pdf_path.to_owned(),
                    profile,
                    Arc::clone(&pdf_activity),
                ))
            }),
            pdf_activity,
            pool: None,
            page_slots_left: profile.max_total_pdf_classification_pages,
            extraction_slots_left: profile.max_pdf_text_extractions,
            hits: CacheHits::default(),
            rounds: Vec::new(),
            pages_refunded: 0,
            lookup_count: 0,
            pdf_lookup_count: 0,
        }
    }

    pub fn restore_snapshot(
        &mut self,
        files: &mut [Candidate],
        cache: &mut ScanCache,
        key: &str,
        pdf_identity: Option<&str>,
    ) -> Result<Option<u64>, ReportError> {
        let Some(snapshot) = cache.read_snapshot(key)? else {
            return Ok(None);
        };
        let Ok((policy, candidates, extractions, pages, chars, rounds, rows)) =
            serde_json::from_slice::<SnapshotManifest<'_>>(&snapshot.manifest[8..])
        else {
            return Ok(None);
        };
        if policy != SNAPSHOT_POLICY
            || [candidates, extractions, pages, chars] != snapshot_limits(self.profile)
            || rounds.len() as u64 > REFILL_ROUNDS
        {
            return Ok(None);
        }

        let mut rows = rows.iter().peekable();
        let mut plan = Vec::with_capacity(files.len());
        for file in files.iter() {
            let Some(row) = rows.next().filter(|row| row.0 == file.relative_path) else {
                return Ok(None);
            };
            let ocr = rows.next_if(|row| row.0 == file.relative_path);
            let query = if file.completed.is_none() && row.3.is_some() {
                let Some(query) = self.snapshot_query(file, row, pdf_identity) else {
                    return Ok(None);
                };
                Some(query)
            } else {
                None
            };
            plan.push((row, ocr, query));
        }
        if rows.next().is_some() {
            return Ok(None);
        }
        let queries = plan
            .iter()
            .filter_map(|(_, _, query)| query.as_ref().map(|(query, _)| query));
        let mut lookup_count = queries.clone().count() as u64;
        let pdf_lookup_count = queries
            .clone()
            .filter(|query| query.pdf_window.is_some())
            .count() as u64;
        let mut hits = cache.lookup(queries)?;
        let mut native = Vec::with_capacity(files.len());
        for (file, (row, _, query)) in files.iter().zip(&plan) {
            let restored = match query {
                Some((query, extract)) => {
                    let Some(complete) =
                        read_cached(file, query, *extract, &mut hits, self.root, self.deadline)
                    else {
                        return Ok(None);
                    };
                    Some(complete)
                }
                None if file.completed.is_some() => None,
                None if file.backend == Some(PDF) => {
                    Some(Completed::new(Outcome::Omitted(PDF_PAGE_QUOTA)))
                }
                None => return Ok(None),
            };
            let complete = restored
                .as_ref()
                .or(file.completed.as_ref())
                .expect("restored native result");
            if *row != &SnapshotRow::new(file, complete)
                || matches!(complete.outcome, Outcome::Failed { .. })
            {
                return Ok(None);
            }
            native.push(restored);
        }
        let Some(refunded) = self.verify_reservations(files, &native, &rounds, pdf_identity)?
        else {
            return Ok(None);
        };

        let mut ocr: Vec<_> = std::iter::repeat_with(|| None).take(files.len()).collect();
        if self.profile.parse.pdf.image_ocr.is_some() {
            let source = files.iter().zip(&native).map(|(file, restored)| {
                (
                    file,
                    restored
                        .as_ref()
                        .or(file.completed.as_ref())
                        .expect("restored native result"),
                )
            });
            let ocr_plan = self.plan_ocr(source, pdf_identity);
            let queries = ocr_plan
                .iter()
                .filter_map(|(_, action)| action.as_ref().ok());
            lookup_count += queries.clone().count() as u64;
            hits.extend(cache.lookup(queries)?);
            for (index, action) in &ocr_plan {
                let complete = match action {
                    Ok(query) => read_cached(
                        &files[*index],
                        query,
                        true,
                        &mut hits,
                        self.root,
                        self.deadline,
                    )
                    .filter(|complete| matches!(complete.outcome, Outcome::Parsed { .. })),
                    Err(reason) => Some(Completed::new(Outcome::Omitted(reason))),
                };
                if complete.is_none() {
                    return Ok(None);
                }
                ocr[*index] = complete;
            }
        }
        for ((file, (_, row, _)), complete) in files.iter().zip(&plan).zip(&ocr) {
            if *row
                != complete
                    .as_ref()
                    .map(|complete| SnapshotRow::new(file, complete))
                    .as_ref()
            {
                return Ok(None);
            }
        }
        let expected = files
            .iter()
            .zip(&native)
            .zip(&ocr)
            .flat_map(|((file, native), ocr)| {
                native
                    .as_ref()
                    .or(file.completed.as_ref())
                    .into_iter()
                    .chain(ocr)
            })
            .filter_map(snapshot_reference);
        if Instant::now() >= self.deadline
            || !cache.accept_snapshot(key, &snapshot, &hits, expected)?
        {
            return Ok(None);
        }
        for ((file, native), ocr) in files.iter_mut().zip(native).zip(ocr) {
            file.completed = native.or(file.completed.take());
            file.ocr = ocr;
        }
        self.hits = hits;
        self.lookup_count = lookup_count;
        self.pdf_lookup_count = pdf_lookup_count;
        self.pages_refunded = refunded;
        self.rounds = rounds.into_owned();
        Ok(Some(snapshot.origin))
    }

    // Verify the saved admissions with the same reservation and refund rules.
    // No worker lanes, ready queues, pool threads or new parser requests are needed.
    fn verify_reservations(
        &self,
        files: &[Candidate],
        native: &[Option<Completed>],
        rounds: &[RefillRoundAudit],
        pdf_identity: Option<&str>,
    ) -> Result<Option<u64>, ReportError> {
        let mut pending: Vec<_> = files
            .iter()
            .zip(native)
            .filter_map(|(file, complete)| Some((file, complete.as_ref()?)))
            .collect();
        let mut pages = self.profile.max_total_pdf_classification_pages;
        let mut extractions = self.profile.max_pdf_text_extractions;
        let mut refunded = 0;
        let mut expected_rounds = rounds.iter();
        for wave in 0..=REFILL_ROUNDS {
            let start_pages = pages;
            let start_extractions = extractions;
            let mut refund = 0;
            let mut refill_keys = Vec::new();
            let mut later = Vec::new();
            for (file, complete) in pending {
                let backend = file.backend.expect("admitted source has a parser");
                if backend == PDF {
                    let Some(held) =
                        reserve_pages(&mut pages, self.profile.parse.pdf.max_pages, wave)
                    else {
                        later.push((file, complete));
                        continue;
                    };
                    let extract =
                        extractions > 0 && matches!(complete.outcome, Outcome::Parsed { .. });
                    if complete.pdf.as_ref().is_none_or(|pdf| {
                        pdf.charged_pages != held || pdf.extraction_slot != extract
                    }) {
                        return Ok(None);
                    }
                    extractions -= u64::from(extract);
                    refund += refunded_pages(held, complete);
                    if wave > 0 {
                        refill_keys.push(
                            parse::query(file, backend, self.profile, held, pdf_identity).key,
                        );
                    }
                } else if complete.query.is_none() || complete.pdf.is_some() {
                    return Ok(None);
                }
            }
            if !refill_keys.is_empty()
                && expected_rounds.next()
                    != Some(&refill_round(
                        wave,
                        &refill_keys,
                        start_pages - pages,
                        start_extractions - extractions,
                        refund,
                    )?)
            {
                return Ok(None);
            }
            refunded += refund;
            pages += refund;
            pending = later;
            if pending.is_empty() {
                break;
            }
        }
        Ok((expected_rounds.next().is_none()
            && pending
                .iter()
                .all(|(_, complete)| matches!(complete.outcome, Outcome::Omitted(PDF_PAGE_QUOTA))))
        .then_some(refunded))
    }

    fn snapshot_query(
        &self,
        file: &Candidate,
        row: &SnapshotRow<'_>,
        pdf_identity: Option<&str>,
    ) -> Option<(CacheQuery, bool)> {
        let backend = file.backend?;
        let key = row.3.as_deref()?;
        let (pages, extract) = match row.4 {
            Some((pages, extract))
                if backend == PDF && (1..=self.profile.parse.pdf.max_pages).contains(&pages) =>
            {
                (pages, extract)
            }
            None if backend != PDF => (0, false),
            _ => return None,
        };
        let mut query = parse::query(file, backend, self.profile, pages, pdf_identity);
        if &query.key != key && row.2.as_deref() == Some("pdf_text_extraction_quota_exhausted") {
            query = parse::inspection_query(&query);
        }
        (&query.key == key).then_some((query, extract))
    }

    pub fn scan(
        &mut self,
        files: &mut [Candidate],
        cache: &mut ScanCache,
        pdf_identity: Option<&str>,
    ) -> Result<(), ReportError> {
        #[cfg(test)]
        SCAN_CALLS.with(|calls| calls.set(calls.get() + 1));
        let full_window = self.profile.parse.pdf.max_pages;
        for wave in 0..=REFILL_ROUNDS {
            let mut work = Vec::new();
            let mut reserved = 0;
            let mut later_pdf = false;
            if Instant::now() >= self.deadline {
                break;
            }
            for (index, candidate) in files.iter().enumerate() {
                if candidate.completed.is_some() {
                    continue;
                }
                let backend = candidate.backend.expect("unfinished source has a parser");
                let pages = if backend == PDF {
                    let Some(pages) = reserve_pages(&mut self.page_slots_left, full_window, wave)
                    else {
                        later_pdf = true;
                        continue;
                    };
                    reserved += pages;
                    pages
                } else {
                    0
                };
                work.push(Work {
                    index,
                    body: parse::query(candidate, backend, self.profile, pages, pdf_identity),
                    inspection: None,
                });
            }
            if work.is_empty() {
                continue;
            }
            let pdf_in_wave = work
                .iter()
                .filter(|item| item.body.pdf_window.is_some())
                .count() as u64;
            if pdf_in_wave > self.extraction_slots_left {
                for item in work
                    .iter_mut()
                    .filter(|item| item.body.pdf_window.is_some())
                {
                    item.inspection = Some(parse::inspection_query(&item.body));
                }
            }
            let queries = work
                .iter()
                .flat_map(|item| std::iter::once(&item.body).chain(&item.inspection));
            self.lookup_count += work.len() as u64;
            self.pdf_lookup_count += pdf_in_wave;
            self.hits.extend(cache.lookup(queries)?);

            let before_refunds = self.pages_refunded;
            let before_extraction = self.extraction_slots_left;
            let close_pdf =
                (!later_pdf || wave == REFILL_ROUNDS) && self.profile.parse.pdf.image_ocr.is_none();
            self.run_wave(&work, files, close_pdf)?;
            if wave > 0 {
                let keys: Vec<_> = work.iter().map(|item| &item.body.key).collect();
                self.rounds.push(refill_round(
                    wave,
                    &keys,
                    reserved,
                    before_extraction - self.extraction_slots_left,
                    self.pages_refunded - before_refunds,
                )?);
            }
        }
        for candidate in files
            .iter_mut()
            .filter(|candidate| candidate.completed.is_none())
        {
            let outcome = if Instant::now() >= self.deadline {
                deadline_outcome(candidate)
            } else {
                Outcome::Omitted(PDF_PAGE_QUOTA)
            };
            candidate.completed = Some(Completed::new(outcome));
        }
        if self.profile.parse.pdf.image_ocr.is_some() {
            self.run_ocr(files, cache, pdf_identity)?;
        }
        for client in &mut self.pdf_clients {
            client
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .close();
        }
        Ok(())
    }

    fn plan_ocr<'f>(
        &self,
        files: impl Iterator<Item = (&'f Candidate, &'f Completed)> + Clone,
        pdf_identity: Option<&str>,
    ) -> Vec<(usize, Result<CacheQuery, &'static str>)> {
        let settings = self
            .profile
            .parse
            .pdf
            .image_ocr
            .expect("authorized OCR profile");
        let mut files_left = settings.max_files;
        let mut pages_left = settings.max_pages;
        let mut chars_left = char_budget(self.profile).saturating_sub(
            files
                .clone()
                .filter_map(|(_, native)| native.obtained())
                .map(|(result, _)| result.content.chars().count() as u64)
                .sum(),
        );
        let chars = parse::ocr_char_limit(self.profile);
        let mut plan = Vec::new();
        for (index, (file, native)) in files.enumerate() {
            if !matches!(native.outcome, Outcome::Negative(PdfNegativeKind::NoText)) {
                continue;
            }
            let pdf = &native.pdf.as_ref().expect("observed PDF").classification;
            let total_pages = pdf.page_count.expect("observed page count");
            let observations = pdf.page_audit.as_ref().expect("observed PDF pages");
            if !observations.all_pages_are_images(total_pages) {
                continue;
            }
            let pages = pages_left.min(total_pages).min(OCR_PAGES_PER_FILE);
            let reason = if files_left == 0 {
                "pdf_image_ocr_file_quota_exhausted"
            } else if pages == 0 {
                "pdf_image_ocr_page_quota_exhausted"
            } else if chars > chars_left {
                "global_context_budget_exceeded"
            } else {
                let selected = match pages {
                    1 => vec![1],
                    2 => vec![1, total_pages],
                    _ => vec![1, total_pages.div_ceil(2), total_pages],
                };
                plan.push((
                    index,
                    Ok(parse::query_for_window(
                        file,
                        PDF,
                        self.profile,
                        Some(PdfWindow {
                            max_pages: total_pages,
                            policy_version: observations.policy_version.clone(),
                            extract_max_pages: pages,
                            selected_pages: Some(selected),
                        }),
                        pdf_identity,
                    )),
                ));
                files_left -= 1;
                pages_left -= pages;
                chars_left -= chars;
                continue;
            };
            plan.push((index, Err(reason)));
        }
        plan
    }

    fn run_ocr(
        &mut self,
        files: &mut [Candidate],
        cache: &mut ScanCache,
        pdf_identity: Option<&str>,
    ) -> Result<(), ReportError> {
        let plan = self.plan_ocr(files.iter().map(|file| (file, file.result())), pdf_identity);
        let queries = plan.iter().filter_map(|(_, action)| action.as_ref().ok());
        let count = queries.clone().count();
        if count > 0 {
            self.hits.extend(cache.lookup(queries)?);
            self.lookup_count += count as u64;
        }
        for (index, action) in &plan {
            let file = &mut files[*index];
            let query = match action {
                Ok(query) => query,
                Err(reason) => {
                    file.ocr = Some(Completed::new(Outcome::Omitted(reason)));
                    continue;
                }
            };
            let job = Job {
                index: *index,
                query,
                extract: true,
                lane: Some(0),
                bytes: 0,
            };
            let mut result =
                read_cached(file, query, true, &mut self.hits, self.root, self.deadline)
                    .unwrap_or_else(|| self.execute(file, job));
            if let Outcome::Failed { reason, status, .. } = &mut result.outcome {
                *reason = if *status == ParseStatus::Timeout {
                    "pdf_image_ocr_timeout"
                } else {
                    "pdf_image_ocr_failed"
                };
            }
            file.ocr = Some(result);
        }
        Ok(())
    }

    fn run_wave(
        &mut self,
        work: &[Work],
        files: &mut [Candidate],
        close_pdf: bool,
    ) -> Result<(), ReportError> {
        let workers = self.profile.execution.max_workers as usize;
        if workers > 1
            && self.pool.is_none()
            && work
                .iter()
                .filter(|item| self.hits.get(&item.body.key).is_none())
                .take(2)
                .count()
                == 2
        {
            self.pool = Some(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(workers.min(work.len()))
                    .build()
                    .map_err(|error| ReportError::new("SCAN_POOL_FAILED", error.to_string()))?,
            );
        }
        let mut hits = std::mem::take(&mut self.hits);
        let (results, budget) = if let Some(pool) = &self.pool {
            pool.in_place_scope_fifo(|scope| {
                self.run_jobs(Some(scope), work, files, &mut hits, close_pdf)
            })
        } else {
            self.run_jobs(None, work, files, &mut hits, close_pdf)
        };
        self.hits = hits;
        self.extraction_slots_left = budget.extractions;
        self.pages_refunded += budget.refunded;
        self.page_slots_left += budget.refunded;
        for (index, result) in results {
            files[index].completed = Some(result);
        }
        Ok(())
    }

    fn run_jobs<'scope>(
        &'scope self,
        scope: Option<&rayon::ScopeFifo<'scope>>,
        work: &'scope [Work],
        files: &'scope [Candidate],
        hits: &mut CacheHits,
        close_pdf: bool,
    ) -> (Vec<(usize, Completed)>, WaveBudget) {
        let mut budget = WaveBudget {
            extractions: self.extraction_slots_left,
            ..WaveBudget::default()
        };
        let (mut pdf, mut local): (VecDeque<_>, VecDeque<_>) =
            work.iter().partition(|item| item.body.pdf_window.is_some());
        let mut results = Vec::with_capacity(work.len());
        let (done, completed) = mpsc::channel();
        let limit = self.profile.execution.max_workers as usize;

        loop {
            if close_pdf && pdf.is_empty() {
                for (lane, busy) in budget.lanes.iter().enumerate() {
                    if !busy {
                        self.pdf_clients[lane]
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .close();
                    }
                }
            }
            let free_lane = budget.lanes.iter().position(|busy| !busy);
            let idle_workers: [bool; 2] = std::array::from_fn(|lane| {
                !budget.lanes[lane]
                    && self.pdf_clients[lane]
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .is_running()
            });
            let resident =
                idle_workers.iter().filter(|&&running| running).count() as u64 * PDF_WORKING_SET;
            let plan = |item: &'scope Work, extract, lane: Option<usize>| {
                let query = item.query(hits, extract);
                let cached = hits.get(&query.key).is_some();
                let bytes = if cached {
                    0
                } else {
                    self.estimated_bytes(&files[item.index])
                };
                if (!cached && query.pdf_window.is_some() && lane.is_none())
                    || budget.active == limit
                    || (!cached
                        && (budget.active != 0 || resident != 0)
                        && parent_working_set() + budget.bytes + resident + bytes
                            - if lane.is_some_and(|lane| idle_workers[lane]) {
                                PDF_WORKING_SET
                            } else {
                                0
                            }
                            > WORKING_SET_LIMIT)
                {
                    return None;
                }
                Some((
                    item,
                    Job {
                        index: item.index,
                        query,
                        extract,
                        lane,
                        bytes,
                    },
                    cached,
                ))
            };
            let next_pdf = pdf
                .front()
                .filter(|_| budget.extractions == 0 || budget.held < budget.extractions);
            let next = next_pdf
                .and_then(|&item| plan(item, budget.extractions > 0, free_lane))
                .or_else(|| local.front().and_then(|&item| plan(item, false, None)));
            if let Some((item, job, cached)) = next {
                if job.query.pdf_window.is_some() {
                    pdf.pop_front();
                } else {
                    local.pop_front();
                }
                let file = &files[job.index];
                if let Some(scope) = scope.filter(|_| !cached && Instant::now() < self.deadline) {
                    budget.active += 1;
                    budget.held += u64::from(job.extract);
                    budget.bytes += job.bytes;
                    if let Some(lane) = job.lane {
                        budget.lanes[lane] = true;
                    }
                    let done = done.clone();
                    scope.spawn_fifo(move |_| {
                        let result = catch_unwind(AssertUnwindSafe(|| self.execute(file, job)))
                            .unwrap_or_else(|_| {
                                Completed::new(parse::failed(
                                    file,
                                    "PARSER_FAILED",
                                    "parser task panicked".into(),
                                    true,
                                ))
                            });
                        let _ = done.send((item, job, result));
                    });
                    continue;
                }
                let result =
                    read_cached(file, job.query, job.extract, hits, self.root, self.deadline)
                        .unwrap_or_else(|| self.execute(file, job));
                results.push((job.index, budget.finish(item, job, result)));
            } else if budget.active != 0 {
                let (item, job, result) = completed.recv().expect("scoped parser task completed");
                budget.active -= 1;
                budget.held -= u64::from(job.extract);
                budget.bytes -= job.bytes;
                if let Some(lane) = job.lane {
                    budget.lanes[lane] = false;
                }
                results.push((job.index, budget.finish(item, job, result)));
            } else if resident != 0 && (!pdf.is_empty() || !local.is_empty()) {
                // Retire idle processes so one large operation can use its full allowance.
                for client in &self.pdf_clients {
                    client
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .close();
                }
            } else {
                break;
            }
        }
        (results, budget)
    }

    fn execute(&self, file: &Candidate, job: Job<'_>) -> Completed {
        if Instant::now() >= self.deadline {
            return Completed::new(deadline_outcome(file));
        }
        let mut client = job.lane.map(|lane| {
            self.pdf_clients[lane]
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        parse::run(
            file,
            job.query,
            self.root,
            self.profile,
            self.deadline,
            client.as_deref_mut().map(|client| (client, job.extract)),
        )
    }

    fn estimated_bytes(&self, file: &Candidate) -> u64 {
        let size = file.file.size_bytes;
        match file.backend.expect("admitted source has a parser") {
            ParserBackend::LightTextV2 => {
                let text = &self.profile.parse.text;
                let window = if file.file.extension == ".log" {
                    text.read_tail_bytes
                } else {
                    text.read_head_bytes
                };
                size.min(window)
                    .min(daily_parsers::text::MAX_TEXT_WINDOW_BYTES)
                    .saturating_mul(4)
                    + 512 * 1024
            }
            // A resident PDF worker includes PDFium and its supporting processes.
            PDF => size.min(16 * MIB) + PDF_WORKING_SET,
            _ => size.saturating_mul(6).clamp(4 * MIB, 32 * MIB),
        }
    }
}

fn parent_working_set() -> u64 {
    #[cfg(windows)]
    {
        use std::mem::size_of_val;
        use windows_sys::Win32::System::{
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::GetCurrentProcess,
        };
        let mut memory = PROCESS_MEMORY_COUNTERS::default();
        let ok = unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut memory,
                size_of_val(&memory) as u32,
            )
        };
        if ok != 0 {
            return memory.WorkingSetSize as u64;
        }
    }
    12 * MIB
}

fn cached_pdf(
    pages: u64,
    status: ClassifyStatus,
    observation: Option<PdfObservation>,
    diagnostic: Option<daily_parsers::contract::WorkerDiagnostic>,
    extraction_slot: bool,
) -> PdfScan {
    let (page_count, page_audit) =
        observation.map_or((None, None), |obs| (Some(obs.page_count), Some(obs.pages)));
    PdfScan {
        classification: ClassifyResult {
            status,
            page_count,
            result_examined_pages: page_audit
                .as_ref()
                .map(|audit| audit.checked_pages.len() as u64),
            page_audit,
            diagnostic,
        },
        charged_pages: pages,
        extraction_slot,
        requests: 0,
        document_opens: 0,
        page_reads: 0,
    }
}

fn read_cached(
    file: &Candidate,
    query: &CacheQuery,
    extract: bool,
    hits: &mut CacheHits,
    root: &Path,
    deadline: Instant,
) -> Option<Completed> {
    let value = hits.take(&query.key)?;
    if let Err(error) = parse::verify_source(file, root, deadline) {
        return Some(Completed::new(parse::source_error(file, &error)));
    }
    let window = query.pdf_window.as_ref();
    let (outcome, pdf) = match value {
        CacheValue::Parsed(mut cached) => {
            let pdf = window.map(|window| {
                let ocr = window.selected_pages.is_some();
                cached_pdf(
                    window.max_pages,
                    if ocr {
                        ClassifyStatus::NoTextInParseWindow
                    } else {
                        ClassifyStatus::TextInParseWindow
                    },
                    cached.observation.take(),
                    None,
                    extract && !ocr,
                )
            });
            cached.result.file_path.clone_from(&file.file.path);
            (
                Outcome::Parsed {
                    result: cached.result,
                    source_sha256: cached.source_sha256,
                    inspection_only: window.is_some() && !extract,
                },
                pdf,
            )
        }
        CacheValue::Negative(negative) => {
            let pages = window.expect("negative has a PDF window").max_pages;
            let (kind, pdf) = match negative {
                PdfNegative::NoText(observation) => (
                    PdfNegativeKind::NoText,
                    cached_pdf(
                        pages,
                        ClassifyStatus::NoTextInParseWindow,
                        Some(observation),
                        None,
                        false,
                    ),
                ),
                PdfNegative::Damaged(mut diagnostic)
                | PdfNegative::PasswordProtected(mut diagnostic) => {
                    let kind = if diagnostic.error_code == "PDF_PROTECTED" {
                        PdfNegativeKind::Protected
                    } else {
                        PdfNegativeKind::InvalidPdf
                    };
                    diagnostic.file_path = Some(file.file.path.clone());
                    (
                        kind,
                        cached_pdf(pages, ClassifyStatus::Error, None, Some(diagnostic), false),
                    )
                }
            };
            (Outcome::Negative(kind), Some(pdf))
        }
    };
    Some(Completed {
        query: Some(query.clone()),
        reused: true,
        pdf,
        ..Completed::new(outcome)
    })
}

#[derive(Clone, Copy)]
struct Job<'a> {
    index: usize,
    query: &'a CacheQuery,
    extract: bool,
    lane: Option<usize>,
    bytes: u64,
}

struct Work {
    index: usize,
    body: CacheQuery,
    inspection: Option<CacheQuery>,
}

impl Work {
    fn query(&self, hits: &CacheHits, extract: bool) -> &CacheQuery {
        self.inspection
            .as_ref()
            .filter(|inspection| {
                !extract
                    && !matches!(hits.get(&self.body.key), Some(CacheValue::Negative(_)))
                    && (matches!(hits.get(&inspection.key), Some(CacheValue::Parsed(_)))
                        || hits.get(&self.body.key).is_none())
            })
            .unwrap_or(&self.body)
    }
}

#[derive(Default)]
struct WaveBudget {
    extractions: u64,
    held: u64,
    refunded: u64,
    active: usize,
    lanes: [bool; 2],
    bytes: u64,
}

impl WaveBudget {
    fn finish(&mut self, item: &Work, job: Job<'_>, mut result: Completed) -> Completed {
        if let Some(window) = &job.query.pdf_window {
            self.refunded += refunded_pages(window.max_pages, &result);
            if result.pdf.as_ref().is_some_and(|pdf| pdf.extraction_slot) {
                self.extractions -= 1;
            }
            if matches!(result.outcome, Outcome::Negative(_)) {
                result.query = Some(item.body.clone());
            }
        }
        result
    }
}

fn reserve_pages(left: &mut u64, full_window: u64, wave: u64) -> Option<u64> {
    if *left == 0 || (wave == 0 && *left < full_window) {
        return None;
    }
    let pages = (*left).min(full_window);
    *left -= pages;
    Some(pages)
}

fn refill_round(
    round: u64,
    keys: &[impl serde::Serialize],
    pages: u64,
    extractions: u64,
    refund: u64,
) -> Result<RefillRoundAudit, ReportError> {
    Ok(RefillRoundAudit {
        round,
        planned_source_count: keys.len() as u64,
        plan_sha256: sha256_hex(&serde_json::to_vec(keys)?),
        candidate_slots_used: keys.len() as u64,
        classification_pages_held: pages,
        extraction_slots_used: extractions,
        reserved_chars: 0,
        refunded_chars: 0,
        refunded_classification_pages: refund,
    })
}

fn refunded_pages(pages: u64, result: &Completed) -> u64 {
    let used = match &result.outcome {
        Outcome::Negative(PdfNegativeKind::InvalidPdf | PdfNegativeKind::Protected) => 0,
        _ => result
            .pdf
            .as_ref()
            .filter(|pdf| {
                matches!(
                    pdf.classification.status,
                    ClassifyStatus::TextInParseWindow | ClassifyStatus::NoTextInParseWindow
                )
            })
            .and_then(|pdf| pdf.classification.result_examined_pages)
            .unwrap_or(pages),
    };
    pages.saturating_sub(used)
}

fn deadline_outcome(file: &Candidate) -> Outcome {
    parse::failed(
        file,
        "STAGE_DEADLINE_EXHAUSTED",
        "scan dispatch deadline reached".into(),
        true,
    )
}
