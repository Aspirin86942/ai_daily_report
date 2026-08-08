//! SourceGuardV2 content-identity tests (spec SourceGuard v2, task-1 brief).
//!
//! The guard binds cache/snapshot identity to filesystem identity
//! (file id + change time) or a full-content SHA-256, never to the legacy
//! `mtime_ns:size` guess alone. A same-size, mtime-preserving replacement must
//! change the guard, otherwise the v2 cache/snapshot would reuse stale output.

use ai_daily_scanner_core::source_guard::{
    compute_source_guard, full_content_sha256, source_guard_kind_from_text, source_guard_kind_text,
    verify_guard, SourceGuardKind, SourceGuardV2,
};
use std::path::Path;
use std::time::SystemTime;

#[test]
fn same_size_and_mtime_replacement_must_change_guard() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.txt");
    std::fs::write(&p, "AAAA").unwrap();
    // 记录并伪造同 size+mtime 的替换内容
    let before = compute_source_guard(&p).unwrap();
    settle_metadata();
    std::fs::write(&p, "BBBB").unwrap();
    settle_metadata();
    let after = compute_source_guard(&p).unwrap();
    // guard 要么不可用（Unavailable，此时上层 fail closed），要么与内容绑定
    match (&before.kind, &after.kind) {
        (SourceGuardKind::Unavailable, _) => {}
        (_, SourceGuardKind::Unavailable) => {}
        _ => assert_ne!(before.guard_sha256, after.guard_sha256),
    }
}

/// Same-size replacement with the last-write time restored to the original.
/// Only the engine-owned change-time/file-id binding (or the full-content
/// fallback) can distinguish the replacement from the original, because the
/// legacy `mtime_ns:size` source version is identical.
#[test]
fn same_size_and_preserved_mtime_replacement_must_change_guard() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.txt");
    std::fs::write(&p, "AAAA").unwrap();
    let before = compute_source_guard(&p).unwrap();
    // A normal local temp file must form the platform metadata identity, not
    // silently degrade to the full-content fallback. A broken handle path in
    // windows_identity/unix_identity must fail the suite, not hide behind
    // content_sha256_v1.
    assert_eq!(
        before.kind,
        platform_metadata_guard_kind(),
        "a normal temp file must form the platform metadata identity"
    );
    let original_mtime = std::fs::metadata(&p).unwrap().modified().unwrap();

    settle_metadata();
    std::fs::write(&p, "BBBB").unwrap();
    restore_modified_time(&p, original_mtime);
    settle_metadata();

    let after = compute_source_guard(&p).unwrap();
    match (&before.kind, &after.kind) {
        (SourceGuardKind::Unavailable, _) | (_, SourceGuardKind::Unavailable) => {}
        _ => {
            assert_ne!(
                before.guard_sha256, after.guard_sha256,
                "same size + restored mtime must still change the guard"
            );
        }
    }
}

/// Pins the platform-specific identity path: a normal local file must produce
/// `WindowsFileIdChangeTimeV1` on Windows and `UnixInodeCtimeV1` on Unix, so a
/// regression that always falls back to the full-content hash is caught.
#[test]
fn normal_local_file_uses_platform_metadata_identity() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("identity.txt");
    std::fs::write(&p, "identity bytes").unwrap();

    let guard = compute_source_guard(&p).expect("guard computation must succeed");
    assert_eq!(
        guard.kind,
        platform_metadata_guard_kind(),
        "the primary filesystem-identity path must be exercised"
    );
    assert!(
        guard.guard_sha256.is_some(),
        "metadata identity carries a hash"
    );
}

#[cfg(windows)]
fn platform_metadata_guard_kind() -> SourceGuardKind {
    SourceGuardKind::WindowsFileIdChangeTimeV1
}

#[cfg(unix)]
fn platform_metadata_guard_kind() -> SourceGuardKind {
    SourceGuardKind::UnixInodeCtimeV1
}

#[cfg(not(any(windows, unix)))]
fn platform_metadata_guard_kind() -> SourceGuardKind {
    SourceGuardKind::ContentSha256V1
}

#[test]
fn verify_guard_recomputes_and_detects_content_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("v.txt");
    std::fs::write(&p, "AAAA").unwrap();
    let before = compute_source_guard(&p).unwrap();
    assert!(verify_guard(&p, &before), "unchanged source must verify");

    settle_metadata();
    std::fs::write(&p, "BBBB").unwrap();
    settle_metadata();
    let after = compute_source_guard(&p).unwrap();
    match (&before.kind, &after.kind) {
        (SourceGuardKind::Unavailable, _) | (_, SourceGuardKind::Unavailable) => {}
        _ => {
            assert!(
                !verify_guard(&p, &before),
                "replaced source must fail verification"
            );
            assert!(
                verify_guard(&p, &after),
                "current guard must verify against itself"
            );
        }
    }
}

#[test]
fn full_content_fallback_hashes_the_entire_source() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("data.txt");
    std::fs::write(&p, "The quick brown fox jumps over the lazy dog").unwrap();

    let hash = full_content_sha256(&p).expect("full content hash");
    // Well-known SHA-256 of the exact string above.
    assert_eq!(
        hash,
        "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
    );
}

#[test]
fn missing_source_guard_is_unavailable_not_invented() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("missing.txt");

    let guard = compute_source_guard(&p).expect("guard computation must not hard fail");
    assert_eq!(guard.kind, SourceGuardKind::Unavailable);
    assert_eq!(guard.guard_sha256, None);
    guard
        .validate()
        .expect("unavailable guard satisfies the invariant");
    // A guard for a different (available) identity must never verify: the
    // unavailable source must fail closed instead of matching an invented guard.
    let foreign = SourceGuardV2 {
        kind: SourceGuardKind::ContentSha256V1,
        guard_sha256: Some("a".repeat(64)),
    };
    assert!(
        !verify_guard(&p, &foreign),
        "unavailable source must not verify a foreign guard"
    );
}

#[test]
fn source_guard_kind_wire_text_matches_schema_literals() {
    assert_eq!(
        source_guard_kind_text(SourceGuardKind::WindowsFileIdChangeTimeV1),
        "windows_file_id_change_time_v1"
    );
    assert_eq!(
        source_guard_kind_text(SourceGuardKind::UnixInodeCtimeV1),
        "unix_inode_ctime_v1"
    );
    assert_eq!(
        source_guard_kind_text(SourceGuardKind::ContentSha256V1),
        "content_sha256_v1"
    );
    assert_eq!(
        source_guard_kind_text(SourceGuardKind::Unavailable),
        "unavailable"
    );
    assert_eq!(
        source_guard_kind_from_text("unavailable"),
        Some(SourceGuardKind::Unavailable)
    );
    assert_eq!(
        source_guard_kind_from_text("content_sha256_v1"),
        Some(SourceGuardKind::ContentSha256V1)
    );
    assert_eq!(source_guard_kind_from_text("guess"), None);
}

#[test]
fn source_guard_v2_validate_enforces_kind_hash_invariant() {
    assert!(
        SourceGuardV2 {
            kind: SourceGuardKind::ContentSha256V1,
            guard_sha256: None,
        }
        .validate()
        .is_err(),
        "available kind without a hash must be rejected"
    );
    assert!(
        SourceGuardV2 {
            kind: SourceGuardKind::ContentSha256V1,
            guard_sha256: Some("not-hex".to_string()),
        }
        .validate()
        .is_err(),
        "malformed hash must be rejected"
    );
    SourceGuardV2 {
        kind: SourceGuardKind::WindowsFileIdChangeTimeV1,
        guard_sha256: Some("a".repeat(64)),
    }
    .validate()
    .expect("available kind with a sha256 is valid");
    SourceGuardV2 {
        kind: SourceGuardKind::Unavailable,
        guard_sha256: None,
    }
    .validate()
    .expect("unavailable guard with a null hash is valid");
}

/// Windows/NTFS updates the change time and last-write time lazily; two writes
/// microseconds apart can otherwise be reported with identical metadata. Pausing
/// briefly makes the guard computation observe the settled filesystem identity.
fn settle_metadata() {
    std::thread::sleep(std::time::Duration::from_millis(50));
}

/// Restores the last-write time so the replacement keeps the original mtime.
#[cfg(windows)]
fn restore_modified_time(path: &Path, time: SystemTime) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, SetFileTime, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    assert_ne!(
        handle, INVALID_HANDLE_VALUE,
        "cannot open file to restore mtime"
    );
    let duration = time.duration_since(SystemTime::UNIX_EPOCH).unwrap();
    let total_hundreds =
        (duration.as_secs() + 11_644_473_600) * 10_000_000 + duration.subsec_nanos() as u64 / 100;
    let last_write = FILETIME {
        dwLowDateTime: (total_hundreds & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (total_hundreds >> 32) as u32,
    };
    let ok = unsafe { SetFileTime(handle, std::ptr::null(), std::ptr::null(), &last_write) };
    assert_ne!(ok, 0, "SetFileTime failed to restore the last-write time");
    unsafe { CloseHandle(handle) };
}

/// Restores the modified time on Unix via futimens semantics.
#[cfg(unix)]
fn restore_modified_time(path: &Path, time: SystemTime) {
    use std::fs::FileTimes;
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_times(FileTimes::new().set_modified(time))
        .expect("restoring the modified time must succeed");
}
