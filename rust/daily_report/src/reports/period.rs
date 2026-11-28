use crate::{types::Period, ReportError};
use serde_json::Value;

fn invalid() -> ReportError {
    ReportError::new("MODEL_RESPONSE_INVALID", "模型响应结构、内容范围或引用无效")
}

const WEEK_FIELDS: &[&str] = &[
    "week_label",
    "date_range",
    "completed_work",
    "self_growth",
    "improvement_actions",
    "work_summary",
    "next_plan",
    "support_needed",
    "other_notes",
];
const MONTH_FIELDS: &[&str] = &[
    "year_month",
    "overview",
    "completed_work",
    "work_summary",
    "next_plan",
];

pub(crate) fn fields(kind: &str) -> Result<&'static [&'static str], ReportError> {
    match kind {
        "weekly" => Ok(WEEK_FIELDS),
        "monthly" => Ok(MONTH_FIELDS),
        _ => Err(invalid()),
    }
}

pub fn validate_identity(kind: &str, value: &Value) -> Result<String, ReportError> {
    let fields = fields(kind)?;
    let object = value.as_object().ok_or_else(invalid)?;
    if object.len() != fields.len()
        || fields.iter().any(|field| {
            object
                .get(*field)
                .and_then(Value::as_str)
                .is_none_or(|s| s.chars().count() > 20_000)
        })
    {
        return Err(invalid());
    }
    let label = value[fields[0]].as_str().ok_or_else(invalid)?;
    let period = if kind == "weekly" {
        let week = Period::weekly(label)?;
        if value["date_range"] != format!("{} ~ {}", week.start, week.end) {
            return Err(invalid());
        }
        week
    } else {
        Period::monthly(label)?
    };
    Ok(period.label)
}
