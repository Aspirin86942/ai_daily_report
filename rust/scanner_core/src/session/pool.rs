use super::*;
use ai_daily_scanner_contract::SessionRestartCounts;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerTransport {
    Session,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionOperationOutcome<T> {
    pub value: T,
    pub transport: WorkerTransport,
    pub attempt_count: u64,
    pub duration_ms: u64,
}

#[derive(Debug)]
pub struct SessionOperationFailure {
    pub failure: ParseFailure,
    pub transport: WorkerTransport,
    pub attempt_count: u64,
    pub duration_ms: u64,
}

#[derive(Clone, Copy)]
pub(super) enum SessionRestartReason {
    LaneSwitch,
    RequestLimit,
    IdleTtl,
    RssLimit,
    Timeout,
    Transport,
    Rejected,
}

impl SessionRestartReason {
    fn after_error(error: &SessionError) -> Self {
        match error {
            SessionError::Timeout => Self::Timeout,
            SessionError::Rejected(_) => Self::Rejected,
            _ => Self::Transport,
        }
    }
    fn record(self, counts: &mut SessionRestartCounts) {
        let count = match self {
            Self::LaneSwitch => &mut counts.lane_switch,
            Self::RequestLimit => &mut counts.request_limit,
            Self::IdleTtl => &mut counts.idle_ttl,
            Self::RssLimit => &mut counts.rss_limit,
            Self::Timeout => &mut counts.timeout,
            Self::Transport => &mut counts.transport,
            Self::Rejected => &mut counts.rejected,
        };
        *count = count.saturating_add(1);
    }
}

/// Lifecycle counters for the worker sessions.
/// Operation attempts stay on each outcome/failure so scheduler metrics and
/// per-file provenance share one exact source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPoolStats {
    pub session_restart_count: u64,
    pub session_restart_reasons: SessionRestartCounts,
    /// `Some(0)` means no session child was started; `None` means Windows Job
    /// accounting was attempted and failed for at least one child.
    pub peak_worker_rss_bytes: Option<u64>,
}

impl SessionPoolStats {
    pub fn combine(left: Self, right: Self) -> Self {
        Self {
            session_restart_count: left
                .session_restart_count
                .saturating_add(right.session_restart_count),
            session_restart_reasons: SessionRestartCounts::combine(
                left.session_restart_reasons,
                right.session_restart_reasons,
            ),
            peak_worker_rss_bytes: match (left.peak_worker_rss_bytes, right.peak_worker_rss_bytes) {
                (Some(left), Some(right)) => Some(left.max(right)),
                _ => None,
            },
        }
    }
}

#[derive(Default)]
struct SessionSlot {
    session: Option<WorkerSession>,
    replacement_pending: Option<SessionRestartReason>,
}

impl SessionSlot {
    fn retire(&mut self, reason: SessionRestartReason) {
        // The permit still owns the slot while Drop terminates and reaps the child.
        drop(self.session.take());
        self.replacement_pending = Some(reason);
    }
}

#[derive(Default)]
struct SlotState {
    idle: Vec<SessionSlot>,
    busy: [usize; 2],
    waiters: VecDeque<(u64, WorkerKind)>,
    next_ticket: u64,
}

impl SlotState {
    fn next_waiter(&self) -> usize {
        let (_, lane) = self.waiters[0];
        // With at most two slots, an idle slot plus a busy lane can run one
        // request of each kind. Give the other lane priority if it is waiting.
        if self.busy[lane as usize] > 0 {
            if let Some(index) = self.waiters.iter().position(|&(_, kind)| kind != lane) {
                return index;
            }
        }
        0
    }
}

/// A slot owns at most one live process, including between requests. Both
/// worker kinds draw from the same slots for a scan.
pub(crate) struct SessionSlots {
    state: Mutex<SlotState>,
    changed: Condvar,
}

impl SessionSlots {
    pub(crate) fn new(concurrency: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SlotState {
                idle: (0..concurrency.clamp(1, 2))
                    .map(|_| SessionSlot::default())
                    .collect(),
                ..SlotState::default()
            }),
            changed: Condvar::new(),
        })
    }

    fn checkout(
        &self,
        expected: &WorkerHello,
        started: Instant,
        timeout: Duration,
    ) -> Option<SlotPermit<'_>> {
        let mut state = lock_unpoison(&self.state);
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        let lane = expected.worker_kind;
        state.waiters.push_back((ticket, lane));
        loop {
            let Some(remaining) = timeout
                .checked_sub(started.elapsed())
                .filter(|t| !t.is_zero())
            else {
                state.waiters.retain(|&(other, _)| other != ticket);
                self.changed.notify_all();
                return None;
            };
            if !state.idle.is_empty() {
                let next = state.next_waiter();
                if state.waiters[next].0 == ticket {
                    // Prefer the most recently idle matching process, then an
                    // empty slot. Replace a different lane only when necessary.
                    let index = state
                        .idle
                        .iter()
                        .rposition(|s| s.session.as_ref().is_some_and(|s| s.identity() == expected))
                        .or_else(|| state.idle.iter().rposition(|s| s.session.is_none()))
                        .unwrap_or(0);
                    state.waiters.remove(next);
                    state.busy[lane as usize] += 1;
                    let slot = state.idle.swap_remove(index);
                    self.changed.notify_all();
                    return Some(SlotPermit {
                        slots: self,
                        lane,
                        slot,
                    });
                }
            }
            (state, _) = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

pub struct WorkerPool {
    command: WorkerCommand,
    expected: WorkerHello,
    worker: RegisteredWorker,
    params: SessionParams,
    slots: Arc<SessionSlots>,
    session_restart_reasons: Mutex<SessionRestartCounts>,
    pool_rss: WorkerRssTracker,
    rss_tracker: WorkerRssTracker,
}

impl WorkerPool {
    pub fn new(
        command: WorkerCommand,
        expected: WorkerHello,
        worker: RegisteredWorker,
        params: SessionParams,
        rss_tracker: WorkerRssTracker,
    ) -> Arc<Self> {
        Self::with_slots(
            command,
            expected,
            worker,
            params,
            rss_tracker,
            SessionSlots::new(params.concurrency),
        )
    }

    pub(crate) fn with_slots(
        command: WorkerCommand,
        expected: WorkerHello,
        worker: RegisteredWorker,
        params: SessionParams,
        rss_tracker: WorkerRssTracker,
        slots: Arc<SessionSlots>,
    ) -> Arc<Self> {
        Arc::new(Self {
            command,
            expected,
            worker,
            params,
            slots,
            session_restart_reasons: Mutex::default(),
            pool_rss: WorkerRssTracker::default(),
            rss_tracker,
        })
    }

    pub fn stats(&self) -> SessionPoolStats {
        for slot in &lock_unpoison(&self.slots.state).idle {
            if let Some(session) = slot
                .session
                .as_ref()
                .filter(|s| s.identity() == &self.expected)
            {
                self.observe_rss(session.peak_rss_bytes());
            }
        }
        let session_restart_reasons = *lock_unpoison(&self.session_restart_reasons);
        SessionPoolStats {
            session_restart_count: session_restart_reasons.total(),
            session_restart_reasons,
            peak_worker_rss_bytes: self.pool_rss.peak_worker_rss_bytes(),
        }
    }

    pub(crate) fn matches(
        &self,
        command: &WorkerCommand,
        expected: &WorkerHello,
        worker: &RegisteredWorker,
        params: SessionParams,
    ) -> bool {
        self.command == *command
            && self.expected == *expected
            && self.worker == *worker
            && self.params == params
    }

    pub(crate) fn registered_worker(&self, command: &WorkerCommand) -> Option<RegisteredWorker> {
        (self.command == *command).then(|| self.worker.clone())
    }

    fn observe_rss(&self, value: Option<u64>) {
        self.pool_rss.observe_started_child(value);
        self.rss_tracker.observe_started_child(value);
    }

    pub fn classify_pdf(
        &self,
        request: &ClassifyOperation,
        timeout: Duration,
    ) -> Result<SessionOperationOutcome<ClassifyResult>, SessionOperationFailure> {
        let started = Instant::now();
        if let Err(failure) = crate::parsers::classifier::validate_classifier_source_before(request)
        {
            return Err(SessionOperationFailure {
                failure,
                transport: WorkerTransport::NotApplicable,
                attempt_count: 0,
                duration_ms: 0,
            });
        }
        let result = self.execute_operation(
            &request.file_path,
            timeout.saturating_sub(started.elapsed()),
            |session, remaining| session_classify(session, request, remaining),
        )?;
        if let Err(failure) = crate::parsers::classifier::validate_classifier_source_after(request)
        {
            return Err(SessionOperationFailure {
                failure,
                transport: result.transport,
                attempt_count: result.attempt_count,
                duration_ms: result.duration_ms,
            });
        }
        Ok(result)
    }

    pub fn parse_pdf(
        &self,
        request: &ParseOperation,
        timeout: Duration,
    ) -> Result<SessionOperationOutcome<ParseResult>, SessionOperationFailure> {
        self.parse_worker(request, timeout)
    }

    pub fn parse_worker(
        &self,
        request: &ParseOperation,
        timeout: Duration,
    ) -> Result<SessionOperationOutcome<ParseResult>, SessionOperationFailure> {
        let started = Instant::now();
        let timeout = if matches!(
            request.parser_limits,
            ai_daily_worker_contract::ParserLimits::PdfImageOcr { .. }
        ) {
            timeout.min(Duration::from_millis(
                request
                    .remaining_timeout_ms
                    .min(ai_daily_worker_contract::OCR_TIMEOUT_MS),
            ))
        } else {
            timeout
        };
        if let Err(failure) = crate::parsers::validate_worker_request(&self.worker, request) {
            return Err(SessionOperationFailure {
                failure,
                transport: WorkerTransport::NotApplicable,
                attempt_count: 0,
                duration_ms: 0,
            });
        }
        let response = self.execute_operation(
            &request.file_path,
            timeout.saturating_sub(started.elapsed()),
            |session, remaining| {
                let mut attempt_request = request.clone();
                attempt_request.remaining_timeout_ms = duration_ms(remaining);
                session_parse(session, &attempt_request, remaining)
            },
        )?;
        let value =
            crate::parsers::validate_session_worker_response(&self.worker, request, response.value)
                .map_err(|failure| SessionOperationFailure {
                    failure,
                    transport: response.transport,
                    attempt_count: response.attempt_count,
                    duration_ms: response.duration_ms,
                })?;
        Ok(SessionOperationOutcome { value, ..response })
    }

    fn execute_operation<T, SessionCall>(
        &self,
        file_path: &str,
        timeout: Duration,
        mut session_call: SessionCall,
    ) -> Result<SessionOperationOutcome<T>, SessionOperationFailure>
    where
        SessionCall: FnMut(&mut WorkerSession, Duration) -> Result<T, SessionError>,
    {
        let deadline_origin = Instant::now();
        let timeout_failure = |attempts, duration| {
            operation_failure(
                SessionError::Timeout.diagnostic(Some(file_path)),
                transport_for_attempts(attempts),
                attempts,
                duration,
            )
        };
        let remaining = |attempts, duration| {
            timeout
                .checked_sub(deadline_origin.elapsed())
                .filter(|left| !left.is_zero())
                .ok_or_else(|| timeout_failure(attempts, duration))
        };
        let mut transport_failure_index = 0_u32;
        let mut session_attempt_count = 0_u64;
        let mut session_duration_ms = 0_u64;

        loop {
            let mut permit = self
                .slots
                .checkout(&self.expected, deadline_origin, timeout)
                .ok_or_else(|| timeout_failure(session_attempt_count, session_duration_ms))?;
            let slot = &mut permit.slot;
            remaining(session_attempt_count, session_duration_ms)?;
            let retire_reason = slot.session.as_ref().and_then(|session| {
                if session.identity() == &self.expected {
                    session.recycle_reason()
                } else {
                    Some(SessionRestartReason::LaneSwitch)
                }
            });
            if let Some(reason) = retire_reason {
                slot.retire(reason);
            }

            if slot.session.is_none() {
                match WorkerSession::start_observed(
                    &self.command,
                    &self.expected,
                    self.params,
                    &self.rss_tracker,
                    remaining(session_attempt_count, session_duration_ms)?,
                ) {
                    Ok(session) => {
                        self.observe_rss(session.peak_rss_bytes());
                        if let Some(reason) = slot.replacement_pending.take() {
                            reason.record(&mut lock_unpoison(&self.session_restart_reasons));
                        }
                        slot.session = Some(session);
                    }
                    Err(error) => {
                        // A failed initial spawn/hello is not a replacement and
                        // did not start the file's logical operation. The
                        // reason from an earlier retirement stays on the slot.
                        match retry_action(&error, transport_failure_index) {
                            RetryAction::RetrySession => {
                                transport_failure_index += 1;
                                continue;
                            }
                            RetryAction::GiveUp => {
                                return Err(operation_failure(
                                    error.diagnostic(Some(file_path)),
                                    transport_for_attempts(session_attempt_count),
                                    session_attempt_count,
                                    session_duration_ms,
                                ));
                            }
                        }
                    }
                }
            }

            let time_left = remaining(session_attempt_count, session_duration_ms)?;
            let attempt_started = Instant::now();
            let outcome = session_call(
                slot.session.as_mut().expect("session was started above"),
                time_left,
            );
            let outcome = if deadline_origin.elapsed() >= timeout {
                Err(SessionError::Timeout)
            } else {
                outcome
            };
            session_attempt_count = session_attempt_count.saturating_add(1);
            session_duration_ms =
                session_duration_ms.saturating_add(observed_duration_ms(attempt_started.elapsed()));
            match outcome {
                Ok(result) => {
                    if let Some(session) = slot.session.as_ref() {
                        self.observe_rss(session.peak_rss_bytes());
                    }
                    return Ok(SessionOperationOutcome {
                        value: result,
                        transport: WorkerTransport::Session,
                        attempt_count: session_attempt_count,
                        duration_ms: session_duration_ms,
                    });
                }
                Err(error) => {
                    if let Some(session) = slot.session.as_ref() {
                        self.observe_rss(session.peak_rss_bytes());
                    }
                    slot.retire(SessionRestartReason::after_error(&error));
                    match retry_action(&error, transport_failure_index) {
                        RetryAction::RetrySession => {
                            transport_failure_index += 1;
                        }
                        RetryAction::GiveUp => {
                            return Err(operation_failure(
                                error.diagnostic(Some(file_path)),
                                WorkerTransport::Session,
                                session_attempt_count,
                                session_duration_ms,
                            ));
                        }
                    }
                }
            }
        }
    }
}

fn operation_failure(
    failure: ParseFailure,
    transport: WorkerTransport,
    attempt_count: u64,
    duration_ms: u64,
) -> SessionOperationFailure {
    SessionOperationFailure {
        failure,
        transport,
        attempt_count,
        duration_ms,
    }
}

fn transport_for_attempts(session_attempt_count: u64) -> WorkerTransport {
    if session_attempt_count > 0 {
        WorkerTransport::Session
    } else {
        WorkerTransport::NotApplicable
    }
}

fn observed_duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

struct SlotPermit<'a> {
    slots: &'a SessionSlots,
    lane: WorkerKind,
    slot: SessionSlot,
}

impl Drop for SlotPermit<'_> {
    fn drop(&mut self) {
        let mut state = lock_unpoison(&self.slots.state);
        state.busy[self.lane as usize] -= 1;
        state.idle.push(std::mem::take(&mut self.slot));
        self.slots.changed.notify_all();
    }
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().max(1).min(u64::MAX as u128) as u64
}

#[cfg(all(test, windows))]
mod tests;
