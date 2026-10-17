# Repository Guidelines

## Project Structure & Module Organization
- `rust/report_app` contains the Rust CLI and report application: `config`, `run`, `store`, `snapshots`, `model`, `reports`, `publish`, `maintenance`, `operations`, and `archive`.
- `run_report` handles daily, weekly and monthly reports; frozen inputs and raw model responses preserve recovery evidence. Daily v3 extracts facts and composes one report.
- The live task uses `.releases/rust-20261016-02/ai-daily-report.exe` with `config-rust` after the 2026-10-16 capacity upgrade. Daily model plan v4 uses independent fact output limits, complete splits and grouped composition; report format stays v3. Retain the 2026-10-15 package and its saved config for unfinished model plan v3 runs. `main.py`, `src/`, the old Python tools, `scanner_native`, `.venv` and legacy workers remain available for rollback. Defer the planned source cleanup until the three real Rust scheduling windows pass; preserve the user environment, databases, reports and backups. See `docs/plans/2026-10-15-rust-cutover.md`.
- `templates/` contains embedded LLM prompts and MiniJinja report templates. Packaged copies must match their compiled bytes.
- `config/` stores YAML settings. Track only `settings.example.yaml`; keep `settings.windows.yaml` and `.secrets.yaml` local.
- `data/` stores the SQLite database and generated Markdown reports; `logs/` holds runtime logs.
- `rust/` contains `report_app`, `scanner_core`, `scanner_contract`, `worker_contract`, `discovery`, `office_parser`, and `pdf_parser`. Current source uses worker protocol v4; rebuild all three executables together. The retained production daily package still uses its original matching workers. PyO3 is outside the active workspace.
- `archive` keeps independent schema v1, raw content objects, immutable full-text extractions and report evidence under `data_dir/archive`. Archive daily reports/frozen inputs use v4 and model plans use v5; old scan reports and recovery remain supported. Cache retention must never remove archive evidence. See `docs/change-archive.md` and `docs/plans/2026-10-17-change-archive.md`.
- The independent current-user `AI Daily Report Collect` task uses `.releases/rust-20261017-archive-02` with `config-rust/archive-collector` every 30 minutes. Its real 2,648-file baseline and manual scheduler probe passed; partial and failed extraction gaps remain explicit. The daily task still uses the older package above until real cross-day archive collection and the existing three scheduled-window gates pass. See `.artifacts/rust-migration/archive-v1/production/acceptance.json` and `docs/archive-performance-20261017.md`.
- `scanner_core::store` keeps its public API in `mod.rs`; private `types`, `runs`, `lease`, `artifacts`, `retention` and `tests` modules separate persistence responsibilities.
- Rust integration tests live under each package's `tests/`; `tests/windows_operations.ps1` verifies operational scripts with scheduler doubles. Existing pytest modules are migration references.

## Build, Test, and Development Commands
- The Rust release runs on Windows x64 without Python. The retained Python production environment is CPython `3.13.13`; do not replace it during migration.
- `ai-daily-report.exe --config-dir <directory> doctor --strict` validates config, templates, databases, workers and the pinned PDFium DLL without network calls.
- `ai-daily-report.exe --config-dir <directory> daily -i "..."` generates a daily report. Add `--no-save` for an audited preview.
- Daily reports default to the previous completed calendar day. Prefer `--target-date YYYY-MM-DD`; `--date` remains a deprecated alias. `--dry-run` scans locally without model calls, while `--no-save` generates an audited preview.
- `ai-daily-report.exe weekly --source db` aggregates weekly reports; `--source scan` scans files instead.
- `ai-daily-report.exe monthly --source db` aggregates monthly reports.
- `collect --baseline` fixes the initial inventory; `collect` saves observed changes and drains the durable parse queue. `daily --source archive` uses archived changes, while `archive` and `stats` query evidence without model calls. Additional archive roots are local-only until explicitly allowed in `archive.model_roots`.
- `scripts/benchmark_archive.py` measures all 2,648 frozen files, tunes read/parse workers, and checks content signatures. Run timing suites without concurrent builds/tests; retain failures, partial coverage and all-file timing CSVs. The unchanged-collection gate is 20 runs with P95 <= 5 seconds and zero source-body reads, parse starts and model calls.
- `cargo test --manifest-path rust/Cargo.toml --workspace --locked` verifies the Rust workspace.
- Build debug workers and examples before integration tests; put the pinned `pdfium.dll` beside the PDF worker. Source identity changes require rebuilding both workers.
- `scripts/build_windows_release.ps1` builds the three production executables with static CRT in an isolated target directory and verifies the complete manifest. Never use the live legacy worker directory as its output.
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
- `Backend`: 只用于 parser backend 命名，例如 `light_text_v1`、`office_v1`、`pdf_text_v1`，不用于业务服务命名。
- `Compressor`: 表示确定性上下文压缩，不调用 LLM，不重新解析文件。
- `Store` / `IndexStore`: 表示 SQLite 持久化、索引和 cache 载体，不承载业务流程判断。
- `Run`: 表示一次 CLI 命令执行、一次 scanner run 或一次 context 构建。
- `Decision`: 表示单个文件在一次 context run 中的策略选择和审计原因。
- 产品讨论中可以把 scanner、parser、compressor 理解成“能力代理”，但代码命名优先使用 `Scheduler`、`Planner`、`Parser`、`Compressor`、`Store` 等确定性术语，避免把职责边界写虚。
- Scanner backend changes must keep `parser_backend` separate from `worker_lane`; benchmark summaries should prove both the parser that produced content and the lane where it executed.
- `.xlsx` reports `rust_xlsx_bounded_v2`; `.docx` / `.pptx` and explicitly enabled `.doc` / `.ppt` use `rust_office_oxide_v2`; true XLS uses calamine, and PDF uses `rust_pdfium_v3`. Each real format has one Rust path. Retired fallback options are rejected.
- Backend, fallback, timeout, and parser-budget changes must participate in the Rust-normalized scanner profile and cache identity to prevent stale parse-cache reuse.

## Testing Guidelines
- Rust tests cover parsing, schemas, report templates, SQLite state transitions, actual bounded worker processes and local HTTP fixtures. No real model calls without explicit cost authorization.
- Use isolated data/config for verification. Production task registration and three real daily windows are separate evidence from local tests.

## Commit & Pull Request Guidelines
- Git history is minimal; no strict convention yet. Use short, imperative commit messages (e.g., "Add weekly scan limits").
- PRs should include a clear summary, linked issues (if any), and notes on config changes or template updates.

## Configuration & Security
- Do not commit API keys. Use `config/.secrets.yaml`, `DEEPSEEK_API_KEY`, or `OPENAI_API_KEY`.
- Update `config/settings.windows.yaml` for path, model, and scanner limits changes.
 - LLM backend is selectable via `llm.provider` (`deepseek` or `openai`). For DeepSeek, set `DEEPSEEK_API_KEY` (or `api.deepseek_api_key` in `config/.secrets.yaml`). For OpenAI, set `OPENAI_API_KEY` (or `api.openai_api_key` in `config/.secrets.yaml`) and use a model that supports JSON schema output (e.g., `gpt-4o-mini`).
