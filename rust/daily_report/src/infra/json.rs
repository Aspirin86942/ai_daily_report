use crate::ReportError;
use serde::{Serialize, Serializer};
use serde_json::{value::RawValue, Map, Number, Value};
use sha2::{Digest, Sha256};

pub fn digest(bytes: &[u8]) -> String {
    daily_parsers::hex(&Sha256::digest(bytes))
}

pub fn valid_hash(value: &str) -> bool {
    hex(value, 64)
}

pub(crate) fn valid_run_id(value: &str) -> bool {
    hex(value, 32)
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// Escape real keys starting with NUL on input. Only an unescaped tag can hold a number.
pub(crate) const FROZEN_NUMBER: &str = "\0ai_daily_report::frozen_number";

pub(crate) fn frozen_json_key(key: &str) -> &str {
    if key.starts_with("\0\0") {
        &key[1..]
    } else {
        key
    }
}

pub(crate) fn frozen_float(text: &str) -> Option<f64> {
    if text.contains(['.', 'e', 'E']) {
        text.parse::<f64>().ok().filter(|value| value.is_finite())
    } else {
        None
    }
}

pub(crate) fn frozen_number(value: &Value) -> Option<&str> {
    frozen_number_token(value).map(|(text, _)| text)
}

fn frozen_number_token(value: &Value) -> Option<(&str, &RawValue)> {
    let map = value.as_object().filter(|map| map.len() == 1)?;
    let text = map.get(FROZEN_NUMBER)?.as_str()?;
    if !text.starts_with(|ch: char| ch == '-' || ch.is_ascii_digit()) {
        return None;
    }
    Some((text, serde_json::from_str(text).ok()?))
}

pub(crate) fn decode_frozen_json(raw: &RawValue, depth: usize) -> Result<Value, ReportError> {
    if depth == 128 {
        return Err(ReportError::new(
            "SNAPSHOT_JSON_INVALID",
            "冻结 JSON 嵌套层数超限",
        ));
    }
    let text = raw.get();
    match text.as_bytes()[0] {
        b'{' => {
            let fields =
                serde_json::from_str::<std::collections::BTreeMap<String, &RawValue>>(text)?;
            Ok(Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| {
                        let key = if key.starts_with('\0') {
                            format!("\0{key}")
                        } else {
                            key
                        };
                        Ok((key, decode_frozen_json(value, depth + 1)?))
                    })
                    .collect::<Result<Map<_, _>, ReportError>>()?,
            ))
        }
        b'[' => Ok(Value::Array(
            serde_json::from_str::<Vec<&RawValue>>(text)?
                .into_iter()
                .map(|value| decode_frozen_json(value, depth + 1))
                .collect::<Result<_, _>>()?,
        )),
        b'-' | b'0'..=b'9' => {
            if let Ok(number) = text.parse::<u64>() {
                return Ok(Value::from(number));
            }
            if let Ok(number) = text.parse::<i64>() {
                return Ok(Value::from(number));
            }
            // The historical reader used a lowercase exponent with an explicit sign.
            let number = match text.split_once(['e', 'E']) {
                Some((mantissa, exponent)) if !exponent.starts_with(['+', '-']) => {
                    format!("{mantissa}e+{exponent}")
                }
                Some((mantissa, exponent)) => format!("{mantissa}e{exponent}"),
                None => text.to_owned(),
            };
            if let Some(float) = frozen_float(&number).and_then(Number::from_f64) {
                if float.to_string() == number {
                    return Ok(Value::Number(float));
                }
            }
            Ok(Value::Object(Map::from_iter([(
                FROZEN_NUMBER.into(),
                Value::String(number),
            )])))
        }
        _ => Ok(serde_json::from_str(text)?),
    }
}

struct EncodedJson<'a> {
    value: &'a Value,
    canonical: bool,
}

impl<'a> EncodedJson<'a> {
    fn child(&self, value: &'a Value) -> Self {
        Self {
            value,
            canonical: self.canonical,
        }
    }
}

impl<'a> Serialize for EncodedJson<'a> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Some((text, raw)) = frozen_number_token(self.value) {
            if self.canonical {
                if let Some(float) = frozen_float(text) {
                    return serialize_canonical_float(float, serializer);
                }
            }
            return raw.serialize(serializer);
        }
        match self.value {
            Value::Array(values) => {
                serializer.collect_seq(values.iter().map(|value| self.child(value)))
            }
            Value::Object(values) => {
                let entry = |(key, value): (&'a String, &'a Value)| {
                    (frozen_json_key(key), self.child(value))
                };
                if self.canonical && !values.keys().is_sorted() {
                    let mut sorted: Vec<_> = values.iter().collect();
                    sorted.sort_unstable_by_key(|(key, _)| *key);
                    serializer.collect_map(sorted.into_iter().map(entry))
                } else {
                    serializer.collect_map(values.iter().map(entry))
                }
            }
            Value::Number(number) if self.canonical && number.is_f64() => {
                let float = number
                    .as_f64()
                    .ok_or_else(|| serde::ser::Error::custom("浮点数超出范围"))?;
                serialize_canonical_float(float, serializer)
            }
            value => value.serialize(serializer),
        }
    }
}

fn serialize_number_token<S: Serializer>(token: &str, serializer: S) -> Result<S::Ok, S::Error> {
    serde_json::from_str::<&RawValue>(token)
        .map_err(serde::ser::Error::custom)?
        .serialize(serializer)
}

fn serialize_canonical_float<S: Serializer>(number: f64, serializer: S) -> Result<S::Ok, S::Error> {
    if number != 0.0 && (number.abs() < 0.0001 || number.abs() >= 1e16) {
        let exponent_form = format!("{number:e}");
        let (mantissa, exponent) = exponent_form.split_once('e').unwrap();
        let exponent: i32 = exponent.parse().unwrap();
        return serialize_number_token(&format!("{mantissa}e{exponent:+03}"), serializer);
    }
    number.serialize(serializer)
}

pub(crate) fn frozen_report_json(value: &Value) -> Result<String, ReportError> {
    Ok(serde_json::to_string(&EncodedJson {
        value,
        canonical: false,
    })?)
}

pub(crate) fn read_frozen_report_json(text: &str) -> Result<Value, ReportError> {
    decode_frozen_json(serde_json::from_str::<&RawValue>(text)?, 0)
}

/// Match the existing Python canonical JSON contract. Existing snapshots are
/// always hashed as original bytes and are never rewritten through this function.
pub fn canonical_json(value: &Value) -> Result<Vec<u8>, ReportError> {
    Ok(serde_json::to_vec(&EncodedJson {
        value,
        canonical: true,
    })?)
}

pub fn fingerprint(value: &Value) -> Result<String, ReportError> {
    Ok(digest(&canonical_json(value)?))
}

#[cfg(test)]
mod tests {
    use super::{digest, valid_hash, valid_run_id};

    #[test]
    fn hex_validators_require_exact_lowercase_lengths() {
        let hash = digest(b"sample");
        assert!(valid_hash(&hash));
        assert!(!valid_hash(&hash.to_ascii_uppercase()));
        assert!(!valid_hash(&hash[..63]));
        assert!(valid_run_id(&hash[..32]));
        assert!(!valid_run_id(&hash));
    }
}
