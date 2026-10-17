> 历史记录：保留当时的数字、状态和结论；当前运行与验收以[当前状态](../../current-state.md)为准。

# Use performance-first hybrid fallback for Rust Office parsing

We optimize Office parsing for cold scanner runs by using a hybrid fallback policy: timeout and deterministic Office parse failures are audited without Python fallback, while environment-unavailable and contract failures may fall back to Python for report usefulness. This keeps cold-run tail latency controlled while preserving fallback where it protects usability rather than masking repeatable slow failures.
