//! `wkp sync` / `wkp sync status`: fetch/merge/push across device
//! branches (design 6.1, 6.2, 6.4; M3-4), and reporting what didn't
//! auto-resolve (M3-6).

use std::path::{Path, PathBuf};

pub(crate) struct SyncOptions {
    pub(crate) path: PathBuf,
    pub(crate) remote: String,
}

/// `wkp sync [--path <dir>] [--remote <name>]` (default remote: `origin`).
pub(crate) fn parse_sync_args(
    mut args: impl Iterator<Item = String>,
) -> Result<SyncOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut remote = "origin".to_string();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--remote" => {
                remote = args.next().ok_or("--remote requires a value")?;
            }
            "--path" => {
                path = PathBuf::from(args.next().ok_or("--path requires a value")?);
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    Ok(SyncOptions { path, remote })
}

/// A one-line summary of what `wkp sync` did, for the CLI's stdout.
pub(crate) struct SyncSummary {
    pub(crate) remote: String,
    pub(crate) merged: Vec<String>,
    pub(crate) pushed_branch: String,
    /// Newly-arrived commits (this sync only, not the whole history) whose
    /// signature doesn't resolve to a known `human:`/`agent:` principal
    /// (M3-7) -- `"<short sha> <subject>"` per entry. Purely informational:
    /// `wkp_core::index::compute_tier`'s signature check already keeps
    /// these out of tier 0/1 regardless of whether this report exists.
    pub(crate) unsigned_or_unknown: Vec<String>,
}

impl std::fmt::Display for SyncSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.merged.is_empty() {
            write!(
                f,
                "wkp: nothing new from {} (pushed {})",
                self.remote, self.pushed_branch
            )?;
        } else {
            write!(
                f,
                "wkp: merged {} into {}, pushed {}",
                self.merged.join(", "),
                self.remote,
                self.pushed_branch
            )?;
        }
        if !self.unsigned_or_unknown.is_empty() {
            write!(
                f,
                "\nwkp: {} newly-arrived commit(s) unsigned or from an unknown signer (still tier 2 regardless):",
                self.unsigned_or_unknown.len()
            )?;
            for entry in &self.unsigned_or_unknown {
                write!(f, "\n  {entry}")?;
            }
        }
        Ok(())
    }
}

/// `wkp sync`: the actual "two machines... stay in sync" mechanic (design
/// 6.1, 6.2, 6.4; M3-4) -- synchronous and on-demand, no daemon (this
/// milestone's own scope note in `docs/plan/milestones.md`). `git fetch`,
/// then merge every other device's `sync/*` branch, plus `main` if the
/// remote has one, into this device's own `sync/<device-id>` branch --
/// M3-2's `wkp merge-driver` auto-resolves every content-mergeable class
/// as part of each merge itself, and M3-3's
/// [`crate::resolve_conflicts::resolve_modify_delete_conflicts`] handles
/// whatever's left (always exactly the modify/delete class, never
/// anything else -- verified by `wkp-git`'s own test that the merge
/// driver is never invoked for that class) before [`wkp_git::finish_merge`]
/// completes the merge commit. Finally pushes the local device branch --
/// never `main` directly, never force: landing content on `main` still
/// needs a human-signed commit or `wkp promote` (M2-6/M2-7's provenance
/// rules apply unchanged; an auto-merge commit here is never signed, so
/// `compute_tier` keeps whatever it touches out of tier 0/1 regardless of
/// which branch it's on).
pub(crate) fn run_sync(opts: &SyncOptions) -> Result<SyncSummary, String> {
    wkp_git::fetch(&opts.path, &opts.remote)?;

    let device_id = wkp_git::sync::device_id(&opts.path)?;
    wkp_git::sync::ensure_device_branch(&opts.path, &device_id)?;
    let own_branch = wkp_git::sync::device_branch_name(&device_id);
    wkp_git::checkout_branch(&opts.path, &own_branch, false)?;
    let before_tip = wkp_git::current_commit(&opts.path)?;

    let mut refs_to_merge = Vec::new();
    if wkp_git::remote_branch_exists(&opts.path, &opts.remote, "main") {
        refs_to_merge.push(format!("{}/main", opts.remote));
    }
    refs_to_merge.extend(wkp_git::other_device_sync_refs(
        &opts.path,
        &opts.remote,
        &device_id,
    )?);

    let merged = merge_refs_into_current_branch(&opts.path, refs_to_merge)?;
    let unsigned_or_unknown =
        report_unsigned_or_unknown_commits(&opts.path, &before_tip, &own_branch)?;

    wkp_git::push_branch(&opts.path, &opts.remote, &own_branch)?;

    Ok(SyncSummary {
        remote: opts.remote.clone(),
        merged,
        pushed_branch: own_branch,
        unsigned_or_unknown,
    })
}

/// M3-7: every commit newly reachable from `until` but not from `since`
/// whose signature doesn't resolve to a known `human:`/`agent:` principal
/// -- reusing [`wkp_git::commits_between`] and
/// [`wkp_git::allowed_signers::signer_for_commit`], no new signature
/// -checking logic. Purely reporting: this never changes tier
/// computation, confidence, or refuses the sync -- `compute_tier`'s own
/// signature gate (M2-6) already excludes anything this flags from tier
/// 0/1 regardless of whether anyone ever reads this report.
fn report_unsigned_or_unknown_commits(
    path: &Path,
    since: &str,
    until: &str,
) -> Result<Vec<String>, String> {
    let mut flagged = Vec::new();
    for (sha, subject) in wkp_git::commits_between(path, since, until)? {
        // A merge commit `wkp sync` itself just created is never signed
        // by design (see `merge_branch`'s doc comment) -- flagging it
        // would mean every sync that needed a real merge reports at
        // least one "unsigned" entry regardless of whether anything
        // actually concerning arrived, drowning out the real signal.
        if wkp_git::is_merge_commit(path, &sha)? {
            continue;
        }
        if wkp_git::allowed_signers::signer_for_commit(path, &sha).is_none() {
            flagged.push(format!("{} {subject}", &sha[..sha.len().min(12)]));
        }
    }
    Ok(flagged)
}

/// Merges each of `refs_to_merge` (already-resolved ref names, e.g.
/// `origin/main` or `refs/remotes/bundle/sync/device-b`) into whichever
/// branch is currently checked out, in order -- shared by `wkp sync`
/// (M3-4) and `wkp bundle import` (M3-5), since "fetch from somewhere,
/// then merge every branch that brought in" is identical for a network
/// remote and a bundle file; only how the refs got there differs. M3-2's
/// `wkp merge-driver` auto-resolves every content-mergeable class as part
/// of each merge itself; whatever's left is always exactly the
/// modify/delete class (M3-3's own boundary test), which
/// [`crate::resolve_conflicts::resolve_modify_delete_conflicts`] handles
/// before [`wkp_git::finish_merge`] completes the commit.
pub(crate) fn merge_refs_into_current_branch(
    path: &Path,
    refs_to_merge: Vec<String>,
) -> Result<Vec<String>, String> {
    let mut merged = Vec::new();
    for git_ref in refs_to_merge {
        let clean = wkp_git::merge_branch(path, &git_ref)?;
        if !clean {
            crate::resolve_conflicts::resolve_modify_delete_conflicts(path)?;
            wkp_git::finish_merge(path)?;
        }
        merged.push(git_ref);
    }
    Ok(merged)
}

/// `wkp sync status [--path <dir>]`: only flag is an optional store path
/// (default: cwd), matching `wkp index`/`wkp materialize`'s convention.
pub(crate) fn parse_sync_status_args(
    mut args: impl Iterator<Item = String>,
) -> Result<PathBuf, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--path" => path = PathBuf::from(args.next().ok_or("--path requires a value")?),
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(path)
}

/// `wkp sync status`: design 6.2 point 3's "ask" -- surfaces whatever
/// `wkp merge-driver` (M3-2) and `wkp sync`/`wkp bundle import`'s own
/// modify/delete handling (M3-3) didn't already auto-resolve, as an
/// explicit, harness-actionable report (M3-6). Reports an empty list on a
/// clean repo, not an error -- a conflict is not required for this to run
/// successfully. Deliberately read-only: resolving a reported conflict is
/// a normal editing task for whoever (human or agent) reads this report,
/// using `wkp`'s existing write commands, not something this command does
/// itself.
pub(crate) fn run_sync_status(path: &Path) -> Result<Vec<wkp_git::Conflict>, String> {
    wkp_git::conflicts(path)
}

/// A one-line-per-conflict summary of `wkp sync status`'s output, for the
/// CLI's stdout.
pub(crate) struct SyncStatusSummary {
    pub(crate) conflicts: Vec<wkp_git::Conflict>,
}

impl std::fmt::Display for SyncStatusSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.conflicts.is_empty() {
            return write!(f, "wkp: clean, nothing to resolve");
        }
        write!(f, "wkp: {} conflict(s):", self.conflicts.len())?;
        for conflict in &self.conflicts {
            write!(
                f,
                "\n  {}\t{}",
                describe_conflict_kind(conflict.kind),
                conflict.path.display()
            )?;
        }
        Ok(())
    }
}

fn describe_conflict_kind(kind: wkp_git::ConflictKind) -> &'static str {
    match kind {
        wkp_git::ConflictKind::ModifyDelete {
            deleted_by: wkp_git::DeletedBy::Ours,
        } => "modify/delete (deleted by us)",
        wkp_git::ConflictKind::ModifyDelete {
            deleted_by: wkp_git::DeletedBy::Theirs,
        } => "modify/delete (deleted by them)",
        wkp_git::ConflictKind::Content => "content",
        wkp_git::ConflictKind::AddAdd => "add/add",
        wkp_git::ConflictKind::BothDeleted => "both deleted",
        wkp_git::ConflictKind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    /// M3-4's own acceptance criterion, end to end: two genuinely
    /// independent local stores (never sharing a commit until they sync
    /// through a shared remote), each `wkp remember`-writing a different
    /// item, each `wkp sync`-ing -- both must end up with *both* items in
    /// their own device branch, and neither's own item is ever lost.
    #[test]
    fn wkp_sync_converges_two_independent_devices_through_a_shared_bare_remote() {
        let remote = temp_dir("sync-bare-remote");
        wkp_git::init_bare_repo(remote.path()).expect("init_bare_repo");

        let device_a = temp_dir("sync-device-a");
        let device_b = temp_dir("sync-device-b");
        test_init(device_a.path()).expect("run_init a");
        test_init(device_b.path()).expect("run_init b");

        let key = generate_test_key_and_register_in_both(
            device_a.path(),
            device_b.path(),
            "agent:claude-code@host",
        );

        for dir in [device_a.path(), device_b.path()] {
            wkp_git::set_local_config(dir, "remote.origin.url", &remote.path().to_string_lossy())
                .expect("set remote url");
            wkp_git::set_local_config(
                dir,
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            )
            .expect("set remote fetch refspec");
        }

        let opts_a = remember_opts(device_a.path(), &key, "knowledge", "Item from device A");
        crate::remember::run_remember_with_body(&opts_a, "content from device A")
            .expect("remember on A");
        let opts_b = remember_opts(device_b.path(), &key, "knowledge", "Item from device B");
        crate::remember::run_remember_with_body(&opts_b, "content from device B")
            .expect("remember on B");

        let sync_opts_a = SyncOptions {
            path: device_a.path().to_path_buf(),
            remote: "origin".to_string(),
        };
        let sync_opts_b = SyncOptions {
            path: device_b.path().to_path_buf(),
            remote: "origin".to_string(),
        };

        // A pushes first (remote is empty; nothing to merge yet).
        run_sync(&sync_opts_a).expect("first sync on A");
        // B fetches A's now-published branch and merges it in (their very
        // first shared commit ever, via --allow-unrelated-histories) --
        // B's device branch now holds both items.
        run_sync(&sync_opts_b).expect("first sync on B");
        // A fetches B's updated branch back; by now B's history contains
        // A's own original commit, so this is an ordinary, already-related
        // merge -- A's device branch also ends up with both items.
        run_sync(&sync_opts_a).expect("second sync on A");

        let device_id_a = wkp_git::sync::device_id(device_a.path()).expect("device_id a");
        let device_id_b = wkp_git::sync::device_id(device_b.path()).expect("device_id b");

        for (dir, device_id, label) in [
            (device_a.path(), &device_id_a, "A"),
            (device_b.path(), &device_id_b, "B"),
        ] {
            let branch = wkp_git::sync::device_branch_name(device_id);
            assert_eq!(
                wkp_git::current_branch(dir).expect("current_branch"),
                branch,
                "device {label} should be left on its own device branch"
            );

            let inbox_dir = dir.join("inbox");
            let mut all_content = String::new();
            for entry in std::fs::read_dir(&inbox_dir).expect("read inbox dir") {
                let path = entry.expect("dir entry").path();
                all_content.push_str(&std::fs::read_to_string(&path).expect("read inbox item"));
            }
            assert!(
                all_content.contains("content from device A"),
                "device {label} lost device A's item: {all_content}"
            );
            assert!(
                all_content.contains("content from device B"),
                "device {label} lost device B's item: {all_content}"
            );
        }
    }

    #[test]
    fn parse_sync_status_args_reads_path() {
        let path = parse_sync_status_args(args(&["--path", "/tmp/store"]))
            .expect("parse_sync_status_args");
        assert_eq!(path, PathBuf::from("/tmp/store"));
    }

    #[test]
    fn run_sync_status_reports_clean_on_a_repo_with_no_conflicts() {
        let temp = temp_dir("sync-status-clean");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::write(dir.join("item.md"), "hello\n").expect("write item.md");
        wkp_git::commit_all(dir, "initial").expect("commit_all");

        let conflicts = run_sync_status(dir).expect("run_sync_status");
        assert!(conflicts.is_empty(), "{conflicts:?}");
        assert_eq!(
            SyncStatusSummary { conflicts }.to_string(),
            "wkp: clean, nothing to resolve"
        );
    }

    /// M3-6's own acceptance criterion, end to end: force a real
    /// unresolved conflict that `wkp merge-driver` (M3-2, scoped to
    /// `*.md` paths only) and `resolve_modify_delete_conflicts` (M3-3,
    /// scoped to the modify/delete class only) cannot touch -- two
    /// branches genuinely changing the same non-`.md` path's content
    /// differently -- and confirm `wkp sync status` reports it.
    #[test]
    fn run_sync_status_reports_a_real_unresolved_content_conflict() {
        let temp = temp_dir("sync-status-content-conflict");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let main = wkp_git::current_branch(dir).expect("current_branch");

        std::fs::write(dir.join("notes.txt"), "base\n").expect("write notes.txt");
        wkp_git::commit_all(dir, "base").expect("commit_all");
        wkp_git::checkout_branch(dir, "feature", true).expect("branch feature");

        wkp_git::checkout_branch(dir, &main, false).expect("checkout main");
        std::fs::write(dir.join("notes.txt"), "changed by main\n").expect("write main change");
        wkp_git::commit_all(dir, "main changes").expect("commit_all");

        wkp_git::checkout_branch(dir, "feature", false).expect("checkout feature");
        std::fs::write(dir.join("notes.txt"), "changed by feature\n")
            .expect("write feature change");
        wkp_git::commit_all(dir, "feature changes").expect("commit_all");

        wkp_git::checkout_branch(dir, &main, false).expect("checkout main");
        let clean = wkp_git::merge_branch(dir, "feature").expect("merge_branch");
        assert!(!clean, "expected a real content conflict");

        let conflicts = run_sync_status(dir).expect("run_sync_status");
        assert_eq!(
            conflicts,
            vec![wkp_git::Conflict {
                path: PathBuf::from("notes.txt"),
                kind: wkp_git::ConflictKind::Content,
            }]
        );

        let summary = SyncStatusSummary { conflicts }.to_string();
        assert!(summary.contains("1 conflict"));
        assert!(summary.contains("content"));
        assert!(summary.contains("notes.txt"));
    }

    /// M3-7's own acceptance criterion, end to end: a `wkp sync` that
    /// brings in one deliberately-unsigned commit (alongside ordinary
    /// agent-signed `wkp remember` commits from both devices) must flag
    /// exactly that one commit as unsigned/unknown-signer, and must not
    /// false-positive on the ordinary signed ones.
    #[test]
    fn wkp_sync_reports_a_newly_arrived_unsigned_commit_without_false_positives() {
        let remote = temp_dir("sync-signature-remote");
        wkp_git::init_bare_repo(remote.path()).expect("init_bare_repo");

        let device_a = temp_dir("sync-signature-device-a");
        let device_b = temp_dir("sync-signature-device-b");
        test_init(device_a.path()).expect("run_init a");
        test_init(device_b.path()).expect("run_init b");

        let key = generate_test_key_and_register_in_both(
            device_a.path(),
            device_b.path(),
            "agent:claude-code@host",
        );

        for dir in [device_a.path(), device_b.path()] {
            wkp_git::set_local_config(dir, "remote.origin.url", &remote.path().to_string_lossy())
                .expect("set remote url");
            wkp_git::set_local_config(
                dir,
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            )
            .expect("set remote fetch refspec");
        }

        // A's own agent-signed item -- must land on A's device branch
        // (M3-4's bootstrap: the very first commit lands there directly),
        // then A makes one more, deliberately unsigned commit directly on
        // that same branch, standing in for content that reached this
        // history without going through wkp's own signed write path.
        let opts_a = remember_opts(device_a.path(), &key, "knowledge", "Signed item from A");
        crate::remember::run_remember_with_body(&opts_a, "signed content from A")
            .expect("remember on A");
        std::fs::write(device_a.path().join("unsigned.md"), "not signed\n")
            .expect("write unsigned.md");
        // Staged and committed narrowly (not `commit_all`'s `git add -A`):
        // A's bootstrap `remember` commit only ever staged its own inbox
        // path, so `.gitattributes`/`.gitignore`/`allowed_signers` are
        // still untracked in A's own working tree at this point -- an
        // `add -A` here would sweep those in too, diverging A's tree from
        // B's (which never staged them either) and turning the merge
        // below into an unrelated "untracked file would be overwritten"
        // failure that has nothing to do with what this test means to
        // exercise.
        wkp_git::stage_path(device_a.path(), "unsigned.md").expect("stage_path unsigned.md");
        wkp_git::commit_staged(device_a.path(), "an unsigned write")
            .expect("commit_staged (deliberately unsigned)");

        let sync_opts_a = SyncOptions {
            path: device_a.path().to_path_buf(),
            remote: "origin".to_string(),
        };
        let summary_a = run_sync(&sync_opts_a).expect("first sync on A (push only)");
        assert!(
            summary_a.unsigned_or_unknown.is_empty(),
            "nothing new arrived on A's own first sync: {:?}",
            summary_a.unsigned_or_unknown
        );

        // B's own agent-signed item, then sync: fetches A's branch
        // (bringing in both A's signed remember commit and A's unsigned
        // one), merges it in.
        let opts_b = remember_opts(device_b.path(), &key, "knowledge", "Signed item from B");
        crate::remember::run_remember_with_body(&opts_b, "signed content from B")
            .expect("remember on B");

        let sync_opts_b = SyncOptions {
            path: device_b.path().to_path_buf(),
            remote: "origin".to_string(),
        };
        let summary_b = run_sync(&sync_opts_b).expect("sync on B");

        assert_eq!(
            summary_b.unsigned_or_unknown.len(),
            1,
            "{:?}",
            summary_b.unsigned_or_unknown
        );
        assert!(summary_b.unsigned_or_unknown[0].contains("an unsigned write"));
        assert!(!summary_b
            .unsigned_or_unknown
            .iter()
            .any(|entry| entry.contains("remember")));
    }
}
