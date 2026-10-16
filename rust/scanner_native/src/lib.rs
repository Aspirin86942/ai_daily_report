use std::panic::{catch_unwind, AssertUnwindSafe};

use ai_daily_scanner_contract::{AdapterPaths, CompressionProfile, ReportMode, ScannerSettings};
use ai_daily_scanner_core::{
    ScanRequest, Scanner, ScannerConfig, ScannerError, ENGINE_BUILD_IDENTITY,
};
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyModule};
use pythonize::depythonize;
use serde::{Deserialize, Serialize};

create_exception!(ai_daily_scanner_native, NativeScannerError, PyException);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeConfig {
    work_dir: String,
    scan_db_path: String,
    scanner_settings: ScannerSettings,
    office_worker_path: String,
    pdf_worker_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeScanRequest {
    report_mode: ReportMode,
    start_date: String,
    end_date: String,
    #[serde(default)]
    window_start_utc: Option<String>,
    #[serde(default)]
    window_end_utc: Option<String>,
    #[serde(default)]
    compression_profile: Option<CompressionProfile>,
}

#[pyclass(name = "Scanner")]
struct PyScanner {
    inner: Scanner,
}

#[pymethods]
impl PyScanner {
    #[new]
    fn new(config: &Bound<'_, PyAny>) -> PyResult<Self> {
        guard_panic(|| {
            let config: NativeConfig = depythonize(config).map_err(|error| {
                PyValueError::new_err(format!("invalid scanner config: {error}"))
            })?;
            let inner = Scanner::open(ScannerConfig {
                work_dir: config.work_dir,
                scan_db_path: config.scan_db_path,
                scanner_settings: config.scanner_settings,
                adapters: AdapterPaths {
                    office_worker_path: config.office_worker_path,
                    pdf_worker_path: config.pdf_worker_path,
                },
            })
            .map_err(scanner_error)?;
            Ok(Self { inner })
        })?
    }

    fn build_context(&self, py: Python<'_>, request: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let request: NativeScanRequest = depythonize(request)
            .map_err(|error| PyValueError::new_err(format!("invalid scan request: {error}")))?;
        let request = ScanRequest {
            report_mode: request.report_mode,
            start_date: request.start_date,
            end_date: request.end_date,
            window_start_utc: request.window_start_utc,
            window_end_utc: request.window_end_utc,
            compression_profile: request.compression_profile,
        };
        let result = guard_panic(|| py.detach(|| self.inner.build_context(&request)))?
            .map_err(scanner_error)?;
        json_result(py, &result.value)
    }

    fn doctor(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let result = guard_panic(|| py.detach(|| self.inner.doctor()))?.map_err(scanner_error)?;
        json_result(py, &result.value)
    }
}

fn json_result(py: Python<'_>, value: &impl Serialize) -> PyResult<Py<PyAny>> {
    // serde_json arbitrary-precision numbers serialize through a private map in
    // generic Serde adapters. The actual JSON wire format preserves integer types.
    let serialized = serde_json::to_string(value)
        .map_err(|_| PyRuntimeError::new_err("result JSON conversion failed"))?;
    py.import("json")?
        .call_method1("loads", (serialized,))
        .map(Bound::unbind)
}

fn scanner_error(error: ScannerError) -> PyErr {
    let (error_code, retryable) = match &error {
        ScannerError::InvalidConfiguration(_) => ("INVALID_REQUEST".to_string(), false),
        ScannerError::Busy => ("SCANNER_BUSY".to_string(), true),
        ScannerError::Operation(_) => ("NATIVE_SCANNER_FAILED".to_string(), false),
        ScannerError::Initialization(store_error) => (
            serde_json::to_value(store_error.error_code())
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| "NATIVE_SCANNER_FAILED".to_string()),
            store_error.retryable(),
        ),
    };
    NativeScannerError::new_err((error_code, error.to_string(), retryable))
}

fn guard_panic<T>(operation: impl FnOnce() -> T) -> PyResult<T> {
    catch_unwind(AssertUnwindSafe(operation)).map_err(|_| {
        NativeScannerError::new_err((
            "NATIVE_SCANNER_PANIC",
            "native scanner aborted the current operation",
            false,
        ))
    })
}

fn verify_runtime(py: Python<'_>) -> PyResult<()> {
    let sys = py.import("sys")?;
    let version_info: (u8, u8, u8) = sys
        .getattr("version_info")?
        .extract::<(u8, u8, u8, &str, u8)>()
        .map(|value| (value.0, value.1, value.2))?;
    if version_info != (3, 13, 13) {
        return Err(PyRuntimeError::new_err(format!(
            "ai_daily_scanner_native requires CPython 3.13.13, found {}.{}.{}",
            version_info.0, version_info.1, version_info.2
        )));
    }
    Ok(())
}

#[pymodule]
fn ai_daily_scanner_native(py: Python<'_>, module: &Bound<'_, PyModule>) -> PyResult<()> {
    verify_runtime(py)?;
    module.add_class::<PyScanner>()?;
    module.add("NativeScannerError", py.get_type::<NativeScannerError>())?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("__build_identity__", ENGINE_BUILD_IDENTITY)?;
    Ok(())
}
