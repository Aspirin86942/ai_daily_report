//! Scanner settings, evidence metadata and frozen historical data formats.

use serde::de;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
mod pdf_audit;
pub use pdf_audit::{pdf_extraction_pages, pdf_sample_pages, PdfPageAudit, PdfPageObservation};
pub(crate) use pdf_audit::{validate_pdf_policy, validate_text_window};

pub const OCR_BACKEND: &str = "rust_pdfium_ocr_v1";
pub const OCR_RANGE_LABEL: &str = "offline_ocr_unverified";

pub trait Validate {
    fn validate(&self) -> Result<(), String>;
}

/// A required JSON field whose value may be null.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nullable<T>(pub Option<T>);

fn deserialize_optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| de::Error::custom("explicit null is not allowed"))
}

fn validate_context_header(contract: &str, version: u64, request_id: &str) -> Result<(), String> {
    require_const(contract, "ai_daily_context", "contract")?;
    require_range(version, 1, 1, "protocol_version")?;
    require_request_id(request_id)
}

fn require_const(actual: &str, expected: &str, field: &str) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{field} must equal {expected}"))
    }
}

fn require_non_empty(value: &str, max: usize, field: &str) -> Result<(), String> {
    let length = value.chars().count();
    if (1..=max).contains(&length) {
        Ok(())
    } else {
        Err(format!("{field} length must be 1..={max}"))
    }
}

pub(crate) fn require_range(value: u64, min: u64, max: u64, field: &str) -> Result<(), String> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(format!("{field} must be in {min}..={max}"))
    }
}

pub(crate) fn require_absolute_path(value: &str, field: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let drive_rooted = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let absolute = value.starts_with('/') || value.starts_with("\\\\") || drive_rooted;
    if absolute && !value.contains('\0') && value.chars().count() <= 32_767 {
        Ok(())
    } else {
        Err(format!("{field} must be an absolute path"))
    }
}

fn require_request_id(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let valid = bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
        && matches!(bytes[14], b'1'..=b'5')
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'A' | b'b' | b'B');
    if valid {
        Ok(())
    } else {
        Err("request_id must be an RFC 4122 UUID v1..v5".to_string())
    }
}

fn require_date(value: &str, field: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [4, 7].contains(&index) || byte.is_ascii_digit())
    {
        return Err(format!("{field} must use YYYY-MM-DD"));
    }
    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map(|_| ())
        .map_err(|_| format!("{field} must be a valid calendar date"))
}

fn require_extension(value: &str, field: &str) -> Result<(), String> {
    let length = value.chars().count();
    let valid = (2..=32).contains(&length)
        && value.starts_with('.')
        && value.chars().skip(1).all(|character| {
            !character.is_ascii_uppercase() && !matches!(character, '\\' | '/' | ':' | '\0')
        });
    if valid {
        Ok(())
    } else {
        Err(format!("{field} must be a lowercase extension"))
    }
}

pub(crate) fn require_source_version(value: &str, field: &str) -> Result<(), String> {
    let Some((mtime, size)) = value
        .strip_prefix("mtime_ns=")
        .and_then(|rest| rest.split_once(":size="))
    else {
        return Err(format!("invalid {field}"));
    };
    if !mtime.is_empty()
        && !size.is_empty()
        && mtime.bytes().all(|byte| byte.is_ascii_digit())
        && size.bytes().all(|byte| byte.is_ascii_digit())
    {
        Ok(())
    } else {
        Err(format!("invalid {field}"))
    }
}

fn require_relative_path(value: &str, field: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    let drive_prefixed = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let rooted = value.starts_with('/') || value.starts_with('\\') || drive_prefixed;
    let escapes = value.split(['/', '\\']).any(|component| component == "..");
    if !value.is_empty() && value.chars().count() <= 32_767 && !rooted && !escapes {
        Ok(())
    } else {
        Err(format!("{field} must be a safe relative path"))
    }
}

fn require_sorted_unique(values: &[String], field: &str) -> Result<(), String> {
    if values.windows(2).all(|pair| pair[0] < pair[1]) {
        Ok(())
    } else {
        Err(format!("{field} must be sorted and unique"))
    }
}

fn require_sha256_hex(value: &str, field: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(format!("{field} must be a lowercase SHA-256"))
    }
}

fn validate_values(
    values: &[String],
    field: &str,
    sorted: bool,
    validate: impl Fn(&str, &str) -> Result<(), String>,
) -> Result<(), String> {
    if values.len() > 256 {
        return Err(format!("{field} contains too many items"));
    }
    for value in values {
        validate(value, field)?;
    }
    if sorted {
        require_sorted_unique(values, field)?;
    }
    Ok(())
}

fn validate_label(value: &str, field: &str) -> Result<(), String> {
    require_non_empty(value, 1024, field)
}

fn validate_extension_timeouts(
    timeouts: &BTreeMap<String, u64>,
    field: &str,
    units_per_second: u64,
) -> Result<(), String> {
    if timeouts.len() > 256 {
        return Err(format!("{field} has too many entries"));
    }
    let key_field = format!("{field} key");
    let value_field = format!("{field} value");
    for (extension, timeout) in timeouts {
        require_extension(extension, &key_field)?;
        require_range(
            *timeout,
            units_per_second,
            3600 * units_per_second,
            &value_field,
        )?;
    }
    Ok(())
}

mod options;
pub use options::*;
mod profiles;
pub use profiles::*;
mod diagnostic;
pub use diagnostic::*;
mod context;
pub use context::*;
mod provenance;
pub use provenance::*;
mod envelope;
pub use envelope::*;

#[cfg(test)]
#[path = "../../tests/unit/settings.rs"]
mod tests;
