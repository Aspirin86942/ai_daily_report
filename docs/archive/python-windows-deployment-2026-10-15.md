# Windows x64 native release, deployment, and rollback

## Scope and authority

Only Windows x64 and exact CPython 3.13.13 are supported. Repository tools can
build and verify a release bundle, make a read-only SQLite backup, and update a
side-by-side release pointer. They do not authorize or automatically perform a
production install, process stop, configuration switch, database archival, or
deployment. Each production mutation requires a separate explicit approval.

The runtime chain is:

```text
CLI → ReportRunner → NativeScanner → PyO3 Scanner
→ scanner_core → worker v2 pools
```

Only Office/PDF workers are child processes. The scanner itself runs in the
Python process.

## Build and verify a release bundle

Use a clean Windows checkout and the repository `.venv`:

```powershell
.\.venv\Scripts\python.exe -m pytest tests -v
cargo fmt --manifest-path rust/Cargo.toml --all -- --check
cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets --locked -- -D warnings
cargo test --manifest-path rust/Cargo.toml --workspace --locked

New-Item -ItemType Directory -Path .\dist -Force | Out-Null
.\scripts\build_windows_release.ps1 `
  -OutputDirectory .\dist\ai-daily-report-2026.08.12 `
  -ReleaseVersion 2026.08.12
```

`build_windows_release.ps1` performs a locked release build, creates exactly
one non-abi3 `cp313-win_amd64` wheel with pinned maturin, installs it into a
disposable CPython 3.13.13 venv, imports it, and proves a wrong version is
rejected. It then builds an allowlisted bundle containing:

- the native wheel, including its repaired runtime DLLs;
- `bin/ai-daily-office-parser.exe`;
- the Python application, templates, example config, exact dependency lock,
  and deployment helpers;
- `manifest.json` with Git commit, target, Python/wheel identity, native and
  Office-worker build identities, Cargo.lock hash, and per-file size/SHA-256.

Verify an existing bundle without executing its application code:

```powershell
.\.venv\Scripts\python.exe scripts\windows_release.py verify-bundle `
  --bundle-dir .\dist\ai-daily-report-2026.08.12
```

The manifest detects missing, extra, changed, duplicate, or unsafe payload
paths. Hashes detect corruption; they do not authenticate a remote publisher.

## Side-by-side layout

An authorized deployment should keep releases immutable and state shared:

```text
<install-root>/
  current.json
  releases/<release-version>/
  shared/config/
  shared/data/reports/
  shared/data/db/reports.sqlite3
  shared/data/db/<legacy-scanner-db>.sqlite3
  shared/data/db/scan_index_v3.sqlite3
  shared/logs/
```

The application release contains the wheel and Office worker. The active
release uses its own CPython 3.13.13 venv and must pass native import, worker
hello, `doctor --strict`, and a daily `--no-save` smoke against a synthetic
test directory before pointer activation.

Mutable config, report SQLite, scanner SQLite, Markdown, and logs never live
inside a release directory. New and old releases must not share a scanner
database across the schema reset.

## Scanner database cutover

The current scanner schema is fresh-only `user_version=3`:

- a missing file is created as v3;
- an existing v3 file is opened;
- any other version fails with `SCANNER_DB_SCHEMA_MISMATCH` and is not changed.

Before an authorized cutover:

1. Stop current report processes.
2. Run `PRAGMA integrity_check` through the archival helper.
3. Use the SQLite backup API to create a timestamped read-only archive and
   hash manifest.
4. Keep the original scanner database and report database untouched.
5. Point the new release at a new `scan_index_v3.sqlite3` path.

The repository helper for step 2–3 is:

```powershell
.\scripts\archive_scanner_database.ps1 `
  -SourceDatabase C:\ai-daily-report\shared\data\db\<legacy-scanner-db>.sqlite3 `
  -ArchiveDirectory C:\ai-daily-report\scanner-db-archives
```

This is a production read and archive write, so run it only after explicit
authorization and only against the resolved target paths. It never migrates,
deletes, or modifies the source database.

## Pointer switch and rollback

Pointer mutation is deliberately gated by `-Apply`:

```powershell
.\scripts\update_release_pointer.ps1 `
  -Mode Switch `
  -PointerPath C:\ai-daily-report\current.json `
  -ReleaseVersion 2026.08.12 `
  -ScannerDatabasePath shared/data/db/scan_index_v3.sqlite3 `
  -Apply
```

The tool atomically records both `current` and `previous`, including each
release's scanner DB pointer. It rejects paths outside `shared/data/db`.

Rollback procedure:

1. Stop new report processes.
2. Verify the previous immutable release and its exact Python runtime.
3. Confirm its old scanner database still exists and was never modified.
4. Atomically swap the pointer.
5. Run previous-release `doctor --strict` and a synthetic smoke.

```powershell
.\scripts\update_release_pointer.ps1 `
  -Mode Rollback `
  -PointerPath C:\ai-daily-report\current.json `
  -Apply
```

The new v3 database remains for diagnosis. It is never converted backward.
Report SQLite, report Markdown, local configuration, and secrets are not
copied or rewritten by pointer changes.

## Deployment success gates

An authorized deployment is complete only when all of these pass on the
selected release:

- bundle manifest and all SHA-256 values;
- CPython 3.13.13 native import and build identity;
- Office and Python worker-v2 hello/capability checks;
- `doctor --strict` against the selected new v3 database path;
- daily `--no-save` smoke on a synthetic directory;
- no scanner child process;
- report SQLite and Markdown behavior unchanged.

Pushes, tags, remote artifacts, production installs, process control, pointer
changes, and real database operations are separate external actions and are
never implied by a local build or test request.
