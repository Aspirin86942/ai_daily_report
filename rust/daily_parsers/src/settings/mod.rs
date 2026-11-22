//! Scanner configuration and shared scan metadata.

use serde::de;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

pub use crate::contract::SourceGuardKind;
mod pdf_audit;
pub use pdf_audit::{pdf_extraction_pages, pdf_sample_pages, PdfPageAudit, PdfPageObservation};
pub(crate) use pdf_audit::{validate_pdf_policy, validate_text_window};

pub const OCR_BACKEND: &str = "rust_pdfium_ocr_v1";
pub const OCR_RANGE_LABEL: &str = "offline_ocr_unverified";

pub trait Validate {
    fn validate(&self) -> Result<(), String>;
}

fn deserialize_optional_non_null<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| de::Error::custom("explicit null is not allowed"))
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

#[cfg(test)]
#[path = "../../tests/unit/settings.rs"]
mod tests;
