# Config / services 最终依赖规范

| Role | Module | Responsibility |
|---|---|---|
| domain DTO | `src/models/scanner_contract.py` | scanner envelope/evidence validation |
| config adapter | `src/services/scanner_config.py` | explicit mutable scanner leaves |
| scanner adapter | `src/services/native_scanner.py` | lazy PyO3 import, conversion, errors |
| report module | `src/services/report_runner/` | report recipes and publication |
| persistence | `src/services/sqlite_store.py` | report SQLite only |
| rendering | `src/services/report_gen.py` | templates and Markdown |

Allowed dependency direction:

```text
src/cli → report module → scanner/config/persistence/render/model adapters
```

No module under `src/services` imports `src.cli`. Only `NativeScanner` imports
the native extension, and only its `build_context` method performs the PyO3
scanner call.
