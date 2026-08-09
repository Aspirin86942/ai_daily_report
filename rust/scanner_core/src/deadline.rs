//! One monotonic deadline origin shared by the run shell, scheduler and store.

use std::time::Instant;

/// Fixed tail reserve for envelope construction and terminal finalization.
pub const FINALIZATION_RESERVE_MS: u64 = 2_000;

/// Monotonic clock source. Values are milliseconds since one stable origin.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// Production clock. Clones retain the same `Instant` origin.
#[derive(Debug, Clone)]
pub struct RealClock {
    origin: Instant,
}

impl RealClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// Deadlines derived exactly once immediately after `begin_run` succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunDeadlines {
    pub origin_ms: u64,
    pub work_deadline_ms: u64,
    pub absolute_deadline_ms: u64,
}

impl RunDeadlines {
    pub fn derive(total_deadline_ms: u64, clock: &dyn Clock) -> Result<Self, String> {
        let origin_ms = clock.now_ms();
        let absolute_deadline_ms = origin_ms
            .checked_add(total_deadline_ms)
            .ok_or_else(|| "deadline arithmetic overflowed".to_string())?;
        let work_deadline_ms = absolute_deadline_ms
            .checked_sub(FINALIZATION_RESERVE_MS)
            .ok_or_else(|| "work deadline underflowed".to_string())?;
        let deadlines = Self {
            origin_ms,
            work_deadline_ms,
            absolute_deadline_ms,
        };
        deadlines.validate(total_deadline_ms)?;
        Ok(deadlines)
    }

    pub fn validate(self, total_deadline_ms: u64) -> Result<(), String> {
        if self.absolute_deadline_ms.checked_sub(self.origin_ms) != Some(total_deadline_ms)
            || self.absolute_deadline_ms.checked_sub(self.work_deadline_ms)
                != Some(FINALIZATION_RESERVE_MS)
            || self.work_deadline_ms <= self.origin_ms
        {
            return Err(
                "deadline pair does not match the profile total and finalization reserve"
                    .to_string(),
            );
        }
        Ok(())
    }

    pub fn remaining_to_work_deadline(self, clock: &dyn Clock) -> u64 {
        self.work_deadline_ms.saturating_sub(clock.now_ms())
    }

    pub fn remaining_to_absolute_deadline(self, clock: &dyn Clock) -> u64 {
        self.absolute_deadline_ms.saturating_sub(clock.now_ms())
    }
}
