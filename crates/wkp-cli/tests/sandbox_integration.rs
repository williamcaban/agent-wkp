//! M6-1 (issue #170): real end-to-end proof that `sandbox.rs`'s two
//! functions actually do what they claim, run as real subprocesses of
//! the actually-compiled `wkp` binary via its two hidden, undocumented
//! `__sandbox-self-test-*` subcommands (`main.rs`'s own doc comment on
//! them explains why they exist and what they deliberately don't
//! prove: `wkp-cli`'s `#![forbid(unsafe_code)]` rules out an `unsafe`
//! FFI call to `ptrace` inside the crate itself, so the claim that the
//! *specific* denied syscalls actually fail was verified by hand
//! instead, in a standalone throwaway program -- see `sandbox.rs`'s own
//! doc comment on `restrict_dangerous_syscalls`).
//!
//! Linux-only, like the functions under test: on `aarch64-apple-darwin`
//! both hidden subcommands are still reachable (the underlying
//! functions are no-ops there) but there's nothing meaningful to assert
//! about restriction taking effect, so this test doesn't run there.

#![cfg(target_os = "linux")]

use std::process::Command;

fn wkp_bin() -> &'static str {
    env!("CARGO_BIN_EXE_wkp")
}

#[test]
fn write_sandbox_allows_inside_denies_outside() {
    let allowed = tempfile::tempdir().expect("tempdir");
    let denied = tempfile::tempdir().expect("tempdir");

    let output = Command::new(wkp_bin())
        .args([
            "__sandbox-self-test-write",
            allowed.path().to_str().expect("utf8 path"),
            denied.path().to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run wkp __sandbox-self-test-write");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected the write sandbox to allow the allowed dir and deny the denied one; \
         stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(stdout.contains("INSIDE_WRITE_OK=true"), "stdout={stdout:?}");
    assert!(
        stdout.contains("OUTSIDE_WRITE_DENIED=true"),
        "stdout={stdout:?}"
    );

    // Independently confirmed from the test process itself, not just
    // trusting the subprocess's own self-report: the file really is
    // there, and really isn't there.
    assert!(allowed.path().join("ok.txt").exists());
    assert!(!denied.path().join("nope.txt").exists());
}

#[test]
fn syscall_filter_leaves_ordinary_operations_working() {
    let probe_dir = tempfile::tempdir().expect("tempdir");

    let output = Command::new(wkp_bin())
        .args([
            "__sandbox-self-test-syscalls",
            probe_dir.path().to_str().expect("utf8 path"),
        ])
        .output()
        .expect("run wkp __sandbox-self-test-syscalls");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected an ordinary write to still succeed after the seccomp filter is applied \
         (a catastrophically backwards filter would fail this); stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("ORDINARY_WRITE_OK=true"),
        "stdout={stdout:?}"
    );
}
