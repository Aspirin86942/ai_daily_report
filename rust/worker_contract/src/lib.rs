//! Shared v2 envelope for crash-isolated scanner workers.
//!
//! Domain parse/classification payloads stay owned by their caller. This crate
//! only defines the small streaming interface shared by Rust and Python
//! workers: one hello frame followed by request/response NDJSON pairs.

use serde::{Deserialize, Serialize};

pub const CONTRACT: &str = "ai_daily_worker";
pub const CONTRACT_VERSION: &str = "ai_daily_worker_v2";
pub const PROTOCOL_VERSION: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerKind {
    Office,
    PythonDocument,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOperation {
    OfficeParse,
    PdfClassify,
    PdfParse,
    PythonOfficeParse,
    PythonSharepointParse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerHello {
    pub contract: String,
    pub protocol_version: u64,
    pub frame: String,
    pub worker_contract_version: String,
    pub worker_kind: WorkerKind,
    pub worker_version: String,
    pub worker_build: String,
    pub supported_operations: Vec<WorkerOperation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequest {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub operation: WorkerOperation,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerResponseStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerDiagnostic {
    pub error_code: String,
    pub message: String,
    pub retryable: bool,
    pub stage: String,
    pub file_path: Option<String>,
    pub backend: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerResponse {
    pub contract: String,
    pub protocol_version: u64,
    pub request_id: String,
    pub operation: WorkerOperation,
    pub status: WorkerResponseStatus,
    pub result: Option<serde_json::Value>,
    pub error: Option<WorkerDiagnostic>,
}

impl WorkerHello {
    pub fn validate(&self) -> Result<(), String> {
        validate_common(&self.contract, self.protocol_version)?;
        if self.frame != "hello" {
            return Err("worker hello frame must be hello".to_string());
        }
        if self.worker_contract_version != CONTRACT_VERSION {
            return Err("worker contract version mismatch".to_string());
        }
        if self.worker_version.is_empty() || self.worker_version.len() > 1024 {
            return Err("worker version is invalid".to_string());
        }
        if self.worker_build.is_empty() || self.worker_build.len() > 1024 {
            return Err("worker build identity is invalid".to_string());
        }
        if self.supported_operations.is_empty() {
            return Err("worker must support at least one operation".to_string());
        }
        let mut operations = self.supported_operations.clone();
        operations.sort_by_key(|operation| *operation as u8);
        operations.dedup();
        if operations.len() != self.supported_operations.len() {
            return Err("worker operations must be unique".to_string());
        }
        Ok(())
    }
}

impl WorkerRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_common(&self.contract, self.protocol_version)?;
        validate_request_id(&self.request_id)
    }
}

impl WorkerResponse {
    pub fn validate(&self) -> Result<(), String> {
        validate_common(&self.contract, self.protocol_version)?;
        validate_request_id(&self.request_id)?;
        match self.status {
            WorkerResponseStatus::Ok if self.result.is_some() && self.error.is_none() => Ok(()),
            WorkerResponseStatus::Error if self.result.is_none() && self.error.is_some() => Ok(()),
            _ => Err("worker response status/result/error mismatch".to_string()),
        }
    }
}

fn validate_common(contract: &str, protocol_version: u64) -> Result<(), String> {
    if contract != CONTRACT {
        return Err("worker contract mismatch".to_string());
    }
    if protocol_version != PROTOCOL_VERSION {
        return Err("worker protocol version mismatch".to_string());
    }
    Ok(())
}

fn validate_request_id(value: &str) -> Result<(), String> {
    let bytes = value.as_bytes();
    if bytes.len() != 36
        || ![8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| ![8, 13, 18, 23].contains(&index) && !byte.is_ascii_hexdigit())
    {
        return Err("request_id must be a canonical UUID".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_requires_v2_identity_and_unique_operations() {
        let hello = WorkerHello {
            contract: CONTRACT.to_string(),
            protocol_version: PROTOCOL_VERSION,
            frame: "hello".to_string(),
            worker_contract_version: CONTRACT_VERSION.to_string(),
            worker_kind: WorkerKind::Office,
            worker_version: "0.1.0".to_string(),
            worker_build: "a".repeat(64),
            supported_operations: vec![WorkerOperation::OfficeParse],
        };
        assert_eq!(hello.validate(), Ok(()));
    }

    #[test]
    fn response_is_a_strict_ok_or_error_union() {
        let response = WorkerResponse {
            contract: CONTRACT.to_string(),
            protocol_version: PROTOCOL_VERSION,
            request_id: "61111111-6111-4111-8111-611111111111".to_string(),
            operation: WorkerOperation::PdfParse,
            status: WorkerResponseStatus::Ok,
            result: Some(serde_json::json!({"content": "ok"})),
            error: None,
        };
        assert_eq!(response.validate(), Ok(()));
    }
}
