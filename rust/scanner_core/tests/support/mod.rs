use std::path::{Path, PathBuf};

pub(super) fn process_fixture_path() -> PathBuf {
    let test_binary = std::env::current_exe().expect("test binary path should be available");
    let path = test_binary
        .parent()
        .and_then(Path::parent)
        .expect("test binary should be in the Cargo target directory")
        .join("examples")
        .join("process_fixture")
        .with_extension(std::env::consts::EXE_EXTENSION);
    assert!(
        path.is_file(),
        "Build the Rust process_fixture example before running this test: {}",
        path.display()
    );
    path
}
