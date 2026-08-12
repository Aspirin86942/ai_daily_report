use std::panic::{catch_unwind, AssertUnwindSafe};

use ai_daily_scanner_contract::{AdapterPaths, CompressionProfile, ReportMode, ScannerProfile};
use ai_daily_scanner_core::{ScanRequest, Scanner, ScannerConfig, ScannerError};
use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyAny, PyModule};
use pythonize::{depythonize, pythonize};
use serde::Deserialize;

create_exception!(ai_daily_scanner_native, NativeScannerError, PyException);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeConfig {
    work_dir: String,
    scan_db_path: String,
    scanner_profile: ScannerProfile,
    office_worker_path: String,
    python_executable: String,
    python_module_root: String,
    python_document_worker_module: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeScanRequest {
    report_mode: ReportMode,
    start_date: String,
    end_date: String,
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
                scanner_profile: config.scanner_profile,
                adapters: AdapterPaths {
                    office_worker_path: config.office_worker_path,
                    python_executable: config.python_executable,
                    python_module_root: config.python_module_root,
                    python_document_worker_module: config.python_document_worker_module,
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
            compression_profile: request.compression_profile,
        };
        let result = guard_panic(|| py.detach(|| self.inner.build_context(&request)))?
            .map_err(scanner_error)?;
        pythonize(py, &result.value)
            .map(Bound::unbind)
            .map_err(|error| PyRuntimeError::new_err(format!("result conversion failed: {error}")))
    }

    fn doctor(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let result = guard_panic(|| py.detach(|| self.inner.doctor()))?.map_err(scanner_error)?;
        pythonize(py, &result.value)
            .map(Bound::unbind)
            .map_err(|error| PyRuntimeError::new_err(format!("result conversion failed: {error}")))
    }
}

fn scanner_error(error: ScannerError) -> PyErr {
    let (error_code, retryable) = match error {
        ScannerError::InvalidConfiguration(_) => ("INVALID_REQUEST", false),
        ScannerError::Busy => ("SCANNER_BUSY", true),
        ScannerError::Operation(_) => ("NATIVE_SCANNER_FAILED", false),
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
    Ok(())
}
