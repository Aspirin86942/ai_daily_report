# Windows x64 deployment and rollback

## Trust boundary

The production runtime is a Python application shell with a Rust
scanner/context core. A release archive is untrusted data until a trusted copy
of `scripts\verify_windows_package.ps1` validates it. The trusted verifier must
come from the source checkout, a previously verified installation, or an
independently authenticated distribution. Never execute the verifier,
installer, Python, or PowerShell files from an unverified archive.

V1 provides integrity and corruption detection, not publisher authentication.
`manifest.json`, `SHA256SUMS`, and binary handshakes cannot prove who produced a
remote artifact. Do not auto-download or treat a remote artifact as trusted
until an independent anchor such as Authenticode or GitHub artifact
attestation/provenance is tied to the expected repository and tag.

## Build and package

Use a clean Windows x64 checkout and PowerShell:

```powershell
.\.venv\Scripts\python.exe -m pytest tests -v
cargo fmt --manifest-path rust/Cargo.toml --all --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path rust/Cargo.toml --workspace --locked
cargo build --manifest-path rust/Cargo.toml --workspace --release --locked

New-Item -ItemType Directory -Path .\dist -Force | Out-Null
.\scripts\package_windows.ps1 `
  -OutputPath .\dist\ai-daily-report-windows-x64.zip `
  -ReleaseVersion "2026.07.16"
```

The archive root is `ai-daily-report-windows-x64/` and contains exactly:

- `ai-daily-scanner.exe` and `ai-daily-office-parser.exe` at their fixed Rust
  release paths;
- `main.py`, every production Python source file, templates,
  `requirements.lock`, and `config/settings.example.yaml`;
- the deploy, verify, install, run, and rollback PowerShell scripts;
- `manifest.json` and `SHA256SUMS`.

It excludes local settings, `.secrets.yaml`, data, logs, `.venv`, build
intermediates, tests, and Cargo sources. Binaries and `rust/target` are never
committed to Git.

`manifest.json` records the Git commit, target triple, release version, Rust
engine version/build, contract versions, Cargo.lock SHA-256, and an
ordinal-sorted case-sensitive allowlist of every payload path, byte size, and
SHA-256. The manifest excludes itself and `SHA256SUMS`; `SHA256SUMS` covers the
manifest and every allowlisted payload and excludes only itself.

## Verify before extraction or execution

```powershell
.\scripts\verify_windows_package.ps1 `
  -ArchivePath .\dist\ai-daily-report-windows-x64.zip
```

The verifier first checks every ZIP name without extracting. It rejects an
unexpected root, missing or extra entry, duplicate or case-colliding name,
absolute/drive/UNC path, `..` traversal, alternate data stream name, directory
entry, symlink/reparse entry, and a non-canonical separator. It then extracts
into a GUID staging directory, verifies the exact allowlist, sizes and all
hashes, and only afterward runs the Rust scanner and Office-worker version
handshakes. Target/build/contract mismatch fails validation.

The archive entry set must equal exactly `{manifest.json, SHA256SUMS} +
manifest.files`. A Python file, template, lock file, PowerShell script, Rust
binary, manifest path, or extra entry tamper fails before package code runs.

## Prepare shared state and install

Choose an absolute install root. Create the shared configuration once; future
installs and rollbacks reuse it byte-for-byte.

```powershell
$installRoot = "D:\ai-daily-report"
New-Item -ItemType Directory `
  -Path "$installRoot\shared\config" -Force | Out-Null
Copy-Item -LiteralPath .\config\settings.example.yaml `
  -Destination "$installRoot\shared\config\settings.windows.yaml"

# Edit the shared file. paths.work_dir must be an absolute approved directory.
# Inject DEEPSEEK_API_KEY or OPENAI_API_KEY into the process/credential system.

.\scripts\install_windows_release.ps1 `
  -ArchivePath .\dist\ai-daily-report-windows-x64.zip `
  -InstallRoot $installRoot
```

The installer uses the trusted checkout verifier, moves the verified package
into a new `releases\<version>` directory, creates that release's `.venv`,
installs `requirements.lock`, and runs `doctor --strict`. Only after those
checks pass does it atomically replace `current.json`. It never overwrites an
existing release, local settings, secrets, report data, scan data, or logs.

Installed layout:

```text
<install-root>/
  current.json
  releases/
    <version-a>/
    <version-b>/
  shared/
    config/settings.windows.yaml
    data/reports/
    data/db/reports.sqlite3
    data/db/scan_index_v2.sqlite3
    logs/
  run_current_release.ps1
  rollback_windows_release.ps1
```

Keep at least the previous verified release. Do not place mutable config, data,
or logs below a version directory.

## Launcher path contract

`run_current_release.ps1` schema-validates `current.json`, proves the selected
relative path stays below `releases/`, rejects a reparse release directory,
sets the selected release as the process working directory, and directly
invokes `.venv\Scripts\python.exe` without another command shell.

It sets all six required absolute variables:

| Variable | Required value |
|---|---|
| `DAILY_REPORT_INSTALL_ROOT` | absolute installation root |
| `DAILY_REPORT_CONFIG_DIR` | `<install-root>\shared\config` |
| `DAILY_REPORT_DATA_DIR` | `<install-root>\shared\data` |
| `DAILY_REPORT_REPORTS_DIR` | `<install-root>\shared\data\reports` |
| `DAILY_REPORT_DB_DIR` | `<install-root>\shared\data\db` |
| `DAILY_REPORT_LOG_DIR` | `<install-root>\shared\logs` |

The presence of `DAILY_REPORT_INSTALL_ROOT` enables installed mode. All six
values must be existing absolute directories. `Config` rejects missing,
relative, version-local, or `shared/`-escaping paths and resolves them once.
The Rust request receives absolute scanner, Office worker, Python worker,
module root, and `scan_index_v2.sqlite3` paths. `logger.setup_logger()` uses the
resolved shared log directory. Strict doctor reports the configuration source
and every effective runtime directory and treats containment drift as an error.

Source-checkout development without `DAILY_REPORT_INSTALL_ROOT` retains
repository-relative data/report/database/log behavior.

Run from any directory:

```powershell
& "D:\ai-daily-report\run_current_release.ps1" doctor --strict
& "D:\ai-daily-report\run_current_release.ps1" list
```

## Upgrade and rollback

Install the next archive with the same trusted bootstrap and install root. The
installer validates the new release in its own directory before switching the
pointer. Shared configuration and data are not copied or rewritten.

```powershell
.\scripts\install_windows_release.ps1 `
  -ArchivePath .\dist\ai-daily-report-windows-x64-next.zip `
  -InstallRoot "D:\ai-daily-report"

& "D:\ai-daily-report\run_current_release.ps1" doctor --strict
```

The scanner database schema upgrade from v1 to v2 is one-way. An
`upgrade-db` request with `apply=false` is a read-only audit only; it does not
authorize a later write. An `apply=true` request requires separate explicit
authorization and a new request ID. Neither mode may be run against the
configured production database as part of release preparation or verification
without that authorization.

The upgrade tool does not create or validate a backup. Before `apply=true`, the
operator must preserve a recoverable pre-upgrade database copy, including the
database plus its WAL/shm sidecars, or use a deployment snapshot that captures
the same state. A rollback across this schema boundary must restore that
pre-upgrade copy before starting the old release. Restoring it discards runs
created after the upgrade; pointing an old release at the v2 database instead
fails closed as `TooNew`.

Rollback validates the previous release manifest and payload with the active
trusted verifier, runs the previous `.venv` strict doctor against the same
shared state, and atomically switches `current.json` only after both succeed:

```powershell
& "D:\ai-daily-report\rollback_windows_release.ps1"
& "D:\ai-daily-report\run_current_release.ps1" doctor --strict
```

Rust v2 owns `shared\data\db\scan_index_v2.sqlite3`; the retired scanner DB is
not destructively downgraded. Outside a schema upgrade, source-checkout rollback
is a Git revert and rebuild and installed rollback is the side-by-side pointer
operation. Remote tags, artifacts, attestations, releases, or branch protection
are external state and require separate explicit authorization to change.

## Release workflow evidence

`.github/workflows/windows-release.yml` builds the locked Windows workspace,
runs package structural/tamper tests, installs two locally identified packages
under a GUID synthetic root, runs strict doctor and zero-network smoke commands
from an unrelated cwd, switches A to B, rolls back B to A, verifies shared
config/data hashes, and ensures logs plus report/scan databases stay under
`shared/`. The workflow sets an LLM hard prohibition and never copies developer
credentials or business files.

Linux remains a compatibility-only source target. It has no production release
artifact, installed-mode guarantee, or Windows process-tree timeout claim.
