use crate::{infra::json::read_frozen_report_json, ReportError};
use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
use serde::Deserializer;
use serde_json::{Map, Value};
use std::fmt;

pub(super) fn read(raw: &str, daily: bool) -> Result<Value, ReportError> {
    // Large legacy daily reports need only the displayed sections and source labels.
    let fields = if daily && raw.len() >= 8 * 1024 * 1024 {
        Fields::Daily
    } else {
        Fields::All
    };
    let Some(report) = read_native(raw, fields) else {
        return read_frozen_report_json(raw);
    };
    if matches!(fields, Fields::Daily) && matches!(report["schema_version"].as_u64(), Some(5 | 6)) {
        return read_native(raw, Fields::All).map_or_else(|| read_frozen_report_json(raw), Ok);
    }
    Ok(report)
}

fn read_native(raw: &str, fields: Fields) -> Option<Value> {
    let mut json = serde_json::Deserializer::from_str(raw);
    let value = fields.deserialize(&mut json).ok()?;
    json.end().ok()?;
    Some(value)
}

#[derive(Clone, Copy)]
enum Fields {
    All,
    Daily,
    Sources,
    Source,
    Discard,
}

impl<'de> DeserializeSeed<'de> for Fields {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Fields {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("report JSON")
    }

    fn visit_unit<E: Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E: Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: Error>(self, value: i64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E: Error>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E: Error>(self, _: f64) -> Result<Value, E> {
        if matches!(self, Self::Discard) {
            Ok(Value::Null)
        } else {
            Err(E::custom("number needs the frozen report reader"))
        }
    }

    fn visit_str<E: Error>(self, value: &str) -> Result<Value, E> {
        if matches!(self, Self::Discard) {
            Ok(Value::Null)
        } else {
            Ok(Value::String(value.to_owned()))
        }
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let element = match self {
            Self::Sources => Self::Source,
            Self::Discard => Self::Discard,
            _ => Self::All,
        };
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(element)? {
            if !matches!(element, Self::Discard) {
                values.push(value);
            }
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut fields = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let field = match (self, key.as_str()) {
                (Self::Daily, "sources") => Self::Sources,
                (
                    Self::Daily,
                    "schema_version" | "content" | "completed_work" | "work_summary" | "next_plan",
                )
                | (Self::Source, "source_id" | "relative_path") => Self::All,
                (Self::Daily | Self::Source | Self::Sources | Self::Discard, _) => Self::Discard,
                (Self::All, _) => Self::All,
            };
            if !matches!(field, Self::Discard) && key.starts_with('\0') {
                return Err(A::Error::custom("key needs the frozen report reader"));
            }
            let value = map.next_value_seed(field)?;
            if !matches!(field, Self::Discard) {
                fields.insert(key, value);
            }
        }
        Ok(Value::Object(fields))
    }
}
