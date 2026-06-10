# Use performance-first hybrid fallback for Rust Office parsing

We optimize Office parsing for cold scanner runs by using a hybrid fallback policy: timeout and deterministic Office parse failures are audited without Python fallback, while environment-unavailable and contract failures may fall back to Python for report usefulness. This keeps cold-run tail latency controlled while preserving fallback where it protects usability rather than masking repeatable slow failures.
