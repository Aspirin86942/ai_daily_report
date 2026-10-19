use crate::ReportError;
use chrono::{DateTime, Datelike, NaiveTime, Timelike, Utc, Weekday};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// Minute-level collection window on Shanghai weekdays (Monday through Friday).
/// `end` is inclusive down to the whole minute, so `18:00:59` is still inside a
/// window ending at `18:00`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkdayWindow {
    pub start: String,
    pub end: String,
}

impl WorkdayWindow {
    pub fn validate(&self) -> Result<(), ReportError> {
        let (start, end) = self.bounds()?;
        if start >= end {
            return Err(super::error("ARCHIVE_CONFIG_INVALID"));
        }
        Ok(())
    }

    /// Strict canonical `HH:MM` bounds. `chrono` alone accepts one-digit hours on
    /// input, so round-tripping the format rejects `9:00` and trailing seconds.
    fn bounds(&self) -> Result<(NaiveTime, NaiveTime), ReportError> {
        let parse = |value: &str| {
            let time = NaiveTime::parse_from_str(value, "%H:%M")
                .map_err(|_| super::error("ARCHIVE_CONFIG_INVALID"))?;
            if time.format("%H:%M").to_string() != value {
                return Err(super::error("ARCHIVE_CONFIG_INVALID"));
            }
            Ok(time)
        };
        Ok((parse(&self.start)?, parse(&self.end)?))
    }

    /// Ticks inside the window. Each tick is `start + n * interval` while it does
    /// not pass `end`; a final tick at `end` is appended when the interval grid
    /// does not land there, and duplicate bounds collapse to one tick.
    pub fn ticks(&self, interval_minutes: u32) -> Result<Vec<NaiveTime>, ReportError> {
        self.validate()?;
        if !(30..=1440).contains(&interval_minutes) || 1440 % interval_minutes != 0 {
            return Err(super::error("ARCHIVE_CONFIG_INVALID"));
        }
        let (start, end) = self.bounds()?;
        let start_minutes = start.hour() * 60 + start.minute();
        let end_minutes = end.hour() * 60 + end.minute();
        let mut ticks = vec![];
        let mut minute = start_minutes;
        while minute <= end_minutes {
            let tick = NaiveTime::from_hms_opt(minute / 60, minute % 60, 0)
                .ok_or_else(|| super::error("ARCHIVE_CONFIG_INVALID"))?;
            if ticks.last() != Some(&tick) {
                ticks.push(tick);
            }
            minute += interval_minutes;
        }
        if ticks.last() != Some(&end) {
            ticks.push(end);
        }
        Ok(ticks)
    }

    /// Whether `at` falls on a Shanghai weekday inside the window. Invalid
    /// windows fail closed instead of allowing every tick.
    pub fn allows(&self, at: DateTime<Utc>) -> bool {
        let Ok((start, end)) = self.bounds() else {
            return false;
        };
        if start >= end {
            return false;
        }
        let local = at.with_timezone(&crate::types::shanghai());
        if !matches!(
            local.weekday(),
            Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
        ) {
            return false;
        }
        let minutes = local.hour() * 60 + local.minute();
        minutes >= start.hour() * 60 + start.minute() && minutes <= end.hour() * 60 + end.minute()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveSettings {
    pub collection_interval_minutes: u32,
    /// `None` keeps the historical all-day interval trigger.
    pub collection_workday_window: Option<WorkdayWindow>,
    pub compress_extractions: bool,
    pub roots: Vec<PathBuf>,
    pub model_roots: Vec<PathBuf>,
    pub allow_work_dir_for_model: bool,
    pub model_ignored_prefixes: Vec<String>,
    pub model_group_max_units: usize,
    pub model_group_max_utf8_bytes: usize,
    pub read_workers: usize,
    pub parse_workers: usize,
    pub memory_mib: u64,
    pub system_reserve_mib: u64,
    pub parent_memory_mib: u64,
    pub worker_memory_mib: u64,
    pub run_seconds: u64,
    pub copy_seconds: u64,
    pub parse_seconds: u64,
    pub max_input_mib: u64,
    pub max_output_mib: u64,
    pub warn_free_mib: u64,
    pub reserve_free_mib: u64,
    /// Keys are worksheet/table labels; values are exact header names.
    pub table_keys: BTreeMap<String, Vec<String>>,
    pub daily_source: String,
}

impl Default for ArchiveSettings {
    fn default() -> Self {
        Self {
            collection_interval_minutes: 30,
            collection_workday_window: None,
            compress_extractions: true,
            roots: vec![],
            model_roots: vec![],
            allow_work_dir_for_model: true,
            model_ignored_prefixes: vec![".tmp-".into()],
            model_group_max_units: 16,
            model_group_max_utf8_bytes: 8192,
            read_workers: 2,
            parse_workers: 5,
            memory_mib: 3072,
            system_reserve_mib: 2048,
            parent_memory_mib: 512,
            worker_memory_mib: 512,
            run_seconds: 1200,
            copy_seconds: 120,
            parse_seconds: 300,
            max_input_mib: 512,
            max_output_mib: 256,
            warn_free_mib: 20480,
            reserve_free_mib: 5120,
            table_keys: BTreeMap::new(),
            daily_source: "scan".into(),
        }
    }
}

impl ArchiveSettings {
    pub fn validate(&self) -> Result<(), ReportError> {
        if !(1..=64).contains(&self.model_group_max_units)
            || !(1024..=32768).contains(&self.model_group_max_utf8_bytes)
            || self.model_ignored_prefixes.iter().any(|prefix| {
                prefix.is_empty() || prefix.len() > 128 || prefix.contains(['/', '\\', '\r', '\n'])
            })
        {
            return Err(super::error("ARCHIVE_CONFIG_INVALID"));
        }
        if !(30..=1440).contains(&self.collection_interval_minutes)
            || 1440 % self.collection_interval_minutes != 0
            || !(1..=2).contains(&self.read_workers)
            || !(1..=5).contains(&self.parse_workers)
            || self.worker_memory_mib != 512
            || self.parent_memory_mib < 512
            || self.system_reserve_mib < 2048
            || self.memory_mib < self.parent_memory_mib + self.worker_memory_mib
            || self.memory_mib > 3072
            || !(1..=1200).contains(&self.run_seconds)
            || !(1..=120).contains(&self.copy_seconds)
            || !(1..=300).contains(&self.parse_seconds)
            || !(1..=512).contains(&self.max_input_mib)
            || !(1..=256).contains(&self.max_output_mib)
            || self.reserve_free_mib > self.warn_free_mib
            || !matches!(self.daily_source.as_str(), "archive" | "scan")
        {
            return Err(super::error("ARCHIVE_CONFIG_INVALID"));
        }
        if let Some(window) = &self.collection_workday_window {
            window.validate()?;
        }
        Ok(())
    }
}
