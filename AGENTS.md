# Repository Guidelines

## Current State and Retained Compatibility
- Read [current runtime and acceptance state](docs/current-state.md) before operational work. Inspect actual task/configuration readback rather than historical preparation records.
- Preserve old packages, saved configurations, Python sources and tools, scanner_native, .venv and matching workers for scan recovery and rollback. Old scan-window gates apply only to old-version cleanup. Keep unfinished runs, response ledgers, snapshots, databases and archive evidence.
- Preserve current extraction coverage and keep OCR disabled unless explicitly authorized. Production tasks, model calls, follow-ups, Git publication and old-version deletion require the existing scoped authorization; documentation and local tests do not extend it.
- Preserve user and other-task changes. Historical evidence is indexed in [docs/archive](docs/archive/README.md); it retains the status and measurements of each recorded date.

## Project Structure and Contracts
- rust/report_app contains the CLI, configuration, run orchestration, report store, snapshots, model execution, reports, publication, maintenance, operations and archive.
- run_report dispatches daily/weekly/monthly reports. Daily orchestration holds the lock and delegates private steps in run/daily.rs; input preparation, recovery, model execution and publication keep their existing contracts.
- rust/Cargo.toml contains report_app, scanner_core, scanner_contract, worker_contract, discovery, text_parser, office_parser, pdf_parser and the build-only build_identity helper. PyO3 is outside the active workspace. text_parser holds the shared text implementation without scanner orchestration or database dependencies.
- Each parser/scanner build script calls build_identity so Cargo tracks shared build code as an explicit dependency. The helper is absent from runtime dependencies. Its metadata cache checks parser dependencies and all local manifests; source/profile/compiler checks remain active.
- Full engine build identity remains the Worker audit identity. Parser cache identity uses rust/parser-inputs.json and verified parser manifests; output-affecting request construction must participate in this identity. Legacy archive compatibility entries require exact parameters, executable/PDFium hashes and retained equivalence evidence.
- Current workers use protocol v4. Build all three executables together; retained old packages use their own matching workers.
- archive retains historical immutable originals, full-text extractions and report evidence under data_dir/archive using schema v1. Archive reports/frozen inputs use v4 and model plans v5; scan reports and historical recovery remain supported. Cache retention must never remove archive evidence.
- `report.profile=personal` uses daily scan input; historical frozen inputs retain their serialized formats. The active daily scan runs once at 09:00 for the previous Shanghai calendar day, with one parser worker and at most 120 seconds for scanning; 18,000 file characters leave room for 2,000 user characters. Reuse native structured excerpts and preserve parser coverage gaps. Scan evidence is a frozen current observation selected by modification time, not an archive version or proof of all changes that day. Scan reports use `data_dir/daily_cache` under a shared processing lock. Reports/frozen input use v5, model plans v6. Missing profile remains evidence and is omitted when serialized to preserve old fingerprints. Keep the historical archive APIs explicit.
- Personal scan schedules contain one 09:00 calendar trigger, with StartWhenAvailable and no logon, catchup or restart triggers. Collect/Parse commands, task kinds and their execution modules are retired. Health reads only Daily/Health and checks the scanner index. Historical reports and audit evidence remain readable.
- Personal limits: 20,000 input characters including 2,000 user characters; new summaries target 3–5 items/420–560 body characters including item titles, with shorter sparse-material reports allowed. Enforce maxima of 5 items/1,500 characters for new plans; historical v4/v5 plans keep their 5-item/600-character compaction limits and the report level retains 10/1,500. Summary plus independent evidence Markdown total at most 100,000 bytes. Freeze both views in the input template and persist attachment bytes/hash in the report; publish the immutable attachment before the summary, restore missing attachments from the report DB, and preserve externally modified files. No cumulative model call, token or cost quotas. Calls, including previews and unknown results, remain audited; date counts survive detail compaction. Processing files plus reservations are bounded by 7 days/1 GiB, with a 72 MiB control pool; diagnostic detail is 30 days/8 MiB plus personal JSONL logs 30 days/2 MiB.
- Personal daily work stops starting new operations at 270 seconds, closes within the 300-second application budget, and needs a scheduler limit of at least 360 seconds. Filesystem calls can block; external termination does not prove a result was saved. Scan discovery and parsing obey the native scanner limits and retain explicit coverage gaps.
- Personal communication failures stop after one request in that run. Scheduled daily runs target only the previous day. Manual regeneration uses current visible files; frozen input recovery does not rescan. Historical issues remain visible after material/detail expiry and clear only when the current publication resolves them. Manual replacement uses `daily --regenerate --reason "..."` without extra-call grants. Each logical step still has bounded retries and the run deadline; permanent usage and prior evidence are retained. Legacy quota settings and artifact fields are accepted only for compatibility and never block calls.
- New personal requests target 420–560 characters with a 1,500-character/5-item ceiling. Validate every item; compact drafts over 1,500 characters in a separately audited step before omitting anything, and preserve item count/order, kinds, certainty and citations during compaction. A draft that already fits the ceiling is published in full without a compaction gap. Only after compaction fails fall back to whole lower-priority items, retain next-step plans during both text and attachment-size reduction, and record failures/omissions. Freeze prompt versions and retain old prompt bytes and response behavior for saved requests.
- scanner_core::store exposes its API through mod.rs and separates persistence responsibilities into private modules.
- templates/ contains embedded prompts and MiniJinja report templates. Packaged copies must match compiled bytes. Track config/settings.example.yaml only; effective settings and secrets remain local.
- data/ stores databases and generated reports; logs/ stores runtime logs. Rust integration tests live under each package's tests/. tests/windows_operations.ps1 uses scheduler doubles; pytest modules are retained migration references.

## Build, Test, and Development Commands
- The Rust release runs on Windows x64 without Python. The retained Python production environment is CPython `3.13.13`; do not replace it during migration.
- `ai-daily-report.exe --config-dir <directory> doctor --strict` validates config, templates, databases, workers and the pinned PDFium DLL without network calls.
- `ai-daily-report.exe --config-dir <directory> daily -i "..."` generates a daily report. Add `--no-save` for an audited preview.
- Daily reports default to the previous completed calendar day. Prefer `--target-date YYYY-MM-DD`; `--date` remains a deprecated alias. `--dry-run` prepares input locally without model calls, while `--no-save` generates an audited preview.
- `ai-daily-report.exe weekly --source db` aggregates weekly reports; `--source scan` scans files instead.
- `ai-daily-report.exe monthly --source db` aggregates monthly reports.
- `ai-daily-report.exe --config-dir <directory> health [--target-date YYYY-MM-DD] [--format json|text]` checks Daily/Health task context, scanner index, published report and attachment hashes, pending/failed runs, disk and database integrity without model calls; exit codes 0 healthy, 1 warning, 2 unhealthy.
- The health task (`-Kind health`) runs `scripts/health_notify.ps1` at 11:00 and 23:00. Rust atomically writes `log_dir/health-YYYY-MM-DD.json`; PowerShell reads an explicit UTF-8 `health --scheduled --output` file, adds two task readbacks and records execution/notification failures separately. Resolved historical failures remain in their original ledger.
- `cargo test --manifest-path rust/Cargo.toml --workspace --locked` verifies the Rust workspace.
- Build debug workers and examples before integration tests; put the pinned `pdfium.dll` beside the PDF worker. Source identity changes require rebuilding both workers.
- `scripts/build_windows_release.ps1` builds the three production executables with static CRT in the shared `.artifacts/rust-migration/cargo-release` target directory and verifies the complete manifest. Reuse this default for normal releases and retain full debug information in the development cache. Never use the live legacy worker directory as its output.
- Final gates: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and the locked workspace tests, all with `--manifest-path rust/Cargo.toml`.

## Coding Style & Naming Conventions
- Rust style: use `cargo fmt`, ordinary modules and explicit types; avoid alternate parsers or automatic recovery frameworks.
- Use `snake_case` for functions/variables, `PascalCase` for classes, and `UPPER_SNAKE_CASE` for constants.
- Keep report business logic in `report_app`; reuse the native scanner and worker process controls.

### Architecture Naming Conventions
- `Scheduler`: 表示一次运行中的策略编排，不表示后台常驻任务、定时器或 daemon。
- `Planner`: 表示生成计划、预算或 profile，不执行重 I/O、解析或持久化副作用。
- `Classifier`: 表示对文件、上下文或 workload 做确定性分类，不做 I/O 副作用。
- `Parser`: 表示具体文件内容解析，输入文件和预算，输出可审计的解析结果。
- `Backend`: 只用于 parser backend 命名，例如 `light_text_v2`、`rust_office_oxide_v2`、`rust_pdfium_v3`，不用于业务服务命名。
- `Compressor`: 表示确定性上下文压缩，不调用 LLM，不重新解析文件。
- `Store` / `IndexStore`: 表示 SQLite 持久化、索引和 cache 载体，不承载业务流程判断。
- `Run`: 表示一次 CLI 命令执行、一次 scanner run 或一次 context 构建。
- `Decision`: 表示单个文件在一次 context run 中的策略选择和审计原因。
- 产品讨论中可以把 scanner、parser、compressor 理解成“能力代理”，但代码命名优先使用 `Scheduler`、`Planner`、`Parser`、`Compressor`、`Store` 等确定性术语，避免把职责边界写虚。
- Scanner backend changes must keep `parser_backend` separate from `worker_lane`; benchmark summaries should prove both the parser that produced content and the lane where it executed.
- `.xlsx` reports `rust_xlsx_bounded_v2`; `.docx` / `.pptx` use `rust_office_oxide_v2`; explicitly enabled `.doc` / `.ppt` use `rust_legacy_office_v3`; true XLS uses `rust_xls_calamine_v3`, and PDF uses `rust_pdfium_v3`. Each real format has one Rust path. Retired fallback options are rejected.
- Backend, timeout, and parser-budget changes must participate in the Rust-normalized scanner profile and cache identity to prevent stale parse-cache reuse.

## Testing Guidelines
- Rust tests cover parsing, schemas, report templates, SQLite state transitions, actual bounded worker processes and local HTTP fixtures. No real model calls without explicit cost authorization.
- Use isolated data/config for verification. Actual scheduled acceptance is separate evidence from local tests. Current deployment and retirement scope live in `docs/current-state.md`; earlier scan-version cleanup gates remain in `docs/archive/history/runtime-through-20261027.md`.

## Commit & Pull Request Guidelines
- Git history is minimal; no strict convention yet. Use short, imperative commit messages (e.g., "Add weekly scan limits").
- PRs should include a clear summary, linked issues (if any), and notes on config changes or template updates.

## Configuration & Security
- Do not commit API keys. Use `config/.secrets.yaml`, `DEEPSEEK_API_KEY`, or `OPENAI_API_KEY`.
- Change only the explicitly selected configuration directory. Verify current task/configuration readback before authorized operational changes.
- LLM backend is selectable via `llm.provider` (`deepseek` or `openai`). For DeepSeek, set `DEEPSEEK_API_KEY` (or `api.deepseek_api_key` in `config/.secrets.yaml`). For OpenAI, set `OPENAI_API_KEY` (or `api.openai_api_key` in `config/.secrets.yaml`) and use a model that supports JSON schema output (e.g., `gpt-4o-mini`).

## Collection retirement
- The 2026-10-27 retirement removes the disabled Collect/Parse tasks, their dedicated configuration, personal archive cache and their retired release as listed in docs/current-state.md. Hashed recovery ZIPs live under .artifacts/personal-collection-retirement-20261027. Preserve current scan snapshots, published report bytes, attachments, call ledgers and data/archive.
- Archive modules now serve retained evidence queries/validation only; no collection, queue drain, new archive daily input or automatic historical catchup. ReportSource::Archive remains serializable for frozen history but is not a CLI value. Retired collection configuration fields are rejected. Retained parser identity parameters serve historical evidence queries only.
