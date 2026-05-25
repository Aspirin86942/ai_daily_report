# Rust Discovery Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Rust CLI discovery backend behind `FileDiscoveryService.bootstrap_full_scan()` with Python fallback and benchmark visibility.

**Architecture:** Python remains the scanner owner. `FileDiscoveryService` prefers `rust` by default, converts Rust stdout JSON back into existing `DiscoveredFile` objects, and falls back to the current Python traversal if the Rust process or contract fails. `python` is still available as an explicit backend for baseline benchmark and troubleshooting. Rust is a small CLI under `rust/discovery/` that reads a JSON request from stdin, walks files without following directory symlinks, emits a stable JSON array, and never writes SQLite or parses file content.

**Tech Stack:** Python 3.10+, pytest, Dynaconf YAML config, Rust stable, Cargo, `serde`, `serde_json`, `chrono`, `walkdir`, `glob`.

---

## File Structure

- Modify `src/core/config.py`
  - Expose `scanner.discovery_backend`, `scanner.rust_discovery_bin`, and `scanner.discovery_timeout_seconds` with Rust-first fallback-safe defaults.
- Modify `src/services/scan_discovery.py`
  - Keep `DiscoveredFile` and current Python traversal.
  - Add `RustDiscoveryError` and `RustDiscoveryRunner` in the same module to avoid import cycles.
  - Route `bootstrap_full_scan()` through backend selection.
- Modify `scripts/benchmark_scanner.py`
  - Include `discovery_backend` in JSON and Markdown output so benchmark evidence proves which backend ran.
- Modify `config/settings.example.yaml`
  - Add default `scanner.discovery_backend: "rust"`, `scanner.rust_discovery_bin`, and `scanner.discovery_timeout_seconds`.
- Modify `README.md`
  - Add the minimal Rust discovery build and opt-in benchmark instructions.
- Modify `.gitignore`
  - Ignore Cargo `target/` directories.
- Create `rust/discovery/Cargo.toml`
  - Define the Rust CLI package `ai-daily-discovery`.
- Create `rust/discovery/src/lib.rs`
  - Implement request parsing types, discovery traversal, filters, metadata conversion, and Rust unit tests.
- Create `rust/discovery/src/main.rs`
  - Read stdin JSON, call library discovery, print stdout JSON, print warnings/errors to stderr.
- Create after build `rust/discovery/Cargo.lock`
  - Track the lockfile because this is an application CLI.
- Modify `tests/test_config.py`
  - Lock config defaults and picklability for new discovery keys.
- Modify `tests/test_scan_discovery.py`
  - Lock backend selection, Rust success conversion, fallback, and fixture behavior.
- Modify `tests/test_benchmark_scanner.py`
  - Lock benchmark JSON/Markdown backend visibility.
- Create `tests/test_rust_discovery_contract.py`
  - Compare real Rust CLI output with Python discovery on a fixture when the release binary exists.

## Task 1: Lock Python Config Defaults And Benchmark Metadata

**Files:**
- Modify: `tests/test_config.py`
- Modify: `tests/test_benchmark_scanner.py`
- Modify: `scripts/benchmark_scanner.py`
- Modify: `src/core/config.py`
- Modify: `config/settings.example.yaml`

- [ ] **Step 1: Write failing config tests**

Append this test to `tests/test_config.py`:

```python
def test_scanner_config_exposes_discovery_backend_defaults_when_keys_absent():
    """配置缺省时应优先走 Rust；Rust 失败时由 discovery 层 fallback。"""
    cfg = object.__new__(Config)
    cfg._settings = SimpleNamespace(
        scanner=SimpleNamespace(
            allowed_extensions=[".txt"],
            ignored_patterns=[],
            max_workers=1,
            excel_max_rows=50,
            pdf_max_pages=5,
            text_max_chars=6000,
        )
    )

    scanner_config = cfg.scanner_config

    assert scanner_config["discovery_backend"] == "rust"
    assert scanner_config["rust_discovery_bin"] == (
        "rust/discovery/target/release/ai-daily-discovery"
    )
    assert scanner_config["discovery_timeout_seconds"] == 30
```

Extend `test_scanner_config_uses_builtin_containers_and_is_picklable()` by adding these YAML lines under `scanner:`:

```yaml
  discovery_backend: rust
  rust_discovery_bin: rust/discovery/target/release/ai-daily-discovery
  discovery_timeout_seconds: 12
```

Then add these assertions before `pickle.dumps(scanner_config)`:

```python
    assert scanner_config["discovery_backend"] == "rust"
    assert scanner_config["rust_discovery_bin"] == (
        "rust/discovery/target/release/ai-daily-discovery"
    )
    assert scanner_config["discovery_timeout_seconds"] == 12
```

- [ ] **Step 2: Run config tests and verify they fail**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_config.py -q
```

Expected: FAIL with a `KeyError` or missing assertion for `discovery_backend`.

- [ ] **Step 3: Implement config defaults**

In `src/core/config.py`, add these keys inside the initial `cfg` dict in `scanner_config` after `worker_lane_mode`:

```python
            "discovery_backend": str(
                getattr(self._settings.scanner, "discovery_backend", "rust")
            ).strip().lower(),
            "rust_discovery_bin": getattr(
                self._settings.scanner,
                "rust_discovery_bin",
                "rust/discovery/target/release/ai-daily-discovery",
            ),
            "discovery_timeout_seconds": getattr(
                self._settings.scanner,
                "discovery_timeout_seconds",
                30,
            ),
```

- [ ] **Step 4: Add example YAML keys**

In `config/settings.example.yaml`, add these under `scanner:` near `excluded_dirs`:

```yaml
  # 默认优先 Rust；二进制缺失或失败时会回退 Python。
  discovery_backend: "rust"
  rust_discovery_bin: "rust/discovery/target/release/ai-daily-discovery"
  discovery_timeout_seconds: 30
```

- [ ] **Step 5: Run config tests and verify they pass**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_config.py -q
```

Expected: PASS.

- [ ] **Step 6: Write failing benchmark metadata tests**

In `tests/test_benchmark_scanner.py`, update the call to `build_benchmark_payload()` in `test_build_benchmark_payload_uses_scan_result_and_metrics()`:

```python
    payload = build_benchmark_payload(
        scan_result=scan_result,
        run_detail=run_detail,
        extension_metrics=extension_metrics,
        reparse_details=reparse_details,
        start_date=date(2026, 5, 23),
        end_date=date(2026, 5, 24),
        summary_mode=True,
        discovery_backend="rust",
    )
```

Update the expected parameters:

```python
    assert payload["parameters"] == {
        "start_date": "2026-05-23",
        "end_date": "2026-05-24",
        "summary_mode": True,
        "discovery_backend": "rust",
    }
```

Append this Markdown assertion to `test_render_markdown_report_renders_stage_counts_and_backend_summary()`:

```python
    assert "- discovery_backend: `rust`" in markdown
```

If that test builds the payload inline, add `"discovery_backend": "rust"` inside `payload["parameters"]`.

- [ ] **Step 7: Run benchmark tests and verify they fail**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_benchmark_scanner.py -q
```

Expected: FAIL because `build_benchmark_payload()` does not accept or render `discovery_backend` yet.

- [ ] **Step 8: Implement benchmark metadata**

In `scripts/benchmark_scanner.py`, change `build_benchmark_payload()` signature to:

```python
def build_benchmark_payload(
    scan_result: ScanResult,
    run_detail: dict[str, int],
    extension_metrics: list[ExtensionMetrics],
    reparse_details: list[ReparseDetail],
    start_date: date,
    end_date: date,
    summary_mode: bool,
    discovery_backend: str,
) -> dict[str, Any]:
```

Add `discovery_backend` to the returned `parameters` dict:

```python
        "parameters": {
            "start_date": start_date.isoformat(),
            "end_date": end_date.isoformat(),
            "summary_mode": summary_mode,
            "discovery_backend": discovery_backend,
        },
```

In `render_markdown_report()`, add this line after the summary mode line:

```python
        f"- discovery_backend: `{parameters.get('discovery_backend', 'rust')}`",
```

In `run_benchmark()`, pass the active scanner config:

```python
        discovery_backend=str(scanner.scanner_cfg.get("discovery_backend", "rust")),
```

- [ ] **Step 9: Run benchmark tests and verify they pass**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_benchmark_scanner.py -q
```

Expected: PASS.

- [ ] **Step 10: Commit config and benchmark metadata**

Run:

```bash
git add src/core/config.py config/settings.example.yaml scripts/benchmark_scanner.py tests/test_config.py tests/test_benchmark_scanner.py
git commit -m "Add discovery backend config metadata"
```

## Task 2: Add Python Rust Runner Selection And Fallback

**Files:**
- Modify: `tests/test_scan_discovery.py`
- Modify: `src/services/scan_discovery.py`

- [ ] **Step 1: Write failing Rust success test**

Add these imports to `tests/test_scan_discovery.py`:

```python
import json
from datetime import datetime
from types import SimpleNamespace
```

In the three existing Python discovery tests in `tests/test_scan_discovery.py`, add this key to each `scanner_cfg` dict:

```python
            "discovery_backend": "python",
```

Those tests are intentionally about the legacy Python traversal rules; setting the backend explicitly keeps them from exercising the Rust fallback path by accident.

Append this test:

```python
def test_bootstrap_full_scan_uses_rust_backend_when_configured(
    tmp_path: Path,
    monkeypatch,
):
    """Rust backend 成功时，应把 stdout JSON 转成现有 DiscoveredFile 契约。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    sample = work_dir / "report.md"
    sample.write_text("hello", encoding="utf-8")
    stat_result = sample.stat()
    payload = [
        {
            "file_identity": f"bootstrap:{str(sample.resolve()).lower()}",
            "path": str(sample.resolve()),
            "extension": ".md",
            "modified_at": datetime.fromtimestamp(stat_result.st_mtime).isoformat(),
            "size_bytes": stat_result.st_size,
            "source_version": (
                f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
            ),
        }
    ]
    calls = []

    def fake_run(*args, **kwargs):
        calls.append((args, kwargs))
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps(payload),
            stderr="",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "rust",
            "rust_discovery_bin": "target/release/ai-daily-discovery",
            "discovery_timeout_seconds": 5,
        },
    )

    [item] = discovery.bootstrap_full_scan(date.today(), date.today())

    assert calls
    request = json.loads(calls[0][1]["input"])
    assert request["work_dir"] == str(work_dir)
    assert request["start_date"] == date.today().isoformat()
    assert request["end_date"] == date.today().isoformat()
    assert request["allowed_extensions"] == [".md"]
    assert request["ignored_patterns"] == []
    assert request["excluded_dirs"] == []
    assert item.path == sample.resolve()
    assert item.extension == ".md"
    assert item.source_version == (
        f"mtime_ns={stat_result.st_mtime_ns}:size={stat_result.st_size}"
    )
```

- [ ] **Step 2: Write failing fallback test**

Append this test:

```python
def test_bootstrap_full_scan_defaults_to_rust_backend(
    tmp_path: Path,
    monkeypatch,
):
    """未显式配置 backend 时，discovery 服务也应优先尝试 Rust。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    payload = []
    calls = []

    def fake_run(*args, **kwargs):
        calls.append((args, kwargs))
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps(payload),
            stderr="",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "rust_discovery_bin": "target/release/ai-daily-discovery",
            "discovery_timeout_seconds": 5,
        },
    )

    files = discovery.bootstrap_full_scan(date.today(), date.today())

    assert files == []
    assert calls
```

- [ ] **Step 3: Write failing fallback test**

Append this test:

```python
def test_bootstrap_full_scan_falls_back_to_python_when_rust_fails(
    tmp_path: Path,
    monkeypatch,
):
    """Rust 进程失败不能中断扫描；fallback 应保持现有 Python discovery 行为。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    sample = work_dir / "fallback.md"
    sample.write_text("fallback", encoding="utf-8")

    def fake_run(*args, **kwargs):
        return SimpleNamespace(
            returncode=2,
            stdout="",
            stderr="boom",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "rust",
            "rust_discovery_bin": "target/release/ai-daily-discovery",
            "discovery_timeout_seconds": 5,
        },
    )

    [item] = discovery.bootstrap_full_scan(date.today(), date.today())

    assert item.path == sample
    assert item.file_identity == f"bootstrap:{str(sample.resolve()).lower()}"
```

- [ ] **Step 4: Write failing invalid-contract fallback test**

Append this test:

```python
def test_bootstrap_full_scan_falls_back_when_rust_json_contract_is_invalid(
    tmp_path: Path,
    monkeypatch,
):
    """Rust stdout 合约错误要在 Python 边界拦住，避免坏数据进入 inventory。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    sample = work_dir / "fallback.md"
    sample.write_text("fallback", encoding="utf-8")

    def fake_run(*args, **kwargs):
        return SimpleNamespace(
            returncode=0,
            stdout=json.dumps([{"path": str(sample)}]),
            stderr="",
        )

    monkeypatch.setattr("src.services.scan_discovery.subprocess.run", fake_run)

    discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            "allowed_extensions": [".md"],
            "ignored_patterns": [],
            "excluded_dirs": [],
            "discovery_backend": "rust",
            "rust_discovery_bin": "target/release/ai-daily-discovery",
            "discovery_timeout_seconds": 5,
        },
    )

    [item] = discovery.bootstrap_full_scan(date.today(), date.today())

    assert item.path == sample
```

- [ ] **Step 5: Run discovery tests and verify they fail**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_scan_discovery.py -q
```

Expected: FAIL because `src.services.scan_discovery.subprocess` and Rust runner do not exist yet.

- [ ] **Step 6: Implement Python runner and backend selection**

In `src/services/scan_discovery.py`, add imports:

```python
import json
import subprocess
```

Add this error type after `DiscoveredFile`:

```python
class RustDiscoveryError(RuntimeError):
    """Rust discovery backend failed before producing a trusted contract."""
```

Add this runner before `FileDiscoveryService`:

```python
class RustDiscoveryRunner:
    """通过 Rust CLI 执行文件发现，并校验 stdout JSON 契约。"""

    def __init__(self, scanner_cfg: dict):
        self.scanner_cfg = scanner_cfg

    def discover(
        self,
        work_dir: Path,
        start_date: date,
        end_date: date,
    ) -> list[DiscoveredFile]:
        request = {
            "work_dir": str(work_dir),
            "start_date": start_date.isoformat(),
            "end_date": end_date.isoformat(),
            "allowed_extensions": self.scanner_cfg["allowed_extensions"],
            "ignored_patterns": self.scanner_cfg["ignored_patterns"],
            "excluded_dirs": self.scanner_cfg.get("excluded_dirs", []),
        }
        completed = subprocess.run(
            [str(self._resolve_binary_path())],
            input=json.dumps(request, ensure_ascii=False),
            text=True,
            capture_output=True,
            timeout=float(self.scanner_cfg.get("discovery_timeout_seconds", 30)),
            check=False,
        )
        if completed.returncode != 0:
            message = completed.stderr.strip() or f"exit code {completed.returncode}"
            raise RustDiscoveryError(message)
        try:
            raw_items = json.loads(completed.stdout)
        except json.JSONDecodeError as exc:
            raise RustDiscoveryError(f"invalid JSON stdout: {exc}") from exc
        if not isinstance(raw_items, list):
            raise RustDiscoveryError("stdout JSON must be a list")
        return [self._to_discovered_file(item) for item in raw_items]

    def _resolve_binary_path(self) -> Path:
        configured = Path(
            str(
                self.scanner_cfg.get(
                    "rust_discovery_bin",
                    "rust/discovery/target/release/ai-daily-discovery",
                )
            )
        )
        if configured.is_absolute():
            return configured
        project_root = Path(__file__).resolve().parent.parent.parent
        return project_root / configured

    def _to_discovered_file(self, item: object) -> DiscoveredFile:
        if not isinstance(item, dict):
            raise RustDiscoveryError("discovered file item must be an object")
        try:
            return DiscoveredFile(
                file_identity=str(item["file_identity"]),
                path=Path(str(item["path"])),
                extension=str(item["extension"]).lower(),
                modified_at=datetime.fromisoformat(str(item["modified_at"])),
                size_bytes=int(item["size_bytes"]),
                source_version=str(item["source_version"]),
            )
        except (KeyError, TypeError, ValueError) as exc:
            raise RustDiscoveryError(f"invalid discovered file item: {item}") from exc
```

Change `bootstrap_full_scan()` to:

```python
    def bootstrap_full_scan(
        self,
        start_date: date,
        end_date: date,
    ) -> List[DiscoveredFile]:
        """执行一次完整文件发现，并返回可落库存的文件元数据。"""
        backend = str(self.scanner_cfg.get("discovery_backend", "rust")).lower()
        if backend == "rust":
            try:
                return RustDiscoveryRunner(self.scanner_cfg).discover(
                    work_dir=self.work_dir,
                    start_date=start_date,
                    end_date=end_date,
                )
            except (OSError, subprocess.SubprocessError, RustDiscoveryError) as exc:
                logger.warning("Rust discovery 失败，回退 Python discovery: %s", exc)
        return self._bootstrap_full_scan_python(start_date, end_date)
```

Move the existing body of `bootstrap_full_scan()` into a new private method:

```python
    def _bootstrap_full_scan_python(
        self,
        start_date: date,
        end_date: date,
    ) -> list[DiscoveredFile]:
        """保留现有 Python discovery 作为默认实现和 Rust fallback。"""
        start_dt = datetime.combine(start_date, datetime.min.time())
        end_dt = datetime.combine(end_date, datetime.max.time())

        files: list[DiscoveredFile] = []
        excluded_dirs = self.scanner_cfg.get("excluded_dirs", [])
        excluded_paths = [Path(directory).resolve() for directory in excluded_dirs]

        for root, _, filenames in os.walk(self.work_dir):
            root_path = Path(root).resolve()

            if self._is_excluded_dir(root_path, excluded_paths):
                continue

            for filename in filenames:
                filename_lower = filename.lower()
                if not self._is_allowed_extension(filename_lower):
                    continue
                if self._matches_ignored_pattern(filename_lower):
                    continue

                file_path = Path(root) / filename
                try:
                    stat_result = file_path.stat()
                    mtime = datetime.fromtimestamp(stat_result.st_mtime)
                    if start_dt <= mtime <= end_dt:
                        resolved_path = file_path.resolve()
                        files.append(
                            DiscoveredFile(
                                file_identity=(
                                    f"bootstrap:{str(resolved_path).lower()}"
                                ),
                                path=file_path,
                                extension=file_path.suffix.lower(),
                                modified_at=mtime,
                                size_bytes=stat_result.st_size,
                                source_version=(
                                    f"mtime_ns={stat_result.st_mtime_ns}:"
                                    f"size={stat_result.st_size}"
                                ),
                            )
                        )
                except Exception as exc:
                    logger.warning("无法读取文件时间 %s: %s", file_path, exc)

        return files
```

- [ ] **Step 7: Run discovery tests and verify they pass**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_scan_discovery.py -q
```

Expected: PASS.

- [ ] **Step 8: Commit Python runner**

Run:

```bash
git add src/services/scan_discovery.py tests/test_scan_discovery.py
git commit -m "Add Python fallback for Rust discovery backend"
```

## Task 3: Add Rust CLI With Unit Tests

**Files:**
- Modify: `.gitignore`
- Create: `rust/discovery/Cargo.toml`
- Create: `rust/discovery/src/lib.rs`
- Create: `rust/discovery/src/main.rs`
- Create after build: `rust/discovery/Cargo.lock`

- [ ] **Step 1: Ignore Cargo build outputs**

Append to `.gitignore`:

```gitignore

# Rust
target/
```

- [ ] **Step 2: Create Cargo package metadata**

Create `rust/discovery/Cargo.toml`:

```toml
[package]
name = "ai-daily-discovery"
version = "0.1.0"
edition = "2021"

[dependencies]
chrono = { version = "0.4", features = ["serde"] }
glob = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
walkdir = "2"
```

- [ ] **Step 3: Write failing Rust unit tests**

Create `rust/discovery/src/lib.rs` with request/output structs and tests first:

```rust
use chrono::{Local, NaiveDate, TimeZone};
use glob::Pattern;
use serde::{Deserialize, Serialize};
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::{DirEntry, WalkDir};

#[derive(Debug, Deserialize)]
pub struct DiscoveryRequest {
    pub work_dir: PathBuf,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub allowed_extensions: Vec<String>,
    pub ignored_patterns: Vec<String>,
    pub excluded_dirs: Vec<PathBuf>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DiscoveredFileOut {
    pub file_identity: String,
    pub path: String,
    pub extension: String,
    pub modified_at: String,
    pub size_bytes: u64,
    pub source_version: String,
}

pub fn discover_files(_request: &DiscoveryRequest) -> io::Result<Vec<DiscoveredFileOut>> {
    unimplemented!("implemented after tests fail");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_extension_is_case_insensitive() {
        assert!(has_allowed_extension("REPORT.MD", &[".md".to_string()]));
        assert!(!has_allowed_extension("REPORT.tmp", &[".md".to_string()]));
    }

    #[test]
    fn ignored_patterns_match_file_name_only() {
        let patterns = compile_patterns(&["~$*".to_string(), "*.tmp".to_string()]).unwrap();

        assert!(matches_ignored_pattern("~$draft.md", &patterns));
        assert!(matches_ignored_pattern("scratch.tmp", &patterns));
        assert!(!matches_ignored_pattern("report.md", &patterns));
    }

    #[test]
    fn excluded_dir_matches_directory_and_children() {
        let root = PathBuf::from("/work/skip");

        assert!(is_excluded_dir(Path::new("/work/skip"), &[root.clone()]));
        assert!(is_excluded_dir(Path::new("/work/skip/nested"), &[root]));
        assert!(!is_excluded_dir(Path::new("/work/keep"), &[PathBuf::from("/work/skip")]));
    }

    #[test]
    fn source_version_uses_mtime_ns_and_size() {
        assert_eq!(build_source_version(123, 456), "mtime_ns=123:size=456");
    }
}
```

- [ ] **Step 4: Run Rust tests and verify they fail**

Run:

```bash
cd rust/discovery && cargo test
```

Expected: FAIL to compile because helper functions are not implemented.

- [ ] **Step 5: Implement Rust discovery library**

Replace the `discover_files()` stub and add helper functions in `rust/discovery/src/lib.rs`:

```rust
pub fn discover_files(request: &DiscoveryRequest) -> io::Result<Vec<DiscoveredFileOut>> {
    let start_dt = request.start_date.and_hms_opt(0, 0, 0).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid start date boundary")
    })?;
    let end_dt = request
        .end_date
        .and_hms_micro_opt(23, 59, 59, 999_999)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid end date boundary"))?;
    let ignored_patterns = compile_patterns(&request.ignored_patterns)?;
    let excluded_dirs = resolve_excluded_dirs(&request.excluded_dirs);
    let mut files = Vec::new();

    let walker = WalkDir::new(&request.work_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() {
                return true;
            }
            let entry_path = entry.path();
            !is_excluded_dir(entry_path, &excluded_dirs)
        });

    for entry_result in walker {
        let entry = match entry_result {
            Ok(value) => value,
            Err(error) => {
                eprintln!("warning: cannot walk entry: {error}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        let file_name_lower = file_name.to_lowercase();
        if !has_allowed_extension(&file_name_lower, &request.allowed_extensions) {
            continue;
        }
        if matches_ignored_pattern(&file_name_lower, &ignored_patterns) {
            continue;
        }

        let metadata = match fs::metadata(entry.path()) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("warning: cannot stat {}: {}", entry.path().display(), error);
                continue;
            }
        };
        let modified_local = metadata_modified_local(&metadata)?;
        let modified_naive = modified_local.naive_local();
        if modified_naive < start_dt || modified_naive > end_dt {
            continue;
        }

        let resolved_path = match fs::canonicalize(entry.path()) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("warning: cannot canonicalize {}: {}", entry.path().display(), error);
                continue;
            }
        };
        let size_bytes = metadata.len();
        let mtime_ns = metadata_mtime_ns(&metadata)?;
        files.push(DiscoveredFileOut {
            file_identity: format!(
                "bootstrap:{}",
                resolved_path.to_string_lossy().to_lowercase()
            ),
            path: resolved_path.to_string_lossy().to_string(),
            extension: lower_extension(entry.path()),
            modified_at: modified_naive.format("%Y-%m-%dT%H:%M:%S%.6f").to_string(),
            size_bytes,
            source_version: build_source_version(mtime_ns, size_bytes),
        });
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn resolve_excluded_dirs(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| fs::canonicalize(path).unwrap_or_else(|_| path.clone()))
        .collect()
}

fn has_allowed_extension(file_name_lower: &str, allowed_extensions: &[String]) -> bool {
    allowed_extensions
        .iter()
        .any(|extension| file_name_lower.ends_with(&extension.to_lowercase()))
}

fn compile_patterns(patterns: &[String]) -> io::Result<Vec<Pattern>> {
    patterns
        .iter()
        .map(|pattern| {
            Pattern::new(&pattern.to_lowercase()).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid ignored pattern {pattern}: {error}"),
                )
            })
        })
        .collect()
}

fn matches_ignored_pattern(file_name_lower: &str, patterns: &[Pattern]) -> bool {
    patterns.iter().any(|pattern| pattern.matches(file_name_lower))
}

fn is_excluded_dir(path: &Path, excluded_dirs: &[PathBuf]) -> bool {
    let comparable = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    excluded_dirs
        .iter()
        .any(|excluded| comparable == *excluded || comparable.starts_with(excluded))
}

fn lower_extension(path: &Path) -> String {
    path.extension()
        .map(|value| format!(".{}", value.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

fn metadata_modified_local(metadata: &Metadata) -> io::Result<chrono::DateTime<Local>> {
    let modified = metadata.modified()?;
    let duration = modified.duration_since(UNIX_EPOCH).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("modified time is before unix epoch: {error}"),
        )
    })?;
    Local
        .timestamp_opt(duration.as_secs() as i64, duration.subsec_nanos())
        .single()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ambiguous local timestamp"))
}

#[cfg(unix)]
fn metadata_mtime_ns(metadata: &Metadata) -> io::Result<u128> {
    use std::os::unix::fs::MetadataExt;

    Ok((metadata.mtime() as u128) * 1_000_000_000 + metadata.mtime_nsec() as u128)
}

#[cfg(not(unix))]
fn metadata_mtime_ns(metadata: &Metadata) -> io::Result<u128> {
    let modified = metadata.modified()?;
    let duration = modified.duration_since(UNIX_EPOCH).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("modified time is before unix epoch: {error}"),
        )
    })?;
    Ok(duration.as_nanos())
}

fn build_source_version(mtime_ns: u128, size_bytes: u64) -> String {
    format!("mtime_ns={mtime_ns}:size={size_bytes}")
}
```

Remove unused imports if `cargo test` reports any warning that is promoted by local settings.

- [ ] **Step 6: Create Rust CLI main**

Create `rust/discovery/src/main.rs`:

```rust
use ai_daily_discovery::{discover_files, DiscoveryRequest};
use std::io::{self, Read};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let request: DiscoveryRequest = serde_json::from_str(&input)?;
    let files = discover_files(&request)?;
    println!("{}", serde_json::to_string(&files)?);
    Ok(())
}
```

- [ ] **Step 7: Run Rust tests and build release binary**

Run:

```bash
cd rust/discovery && cargo test
cd rust/discovery && cargo build --release
```

Expected: both commands PASS, and `rust/discovery/target/release/ai-daily-discovery` exists.

- [ ] **Step 8: Commit Rust CLI**

Run:

```bash
git add .gitignore rust/discovery/Cargo.toml rust/discovery/Cargo.lock rust/discovery/src/lib.rs rust/discovery/src/main.rs
git commit -m "Add Rust discovery CLI"
```

## Task 4: Add Real Rust/Python Contract Test

**Files:**
- Create: `tests/test_rust_discovery_contract.py`

- [ ] **Step 1: Write contract test**

Create `tests/test_rust_discovery_contract.py`:

```python
"""Rust discovery CLI 与 Python discovery 的输出契约测试。"""

from __future__ import annotations

from datetime import date
from pathlib import Path

import pytest

from src.services.scan_discovery import FileDiscoveryService


RUST_DISCOVERY_BIN = (
    Path(__file__).resolve().parents[1]
    / "rust/discovery/target/release/ai-daily-discovery"
)


@pytest.mark.skipif(
    not RUST_DISCOVERY_BIN.exists(),
    reason="Rust discovery release binary has not been built",
)
def test_rust_discovery_matches_python_backend_for_fixture(tmp_path: Path):
    """同一组 fixture 下 Rust 和 Python 应发现同一批文件并保持版本指纹一致。"""
    work_dir = tmp_path / "work"
    work_dir.mkdir()
    included_dir = work_dir / "included"
    included_dir.mkdir()
    excluded_dir = work_dir / "excluded"
    excluded_dir.mkdir()

    keep_md = included_dir / "keep.MD"
    keep_txt = included_dir / "note.txt"
    keep_md.write_text("keep", encoding="utf-8")
    keep_txt.write_text("note", encoding="utf-8")
    (included_dir / "~$draft.md").write_text("ignore", encoding="utf-8")
    (included_dir / "scratch.tmp").write_text("ignore", encoding="utf-8")
    (excluded_dir / "blocked.md").write_text("blocked", encoding="utf-8")

    base_cfg = {
        "allowed_extensions": [".md", ".txt", ".tmp"],
        "ignored_patterns": ["~$*", "*.tmp"],
        "excluded_dirs": [str(excluded_dir)],
    }
    python_discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={**base_cfg, "discovery_backend": "python"},
    )
    rust_discovery = FileDiscoveryService(
        work_dir=work_dir,
        scanner_cfg={
            **base_cfg,
            "discovery_backend": "rust",
            "rust_discovery_bin": str(RUST_DISCOVERY_BIN),
            "discovery_timeout_seconds": 10,
        },
    )

    python_items = python_discovery.bootstrap_full_scan(date.today(), date.today())
    rust_items = rust_discovery.bootstrap_full_scan(date.today(), date.today())

    def comparable(items):
        return sorted(
            (
                item.file_identity,
                item.path.resolve(),
                item.extension,
                item.size_bytes,
                item.source_version,
            )
            for item in items
        )

    assert comparable(rust_items) == comparable(python_items)
```

- [ ] **Step 2: Run contract test after Rust build**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_rust_discovery_contract.py -q
```

Expected: PASS when the release binary exists. If it skips, run `cd rust/discovery && cargo build --release` first, then rerun.

- [ ] **Step 3: Commit contract test**

Run:

```bash
git add tests/test_rust_discovery_contract.py
git commit -m "Add Rust discovery contract test"
```

## Task 5: Document Rust Discovery Opt-In

**Files:**
- Modify: `README.md`
- Modify: `AGENTS.md` only if local repo guidance needs a Rust command reminder

- [ ] **Step 1: Update README configuration section**

In `README.md`, add this subsection under the scanner/config area:

````markdown
### Rust Discovery Backend

默认优先使用 Rust discovery；如果二进制未构建、路径配置错误或 stdout 合约失败，会记录 warning 并回退到 Python discovery：

```yaml
scanner:
  discovery_backend: "rust"
  rust_discovery_bin: "rust/discovery/target/release/ai-daily-discovery"
```

本机要测试 Rust discovery 时，先构建 CLI：

```bash
cd rust/discovery
cargo build --release
```

然后只修改本机配置 `config/settings.linux.yaml`：

```yaml
scanner:
  discovery_backend: "rust"
```

需要跑 Python baseline benchmark 时，把本机 `config/settings.linux.yaml` 临时改成：

```yaml
scanner:
  discovery_backend: "python"
```

benchmark 报告中的 `discovery_backend` 字段用于确认本轮配置；如果看到 Rust fallback warning，说明配置是 Rust，但实际 discovery 已降级到 Python。
````

- [ ] **Step 2: Run Markdown/literal sanity checks**

Run:

```bash
rg -n "discovery_backend|cargo build --release|ai-daily-discovery" README.md config/settings.example.yaml AGENTS.md
```

Expected: README and example config mention the new keys; AGENTS only changes if needed.

- [ ] **Step 3: Commit docs**

Run:

```bash
git add README.md AGENTS.md config/settings.example.yaml
git commit -m "Document Rust discovery backend"
```

## Task 6: Full Verification And Benchmark Evidence

**Files:**
- No source edits expected.
- Generated benchmark files under `data/benchmarks/` are runtime evidence and normally ignored by `data/`.

- [ ] **Step 1: Run focused Python tests**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/test_config.py tests/test_scan_discovery.py tests/test_benchmark_scanner.py tests/test_rust_discovery_contract.py -q
```

Expected: PASS.

- [ ] **Step 2: Run Rust tests and release build**

Run:

```bash
cd rust/discovery && cargo test
cd rust/discovery && cargo build --release
```

Expected: PASS and release binary exists.

- [ ] **Step 3: Run full Python test suite**

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python -m pytest tests/ -q
```

Expected: PASS.

- [ ] **Step 4: Run whitespace check**

Run:

```bash
git diff --check
```

Expected: no output and exit code 0.

- [ ] **Step 5: Run Python backend benchmark baseline**

Ensure `config/settings.linux.yaml` has:

```yaml
scanner:
  discovery_backend: "python"
```

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python scripts/benchmark_scanner.py \
  --start-date 2026-05-24 \
  --end-date 2026-05-25 \
  --json-out data/benchmarks/scanner-python-2026-05-24_2026-05-25.json \
  --markdown-out data/benchmarks/scanner-python-2026-05-24_2026-05-25.md
```

Expected: stdout JSON has `"discovery_backend": "python"` under `parameters`.

- [ ] **Step 6: Run Rust backend benchmark**

Ensure `config/settings.linux.yaml` has:

```yaml
scanner:
  discovery_backend: "rust"
```

Run:

```bash
/home/george/miniconda3/bin/conda run -n test python scripts/benchmark_scanner.py \
  --start-date 2026-05-24 \
  --end-date 2026-05-25 \
  --json-out data/benchmarks/scanner-rust-2026-05-24_2026-05-25.json \
  --markdown-out data/benchmarks/scanner-rust-2026-05-24_2026-05-25.md
```

Expected: stdout JSON has `"discovery_backend": "rust"` under `parameters`, `metrics.discovered_count` is not lower than the Python baseline, and `metrics.discovery_duration_ms` is lower than the Python baseline on the same date range.

- [ ] **Step 7: Restore local config if needed**

If this task changed `config/settings.linux.yaml`, restore the user's preferred backend after collecting evidence. The file is ignored and must not be committed.

- [ ] **Step 8: Review final git state**

Run:

```bash
git status --short
git log --oneline -8
```

Expected: source changes are committed in small commits or staged intentionally; ignored local YAML and generated `data/` files do not appear.

## Self-Review

- Spec coverage:
  - Backend selection and Python fallback: Task 2.
  - Rust CLI stdin/stdout JSON contract: Task 3.
  - Fixture consistency between Python and Rust: Task 4.
  - Config defaults and opt-in local YAML behavior: Task 1 and Task 5.
  - Benchmark backend visibility and performance evidence: Task 1 and Task 6.
  - Non-goals are preserved: no parser, cache, aggregator, SQLite schema, or ContextScheduler changes are planned.
- Placeholder scan:
  - No placeholder phrases from the planning checklist remain.
- Type consistency:
  - Python config keys are `discovery_backend`, `rust_discovery_bin`, and `discovery_timeout_seconds`.
  - Rust request fields match the approved spec: `work_dir`, `start_date`, `end_date`, `allowed_extensions`, `ignored_patterns`, `excluded_dirs`.
  - Rust output fields match `DiscoveredFile`: `file_identity`, `path`, `extension`, `modified_at`, `size_bytes`, `source_version`.

## 伪代码草案

```python
# Python 主边界：scanner 仍只依赖 FileDiscoveryService.bootstrap_full_scan()
def bootstrap_full_scan(start_date: date, end_date: date) -> list[DiscoveredFile]:
    backend = scanner_cfg.get("discovery_backend", "rust")
    if backend == "rust":
        try:
            return RustDiscoveryRunner(scanner_cfg).discover(work_dir, start_date, end_date)
        except RustDiscoveryError as exc:
            logger.warning("Rust discovery 失败，回退 Python discovery: %s", exc)
    return _bootstrap_full_scan_python(start_date, end_date)
```

```rust
// Rust CLI 主流程：只做 discovery，不写数据库、不解析文件内容。
fn main() -> Result<()> {
    let request = read_stdin_json::<DiscoveryRequest>()?;
    let mut files = discover_files(&request)?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    write_stdout_json(files)?;
    Ok(())
}
```
