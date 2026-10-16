use crate::ReportError;
use chrono::{DateTime, FixedOffset};
use minijinja::{context, Environment, Error, ErrorKind, Value};
use serde_json::Value as JsonValue;
pub mod data;
pub mod legacy;
pub mod period;

pub const DAILY_TEMPLATE_V3: &str =
    include_str!("../../../templates/evidence_report_template_v3.md");

pub fn render_daily(report: &data::DailyReport, template: &str) -> Result<String, ReportError> {
    report.validate()?;
    let mut value = serde_json::to_value(report)?;
    value["source_labels"] = serde_json::to_value(report.source_labels())?;
    value["historical_reconstruction"] = JsonValue::Bool(report.is_historical()?);
    let body = render(template, &value, &report.generated_at)?;
    Ok(format!(
        "{}\n",
        body.replace("\r\n", "\n").replace('\r', "\n").trim_end()
    ))
}

pub fn evidence_text(value: &str) -> String {
    let escaped = value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let mut output = String::with_capacity(escaped.len());
    for ch in escaped.chars() {
        if "\\`*_{}[]()#+!|>".contains(ch) {
            output.push('\\');
        }
        output.push(if ch == '\r' || ch == '\n' { ' ' } else { ch });
    }
    output
}

pub fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.add_filter("evidence_text", |value: String| evidence_text(&value));
    env.add_filter("shanghai_time", |value: String| -> Result<String, Error> {
        let timestamp = DateTime::parse_from_rfc3339(&value)
            .map_err(|_| Error::new(ErrorKind::InvalidOperation, "invalid report timestamp"))?;
        Ok(timestamp
            .with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap())
            .format("%Y-%m-%d %H:%M:%S +08:00")
            .to_string())
    });
    // Only the two map methods used by the frozen historical template are needed.
    env.set_unknown_method_callback(|state, value, method, args| match method {
        "items" if args.is_empty() => state.apply_filter("items", std::slice::from_ref(value)),
        "get" if args.len() == 1 || args.len() == 2 => {
            let item = value.get_item(&args[0])?;
            Ok(if item.is_undefined() {
                args.get(1).cloned().unwrap_or(Value::from(()))
            } else {
                item
            })
        }
        _ => Err(Error::new(ErrorKind::UnknownMethod, method.to_owned())),
    });
    env
}

pub fn render(template: &str, report: &JsonValue, now: &str) -> Result<String, ReportError> {
    environment()
        .render_str(
            template,
            context! { report => template_value(report), now => now },
        )
        .map_err(|error| ReportError::new("TEMPLATE_INVALID", error.to_string()))
}

// serde_json's arbitrary_precision feature serializes numbers through a private
// map. Convert actual JSON values directly so that numeric report fields remain
// numbers in the template rather than exposing that internal representation.
pub(crate) fn template_value(value: &JsonValue) -> Value {
    match value {
        JsonValue::Null => Value::from(()),
        JsonValue::Bool(value) => Value::from(*value),
        JsonValue::String(value) => Value::from(value.clone()),
        JsonValue::Number(value) => {
            if let Some(number) = value.as_i64() {
                Value::from(number)
            } else if let Some(number) = value.as_u64() {
                Value::from(number)
            } else if value.is_f64() {
                Value::from(value.as_f64().expect("finite JSON number"))
            } else {
                Value::from(value.to_string())
            }
        }
        JsonValue::Array(values) => {
            Value::from(values.iter().map(template_value).collect::<Vec<_>>())
        }
        JsonValue::Object(values) => Value::from_pairs(
            values
                .iter()
                .map(|(key, value)| (key.clone(), template_value(value))),
        ),
    }
}
