use crate::{
    model::{contracts::invalid, plan::StepKind},
    types::{Period, ReportMode},
    ReportError,
};
use serde_json::{json, Value};

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
pub const WEEK_TEMPLATE: &str = include_str!("../../../../templates/weekly_template.md");
pub const MONTH_TEMPLATE: &str = include_str!("../../../../templates/monthly_template.md");

pub fn fields(kind: StepKind) -> &'static [&'static str] {
    if kind == StepKind::Weekly {
        WEEK_FIELDS
    } else {
        MONTH_FIELDS
    }
}
pub fn schema(kind: StepKind) -> Value {
    let properties: serde_json::Map<String, Value> = fields(kind)
        .iter()
        .map(|key| ((*key).into(), json!({"type":"string"})))
        .collect();
    json!({"type":"object","additionalProperties":false,"properties":properties,"required":fields(kind)})
}
pub fn kind(period: &Period) -> StepKind {
    if period.mode == ReportMode::Weekly {
        StepKind::Weekly
    } else {
        StepKind::Monthly
    }
}
pub fn validate(kind: StepKind, value: &Value, target: &Value) -> Result<(), ReportError> {
    let object = value.as_object().ok_or_else(invalid)?;
    if !matches!(kind, StepKind::Weekly | StepKind::Monthly)
        || object.len() != fields(kind).len()
        || fields(kind).iter().any(|field| {
            object
                .get(*field)
                .and_then(Value::as_str)
                .is_none_or(|s| s.chars().count() > 20000)
        })
    {
        return Err(invalid());
    }
    if kind == StepKind::Weekly {
        if value["week_label"] != target["week_label"]
            || value["date_range"] != target["date_range"]
        {
            return Err(invalid());
        }
    } else if value["year_month"] != target["year_month"] {
        return Err(invalid());
    }
    Ok(())
}
pub fn validate_identity(kind: &str, value: &Value) -> Result<String, ReportError> {
    let period = match kind {
        "weekly" => Period::weekly(
            crate::types::parse_date("2026-01-01")?,
            value["week_label"].as_str(),
            crate::types::ReportSource::Db,
        )?,
        "monthly" => Period::monthly(
            crate::types::parse_date("2026-01-01")?,
            value["year_month"].as_str(),
            crate::types::ReportSource::Db,
        )?,
        _ => return Err(invalid()),
    };
    let target = json!({"week_label":period.label,"year_month":period.label,"date_range":format!("{} ~ {}",period.start,period.end)});
    validate(self::kind(&period), value, &target)?;
    Ok(period.label)
}
pub fn prompt(kind: StepKind, target: &Value) -> Result<String, ReportError> {
    let template = if kind == StepKind::Weekly {
        include_str!("../../../../templates/weekly_prompt.md")
    } else {
        include_str!("../../../../templates/monthly_prompt.md")
    };
    let mut template = template.to_owned();
    let mut context = target.clone();
    context["schema"] =
        json!(String::from_utf8(crate::snapshots::canonical_json(&schema(kind))?).unwrap());
    for field in [
        "schema",
        "week_label",
        "year_month",
        "data_source",
        "reports_summary",
        "missing_days",
        "file_context",
    ] {
        template = template.replace(&format!("{{{field}}}"), &format!("{{{{ {field} }}}}"));
    }
    super::environment()
        .render_str(&template, super::template_value(&context))
        .map_err(|error| ReportError::new("TEMPLATE_INVALID", error.to_string()))
}
