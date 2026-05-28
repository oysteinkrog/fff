//! Axis-D guardrail: verify that the initial filesystem walk refuses to
//! proceed when `FilePickerOptions::max_indexable_files` is exceeded.
//!
//! The two failure modes observed in production (CPU diagnosis, 2026-05-28)
//! were a Claude Code session opened in `/c/users/oystein` and another in
//! `/c/WORK/desktop` — neither a git root, both with hundreds of thousands
//! of files. With the cap in place, both would have exited cleanly with a
//! machine-parseable `TooManyFiles` error at startup instead of pegging
//! CPU for hours.

use std::fs;

use fff_search::Error;
use fff_search::file_picker::{FFFMode, FilePicker};
use fff_search::{FilePickerOptions, SharedFilePicker, SharedFrecency};
use tempfile::TempDir;

/// Create `n` empty files under `dir`. Returns the temp dir handle so the
/// caller keeps it alive for the duration of the test.
fn seed_n_files(n: usize) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path();
    for i in 0..n {
        let path = base.join(format!("file_{i:05}.txt"));
        fs::write(&path, b"x").unwrap();
    }
    tmp
}

#[test]
fn walk_filesystem_aborts_when_max_indexable_files_exceeded() {
    let tmp = seed_n_files(50);

    let shared_picker = SharedFilePicker::default();
    let shared_frecency = SharedFrecency::default();

    let result = FilePicker::new_with_shared_state(
        shared_picker.clone(),
        shared_frecency,
        FilePickerOptions {
            base_path: tmp.path().to_string_lossy().to_string(),
            enable_mmap_cache: false,
            enable_content_indexing: false,
            mode: FFFMode::Ai,
            cache_budget: None,
            watch: false,
            follow_symlinks: false,
            max_indexable_files: 10, // cap below the 50 seeded
        },
    );

    // `new_with_shared_state` returns `Ok(())` because it only spawns the
    // background scan thread. The fatal error surfaces asynchronously via
    // the picker's `scan_error()` accessor, which is how the MCP binary's
    // startup polling loop detects the failure.
    assert!(result.is_ok(), "new_with_shared_state returned Err: {result:?}");

    // Wait for the background scan to settle and check the error slot.
    // 5 s is generous for a 50-file walk that aborts on file #11.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut scan_error = None;
    while std::time::Instant::now() < deadline {
        let guard = shared_picker.read().unwrap();
        if let Some(picker) = guard.as_ref() {
            if !picker.is_scan_active() {
                scan_error = picker.scan_error();
                break;
            }
        }
        drop(guard);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let err = scan_error.expect(
        "scan_error should be set after the walk aborts on file-count cap",
    );
    assert!(
        err.starts_with("Too many files"),
        "unexpected scan_error: {err}"
    );
    assert!(err.contains("limit is 10"), "unexpected scan_error: {err}");
}

#[test]
fn walk_filesystem_no_cap_when_max_indexable_files_is_zero() {
    let tmp = seed_n_files(20);

    let shared_picker = SharedFilePicker::default();
    let shared_frecency = SharedFrecency::default();

    FilePicker::new_with_shared_state(
        shared_picker.clone(),
        shared_frecency,
        FilePickerOptions {
            base_path: tmp.path().to_string_lossy().to_string(),
            enable_mmap_cache: false,
            enable_content_indexing: false,
            mode: FFFMode::Ai,
            cache_budget: None,
            watch: false,
            follow_symlinks: false,
            max_indexable_files: 0, // disabled
        },
    )
    .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut settled = false;
    while std::time::Instant::now() < deadline {
        let guard = shared_picker.read().unwrap();
        if let Some(picker) = guard.as_ref() {
            if !picker.is_scan_active() {
                assert!(
                    picker.scan_error().is_none(),
                    "scan_error set despite max_indexable_files=0"
                );
                settled = true;
                break;
            }
        }
        drop(guard);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(settled, "scan did not settle within 5s");
}

#[test]
fn too_many_files_error_message_includes_count_and_limit() {
    // Direct construction so the test does not depend on the walk timing.
    let err = Error::TooManyFiles {
        count: 12_345,
        limit: 10_000,
    };
    let display = err.to_string();
    assert!(display.contains("12345"), "missing count: {display}");
    assert!(display.contains("10000"), "missing limit: {display}");
}
