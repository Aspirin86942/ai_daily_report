use crate::infra::json::{frozen_float, frozen_json_key, frozen_number};
use crate::types::shanghai;
use chrono::DateTime;
use minijinja::{Environment, Error, ErrorKind, Value};
use serde_json::Value as JsonValue;
pub mod data;
pub mod period;
pub mod personal;

fn literal_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn source_verbatim(value: &str) -> String {
    literal_html(value).replace('\r', "&#13;")
}

pub fn evidence_text(value: &str) -> String {
    let escaped = literal_html(value);
    let mut output = String::with_capacity(escaped.len());
    for (index, ch) in escaped.chars().enumerate() {
        if "\\`*_|".contains(ch) || ch == '#' && index == 0 {
            output.push('\\');
        }
        output.push(if ch == '\r' || ch == '\n' { ' ' } else { ch });
    }
    output
}

fn markdown_inline(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let leading_digits = value.bytes().take_while(u8::is_ascii_digit).count();
    for (i, ch) in value.char_indices() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '\r' | '\n' => output.push(' '),
            _ => {
                if "\\`*_|[]~#".contains(ch)
                    || i == 0 && matches!(ch, '-' | '+' | '=')
                    || i == leading_digits && leading_digits > 0 && matches!(ch, '.' | ')')
                {
                    output.push('\\');
                }
                output.push(ch);
            }
        }
    }
    output
}

fn markdown_lines(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(markdown_inline)
        .collect::<Vec<_>>()
        .join("  \n")
}

pub fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.add_filter("evidence_text", |value: String| evidence_text(&value));
    env.add_filter("report_inline", markdown_inline);
    env.add_filter("report_lines", markdown_lines);
    env.add_filter("source_literal", literal_html);
    env.add_filter("source_verbatim", source_verbatim);
    env.add_filter("shanghai_time", |value: String| -> Result<String, Error> {
        let timestamp = DateTime::parse_from_rfc3339(&value)
            .map_err(|_| Error::new(ErrorKind::InvalidOperation, "invalid report timestamp"))?;
        Ok(timestamp
            .with_timezone(&shanghai())
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

// Historical templates displayed out-of-range JSON numbers as text.
pub(crate) fn template_value(value: &JsonValue) -> Value {
    if let Some(number) = frozen_number(value) {
        return frozen_float(number).map_or_else(|| Value::from(number), Value::from);
    }
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
                .map(|(key, value)| (frozen_json_key(key).to_owned(), template_value(value))),
        ),
    }
}

#[cfg(test)]
#[test]
fn frozen_report_template_preserves_json_numbers_and_empty_values() {
    let report = template_value(&serde_json::json!({
        "items": [], "value": null, "count": 32, "ratio": 0.75
    }));
    let template = "{% for x in report.items %}{{x}}{% else %}无{% endfor %}|{{report.value or '缺失'}}|{{report.count}}/{{report.ratio}}";
    assert_eq!(
        environment()
            .render_str(template, minijinja::context! { report })
            .unwrap(),
        "无|缺失|32/0.75"
    );
}
