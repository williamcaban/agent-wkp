//! Process hardening for the local binary (design 7.5, M6-1, issue
//! #170): on Linux, a [Landlock](https://docs.kernel.org/userspace-api/landlock.html)
//! ruleset restricting *write*-class filesystem access (create,
//! modify, delete, rename) to a caller-given set of paths, plus a
//! [seccomp](https://man7.org/linux/man-pages/man2/seccomp.2.html)
//! filter denying syscalls no normal `wkp` operation has any legitimate
//! reason to call.
//!
//! **Scope, deliberately narrow for this first pass**: write access
//! only, not read or execute. `wkp search`/`context`/`materialize`
//! (read-only, the latency-critical hot path CLAUDE.md's own
//! non-negotiable priority 1 protects) don't call this at all --
//! there's nothing to protect on the write side, and adding syscalls to
//! that path needs its own benchmark, not bundling into this task.
//! Commands that do write ([`crate::remember`], [`crate::promote`],
//! [`crate::forget`], [`crate::index_cmd`]) call [`restrict_writes_to`]
//! once they've resolved their own store path, passing it directly --
//! this module never calls `std::env::current_dir()` itself, since
//! `wkp init <path>` and any future `--path`-overriding command need
//! their own already-resolved path, not a hardcoded cwd assumption.
//!
//! **Best-effort by design** (Landlock's own documented posture, and
//! this module's own choice for seccomp): on a kernel without
//! Landlock/seccomp support, or an older kernel supporting only a
//! subset, this silently applies whatever it can rather than refusing
//! to run -- design 7.5 describes this as hardening for the common
//! case, not a hard requirement the binary enforces everywhere.
//!
//! **Non-Linux targets** (`aarch64-apple-darwin`): both underlying
//! crates are Linux-specific (Landlock is a Linux Security Module,
//! seccomp-bpf is a Linux syscall-filtering facility) and aren't even
//! compiled in on other platforms (`Cargo.toml`'s own
//! `target.'cfg(target_os = "linux")'` dependency table) -- the
//! functions below become no-ops there instead, so call sites never
//! need their own `#[cfg(...)]`.

use std::path::Path;

/// Denies these specific syscalls (an empty-condition rule, so each
/// matches unconditionally) while leaving every other syscall alone --
/// process-tracing, filesystem-mount manipulation, and kernel-module
/// loading, none of which any normal `wkp` operation (or the `git` it
/// shells out to, `wkp-git`'s own sanctioned exception) has a
/// legitimate reason to call. A conservative deny-list, not an
/// allow-list: enumerating every syscall the Rust standard library,
/// SQLite, and `git` itself actually need would be its own large,
/// fragile, ongoing research project (and getting it wrong fails
/// closed -- the process would simply stop working); blocking a short,
/// specific, well-understood set of dangerous ones is far lower risk
/// while still providing real hardening against a real class of
/// post-compromise technique (ptrace-based process injection, mount
/// manipulation, module loading -- all meaningful if `wkp` or a `git`
/// subprocess it spawns were ever compromised by malicious content).
/// A `const` array (rather than a function) doesn't work here: `SYS_iopl`
/// and `SYS_ioperm` (x86 I/O port permission control -- ARM has no legacy
/// I/O port address space, so `libc` doesn't define them for `aarch64` at
/// all) can only be included on the architectures where they exist. This
/// was a real `aarch64-unknown-linux-musl` cross-compile failure (`E0425:
/// cannot find value SYS_iopl in crate libc`), not a hypothetical -- caught
/// by this repo's own cross-compile CI job, not local testing (this
/// session's own `cargo build` only ever ran on x86_64).
#[cfg(target_os = "linux")]
fn denied_syscalls() -> Vec<i64> {
    let mut denied = vec![
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_reboot,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_acct,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_personality,
        libc::SYS_bpf,
    ];
    #[cfg(target_arch = "x86_64")]
    denied.extend([libc::SYS_iopl, libc::SYS_ioperm]);
    denied
}

/// Restricts write-class filesystem access ([`landlock::AccessFs::from_write`])
/// to exactly `allowed_write_paths`, applied to this process and (where
/// the running kernel supports it, Landlock ABI >= V8) every thread.
/// Read and execute access are left completely unrestricted.
///
/// A path that doesn't exist yet (e.g. `wkp init`'s own target
/// directory, about to be created) falls back to its parent, which
/// does -- a Landlock rule needs an openable file descriptor to anchor
/// to, and failing the whole sandbox setup over a directory this exact
/// call is about to create would be self-defeating.
///
/// Never errors just because the running kernel doesn't support
/// Landlock at all: that shows up as [`landlock::RulesetStatus::NotEnforced`]
/// (logged to stderr, informational), not an `Err` -- an `Err` here
/// means an actual Landlock API misuse, not "unsupported kernel," per
/// Landlock's own documented guidance. Returns `()`, not the status
/// object, so call sites (identical on every platform, per this
/// module's own doc comment) never need platform-specific handling --
/// this function logs the status itself.
#[cfg(target_os = "linux")]
pub(crate) fn restrict_writes_to(allowed_write_paths: &[&Path]) -> std::io::Result<()> {
    use landlock::{
        AccessFs, PathBeneath, PathFd, RestrictSelfAttr, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, ABI,
    };

    let abi = ABI::V9;
    let access_w = AccessFs::from_write(abi);

    let mut ruleset = Ruleset::default()
        .handle_access(access_w)
        .map_err(to_io_error)?
        .create()
        .map_err(to_io_error)?;

    for path in allowed_write_paths {
        let anchor: std::path::PathBuf = if path.exists() {
            path.to_path_buf()
        } else {
            path.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| path.to_path_buf())
        };
        let fd = PathFd::new(&anchor).map_err(to_io_error)?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access_w))
            .map_err(to_io_error)?;
    }

    let status = ruleset
        .all_threads(true)
        .map_err(to_io_error)?
        .restrict_self()
        .map_err(to_io_error)?;

    if status.ruleset != RulesetStatus::FullyEnforced {
        eprintln!(
            "wkp: write-sandbox not fully enforced by this kernel ({:?}); \
             continuing without full protection (design 7.5's best-effort posture)",
            status.ruleset
        );
    }
    Ok(())
}

/// Applies [`denied_syscalls`] to this process. Verified by hand (a
/// standalone forked-child test program, so a wrong-polarity filter
/// could never affect anything but that throwaway child): built a
/// filter listing only one syscall (`ptrace`) with `mismatch_action =
/// Allow` and `match_action = Errno`, confirmed an ordinary syscall
/// (`write`, via `println!`) still succeeded and the explicitly listed
/// one was refused with `EPERM` -- `seccompiler`'s own doc text for
/// these two parameters ("action taken for syscalls that do/don't
/// match the filter") is ambiguous enough about which one covers
/// completely *unlisted* syscalls that this was worth confirming
/// empirically rather than trusting the prose.
#[cfg(target_os = "linux")]
pub(crate) fn restrict_dangerous_syscalls() -> std::io::Result<()> {
    use seccompiler::{SeccompAction, SeccompFilter, TargetArch};
    use std::collections::BTreeMap;
    use std::convert::TryInto;

    #[cfg(target_arch = "x86_64")]
    let arch = TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    let arch = TargetArch::aarch64;

    let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
    for syscall in denied_syscalls() {
        rules.insert(syscall, vec![]);
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow, // mismatch_action: every unlisted syscall
        SeccompAction::Errno(libc::EPERM as u32), // match_action: the denied ones above
        arch,
    )
    .map_err(to_io_error)?;

    let bpf: seccompiler::BpfProgram = filter.try_into().map_err(to_io_error)?;
    seccompiler::apply_filter(&bpf).map_err(to_io_error)
}

#[cfg(target_os = "linux")]
fn to_io_error(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn restrict_writes_to(_allowed_write_paths: &[&Path]) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn restrict_dangerous_syscalls() -> std::io::Result<()> {
    Ok(())
}

// No in-crate tests here: proving either function's *effect* means
// actually applying an irrevocable process-wide restriction (Landlock
// and seccomp restrictions can never be relaxed once applied, only
// tightened further), which would then apply to every other test
// `cargo test` runs afterward in the same process -- these need real
// process isolation, not just a careful `#[test]`. `main.rs` exposes
// two hidden subcommands (`__sandbox-self-test-write`,
// `__sandbox-self-test-syscalls`) that apply each restriction and then
// do a small number of *safe* (no `unsafe`, which this crate forbids)
// probes, run as real subprocesses of the actually-compiled `wkp`
// binary by `tests/sandbox_integration.rs` -- the same "drive the real
// binary as a subprocess" pattern `wkp-hub`'s own integration tests
// already establish. The specific claim that `ptrace` itself gets
// blocked (as opposed to "ordinary operations still work, and denied
// ones don't crash the process") needs an actual `unsafe` FFI call to
// `ptrace` to observe -- forbidden in this crate -- so that half was
// verified by hand instead (a standalone throwaway program, documented
// in this module's own doc comment on [`restrict_dangerous_syscalls`]
// and this task's own PR description), not by an automated test here.
