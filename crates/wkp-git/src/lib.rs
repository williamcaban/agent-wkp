#![forbid(unsafe_code)]

//! Git plumbing wrapper, bundles, and sync.
//! CODEOWNERS-gated: changes here need a human co-sign (design 9.1).
//! No `Command::new("git")` is allowed outside this crate (CLAUDE.md hard rule).
//! Implementation lands starting in M2; see `docs/plan/milestones.md`.
//!
//! Split into modules by concern (version checking, init, working-tree
//! change detection, merge-conflict handling, remote/branch plumbing,
//! bundles) but re-exported flat from here, so every `wkp_git::foo(...)`
//! call site elsewhere in the workspace is unaffected by which file a
//! given function actually lives in.

mod bundle;
mod changes;
mod config;
mod conflicts;
mod init;
mod plumbing;
mod remote;
mod version;

pub mod allowed_signers;
pub mod provenance;
pub mod signed_commit;
pub mod sync;

#[cfg(test)]
mod test_support;

pub use bundle::{bundle_create, bundle_fetch, bundle_verify};
pub use changes::{detect_changes, list_tracked_files, remove_from_index, ChangeSet, Renamed};
pub use config::{fsmonitor_enabled, get_local_config, set_local_config};
pub use conflicts::{
    conflicts, modify_delete_conflicts, stage_path, Conflict, ConflictKind, DeletedBy,
    ModifyDeleteConflict,
};
pub use init::{apply_init_settings, commit_all, init_bare_repo, init_repo};
pub use remote::{
    checkout_branch, current_branch, fetch, finish_merge, local_branch_exists, merge_branch,
    other_device_sync_refs, push_branch, refs_matching, remote_branch_exists,
};
pub use version::{check_git_version, ensure_min_git_version, GitVersionCheck, MIN_GIT_VERSION};

use plumbing::{run_git, run_git_stdout, run_git_with_stdin};
