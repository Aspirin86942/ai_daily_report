use crate::ReportError;
use chrono::{
    DateTime, Datelike, Duration, FixedOffset, NaiveDate, NaiveTime, TimeZone, Utc, Weekday,
};
use serde::{Deserialize, Serialize};

pub fn shanghai() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).unwrap()
}

pub fn new_run_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let identity = format!(
        "{}:{}:{}",
        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    crate::snapshots::digest(identity.as_bytes())[..32].to_owned()
}

pub fn parse_date(value: &str) -> Result<NaiveDate, ReportError> {
    let parsed = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| ReportError::new("DATE_INVALID", "日期须为 YYYY-MM-DD"))?;
    if value != parsed.format("%Y-%m-%d").to_string()
        || parsed.year() < 1
        || parsed >= NaiveDate::from_ymd_opt(9999, 12, 31).unwrap()
    {
        return Err(ReportError::new("DATE_INVALID", "日期必须允许计算次日零点"));
    }
    Ok(parsed)
}

pub fn utc_window(date: NaiveDate) -> Result<(DateTime<Utc>, DateTime<Utc>), ReportError> {
    let next = date
        .succ_opt()
        .ok_or_else(|| ReportError::new("DATE_INVALID", "日期超出范围"))?;
    let midnight = |day: NaiveDate| {
        shanghai()
            .from_local_datetime(&day.and_hms_opt(0, 0, 0).unwrap())
            .unwrap()
            .with_timezone(&Utc)
    };
    Ok((midnight(date), midnight(next)))
}

pub fn target_date(
    now: DateTime<Utc>,
    explicit: Option<NaiveDate>,
    scheduled: bool,
    due: NaiveTime,
) -> Result<NaiveDate, ReportError> {
    let local = now.with_timezone(&shanghai());
    if scheduled && explicit.is_some() {
        return Err(ReportError::new(
            "DATE_CONFLICT",
            "定时运行不能指定目标日期",
        ));
    }
    if let Some(target) = explicit {
        if target >= local.date_naive() {
            return Err(ReportError::new(
                "DATE_NOT_COMPLETED",
                "目标日期必须是已结束的自然日",
            ));
        }
        return Ok(target);
    }
    Ok(local.date_naive()
        - Duration::days(if scheduled && local.time() < due {
            2
        } else {
            1
        }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ReportSource {
    Db,
    Scan,
    // Historical frozen inputs still deserialize this source.
    #[value(skip)]
    Archive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportMode {
    Daily,
    Weekly,
    Monthly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunContext {
    pub run_id: String,
    pub scope_id: String,
    pub target_date: NaiveDate,
    pub timezone: String,
    pub window_start_utc: DateTime<Utc>,
    pub window_end_utc: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub config_fingerprint: String,
}

impl RunContext {
    pub fn new(
        run_id: String,
        now: DateTime<Utc>,
        target: NaiveDate,
        config_fingerprint: String,
    ) -> Result<Self, ReportError> {
        let (window_start_utc, window_end_utc) = utc_window(target)?;
        let context = Self {
            run_id,
            scope_id: "personal_work".into(),
            target_date: target,
            timezone: "Asia/Shanghai".into(),
            window_start_utc,
            window_end_utc,
            started_at: now,
            config_fingerprint,
        };
        context.validate()?;
        Ok(context)
    }

    pub fn validate(&self) -> Result<(), ReportError> {
        if self.run_id.len() != 32
            || !self
                .run_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.scope_id != "personal_work"
            || self.timezone != "Asia/Shanghai"
            || (self.window_start_utc, self.window_end_utc) != utc_window(self.target_date)?
        {
            return Err(ReportError::new(
                "RUN_CONTEXT_INVALID",
                "运行身份、业务范围或日期窗口不一致",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Period {
    pub mode: ReportMode,
    pub source: ReportSource,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub label: String,
}

impl Period {
    pub fn weekly(
        today: NaiveDate,
        label: Option<&str>,
        source: ReportSource,
    ) -> Result<Self, ReportError> {
        let week = today.iso_week();
        let label = label
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}-W{:02}", week.year(), week.week()));
        let invalid = || ReportError::new("WEEK_INVALID", "周须为有效的 YYYY-Wnn");
        if label.len() != 8 || !label.is_ascii() || &label[4..6] != "-W" {
            return Err(invalid());
        }
        let year = label[..4].parse().map_err(|_| invalid())?;
        let number = label[6..].parse().map_err(|_| invalid())?;
        let start = NaiveDate::from_isoywd_opt(year, number, Weekday::Mon).ok_or_else(invalid)?;
        Ok(Self {
            mode: ReportMode::Weekly,
            source,
            start,
            end: start + Duration::days(6),
            label,
        })
    }

    pub fn monthly(
        today: NaiveDate,
        label: Option<&str>,
        source: ReportSource,
    ) -> Result<Self, ReportError> {
        let label = label
            .map(str::to_owned)
            .unwrap_or_else(|| today.format("%Y-%m").to_string());
        if label.len() != 7 {
            return Err(ReportError::new("MONTH_INVALID", "月份须为 YYYY-MM"));
        }
        let start = parse_date(&format!("{label}-01"))?;
        let (year, month) = if start.month() == 12 {
            (start.year() + 1, 1)
        } else {
            (start.year(), start.month() + 1)
        };
        let next = NaiveDate::from_ymd_opt(year, month, 1)
            .ok_or_else(|| ReportError::new("MONTH_INVALID", "月份超出范围"))?;
        Ok(Self {
            mode: ReportMode::Monthly,
            source,
            start,
            end: next - Duration::days(1),
            label,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ReportRequest {
    pub period: Period,
    pub context: RunContext,
    pub input: String,
    pub save: bool,
    pub dry_run: bool,
    pub scheduled: bool,
    pub regeneration_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Created,
    Scanning,
    Prepared,
    Generating,
    DbCommitted,
    Published,
    Preview,
    Observed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportOutcome {
    pub run_id: String,
    pub target_date: String,
    pub quality: String,
    pub markdown: String,
    pub output_path: Option<std::path::PathBuf>,
    pub action: String,
    pub model_calls: u64,
}
