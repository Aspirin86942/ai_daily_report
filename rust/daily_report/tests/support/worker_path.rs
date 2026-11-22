use std::path::PathBuf;

pub fn pdf_worker() -> PathBuf {
    let directory = std::env::var_os("AI_DAILY_TEST_WORKER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned()
        });
    directory.join("ai-daily-pdf-worker.exe")
}
