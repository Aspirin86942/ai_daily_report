use std::cell::OnceCell;
use std::collections::VecDeque;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};

use super::*;

#[derive(Clone, Copy)]
pub(super) enum WorkLane {
    Inline,
    Office,
    Pdf,
}

#[derive(Default)]
struct PendingJobs {
    waiting: [VecDeque<usize>; 3],
    active: [usize; 3],
}

impl PendingJobs {
    fn next_runnable(&mut self, parallel: bool) -> Option<(usize, usize)> {
        let inline = WorkLane::Inline as usize;
        let office = WorkLane::Office as usize;
        let pdf = WorkLane::Pdf as usize;
        let has_demand = |lane: usize| !self.waiting[lane].is_empty() || self.active[lane] > 0;
        let mixed = parallel && has_demand(office) && has_demand(pdf);
        // Fill one slot per native lane first in a mixed stage. Inline work
        // can use any slot left after those runnable native requests.
        let lane = self
            .waiting
            .iter()
            .enumerate()
            .filter(|(lane, _)| !mixed || *lane == inline || self.active[*lane] == 0)
            .filter_map(|(lane, tickets)| Some((mixed && lane == inline, *tickets.front()?, lane)))
            .min()
            .map(|(_, _, lane)| lane)?;
        let index = self.waiting[lane]
            .pop_front()
            .expect("nonempty runnable lane");
        self.active[lane] += 1;
        Some((index, lane))
    }
}

pub(super) enum DispatchEvent<R> {
    Completed(usize, Option<R>),
    FlushCache,
}

pub(super) struct RollingExecutor {
    concurrency: usize,
    pool: OnceCell<Result<rayon::ThreadPool, SchedulerFailure>>,
}

impl RollingExecutor {
    pub(super) fn new(concurrency: usize) -> Self {
        Self {
            concurrency,
            pool: OnceCell::new(),
        }
    }

    pub(super) fn run<T, R>(
        &self,
        tasks: &[T],
        clock: &dyn Clock,
        work_deadline: u64,
        lane_for: impl Fn(&T) -> WorkLane,
        invoke: impl Fn(&T) -> Option<R> + Sync,
        mut accept: impl FnMut(DispatchEvent<R>) -> Option<u64>,
    ) -> Result<(), SchedulerFailure>
    where
        T: Sync,
        R: Send,
    {
        match tasks {
            [] => return Ok(()),
            [task] => {
                let result = (clock.now_ms() < work_deadline)
                    .then(|| invoke(task))
                    .flatten();
                accept(DispatchEvent::Completed(0, result));
                return Ok(());
            }
            _ => {}
        }
        if clock.now_ms() >= work_deadline {
            for index in 0..tasks.len() {
                accept(DispatchEvent::Completed(index, None));
            }
            return Ok(());
        }

        let pool = self
            .pool
            .get_or_init(|| {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(self.concurrency)
                    .build()
                    .map_err(|_| {
                        SchedulerFailure::new(
                            ErrorCode::InternalError,
                            "scheduler worker pool could not be created".to_string(),
                            true,
                            DiagnosticStage::Process,
                        )
                    })
            })
            .as_ref()
            .map_err(Clone::clone)?;
        let mut pending = PendingJobs::default();
        for (index, task) in tasks.iter().enumerate() {
            pending.waiting[lane_for(task) as usize].push_back(index);
        }
        let (sender, receiver) = sync_channel(self.concurrency);
        let invoke = &invoke;
        pool.in_place_scope(|scope| {
            let mut in_flight = 0;
            let mut completed = None;
            let mut flush_at = None;
            loop {
                // Refill the freed slot before merging its result or writing SQLite.
                while in_flight < self.concurrency && clock.now_ms() < work_deadline {
                    let Some((index, lane)) = pending.next_runnable(self.concurrency > 1) else {
                        break;
                    };
                    let task = &tasks[index];
                    let sender = sender.clone();
                    scope.spawn(move |_| {
                        // Always signal completion, including a panicking adapter.
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            (clock.now_ms() < work_deadline)
                                .then(|| invoke(task))
                                .flatten()
                        }));
                        let _ = sender.send((index, lane, result));
                    });
                    in_flight += 1;
                }
                if let Some((index, result)) = completed.take() {
                    flush_at = accept(DispatchEvent::Completed(index, result));
                }
                if in_flight == 0 {
                    break;
                }
                let message = match flush_at {
                    Some(at) => {
                        let wait = Duration::from_millis(at.saturating_sub(clock.now_ms()));
                        match receiver.recv_timeout(wait) {
                            Err(RecvTimeoutError::Timeout) => {
                                if clock.now_ms() >= at {
                                    flush_at = accept(DispatchEvent::FlushCache);
                                }
                                continue;
                            }
                            message => message.expect("worker completion channel"),
                        }
                    }
                    None => receiver.recv().expect("worker completion channel"),
                };
                let (index, lane, result) = message;
                pending.active[lane] -= 1;
                completed = Some((index, result.unwrap_or_else(|panic| resume_unwind(panic))));
                in_flight -= 1;
            }
            for index in pending.waiting.into_iter().flatten() {
                accept(DispatchEvent::Completed(index, None));
            }
        });
        Ok(())
    }
}
