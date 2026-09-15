//! `wkp bundle export`/`wkp bundle import`: air-gapped sync (design 6.1,
//! M3-5).

use std::path::PathBuf;

pub(crate) struct BundleExportOptions {
    pub(crate) path: PathBuf,
    pub(crate) bundle_path: PathBuf,
    pub(crate) since: Option<String>,
}

/// `wkp bundle export <bundle-path> [--since <ref>] [--path <dir>]`.
pub(crate) fn parse_bundle_export_args(
    mut args: impl Iterator<Item = String>,
) -> Result<BundleExportOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut bundle_path: Option<PathBuf> = None;
    let mut since: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--since" => since = Some(args.next().ok_or("--since requires a value")?),
            "--path" => path = PathBuf::from(args.next().ok_or("--path requires a value")?),
            other if !other.starts_with('-') && bundle_path.is_none() => {
                bundle_path = Some(PathBuf::from(other));
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let bundle_path = bundle_path.ok_or(
        "bundle export requires an output path, e.g. `wkp bundle export /path/to/out.bundle`",
    )?;
    Ok(BundleExportOptions {
        path,
        bundle_path,
        since,
    })
}

/// A one-line summary of what `wkp bundle export` did, for the CLI's stdout.
pub(crate) struct BundleExportSummary {
    pub(crate) bundle_path: PathBuf,
    pub(crate) refs: Vec<String>,
}

impl std::fmt::Display for BundleExportSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "wkp: exported {} to {}",
            self.refs.join(", "),
            self.bundle_path.display()
        )
    }
}

/// `wkp bundle export`: the "export" half of design 6.1's air-gapped sync
/// (M3-5) -- a `git bundle` containing everything reachable from this
/// device's own `sync/<device-id>` branch, plus `main` if this store has
/// one locally. `--since <ref>` scopes it to an incremental range instead
/// of each ref's complete history. An explicit, human-invoked command
/// (never automatic/scheduled -- out of this task's scope).
pub(crate) fn run_bundle_export(opts: &BundleExportOptions) -> Result<BundleExportSummary, String> {
    let device_id = wkp_git::sync::device_id(&opts.path)?;
    let device_branch = wkp_git::sync::device_branch_name(&device_id);

    let mut refs = vec![device_branch];
    if wkp_git::local_branch_exists(&opts.path, "main") {
        refs.push("main".to_string());
    }

    let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();
    wkp_git::bundle_create(
        &opts.path,
        &opts.bundle_path,
        &ref_strs,
        opts.since.as_deref(),
    )?;

    Ok(BundleExportSummary {
        bundle_path: opts.bundle_path.clone(),
        refs,
    })
}

pub(crate) struct BundleImportOptions {
    pub(crate) path: PathBuf,
    pub(crate) bundle_path: PathBuf,
}

/// `wkp bundle import <bundle-path> [--path <dir>]`.
pub(crate) fn parse_bundle_import_args(
    mut args: impl Iterator<Item = String>,
) -> Result<BundleImportOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut bundle_path: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--path" => path = PathBuf::from(args.next().ok_or("--path requires a value")?),
            other if !other.starts_with('-') && bundle_path.is_none() => {
                bundle_path = Some(PathBuf::from(other));
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let bundle_path = bundle_path.ok_or(
        "bundle import requires a bundle path, e.g. `wkp bundle import /path/to/in.bundle`",
    )?;
    Ok(BundleImportOptions { path, bundle_path })
}

/// A one-line summary of what `wkp bundle import` did, for the CLI's stdout.
pub(crate) struct BundleImportSummary {
    pub(crate) merged: Vec<String>,
}

impl std::fmt::Display for BundleImportSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.merged.is_empty() {
            write!(f, "wkp: nothing new from the bundle")
        } else {
            write!(f, "wkp: merged {} from the bundle", self.merged.join(", "))
        }
    }
}

/// `wkp bundle import`: the "import" half of design 6.1's air-gapped sync
/// (M3-5) -- verifies the bundle first ([`wkp_git::bundle_verify`]),
/// refusing a corrupted or incompatible file with a clear error before
/// ever fetching from it, then fetches every ref it contains into
/// `refs/remotes/bundle/*` ([`wkp_git::bundle_fetch`]) and merges each one
/// into this device's own branch via
/// [`crate::sync_cmd::merge_refs_into_current_branch`] -- the same
/// merge/resolve machinery `wkp sync` (M3-4) uses for a network remote,
/// since "fetch from somewhere, then merge every branch that brought in"
/// doesn't care whether "somewhere" was a remote or a bundle file. There
/// is nothing to push back to in the air-gapped case, so this stops after
/// merging (unlike `wkp sync`). A bundle's own re-export of this device's
/// own branch (e.g. re-importing a bundle this same device produced) is
/// skipped rather than merged into itself.
pub(crate) fn run_bundle_import(opts: &BundleImportOptions) -> Result<BundleImportSummary, String> {
    wkp_git::bundle_verify(&opts.path, &opts.bundle_path)?;
    wkp_git::bundle_fetch(&opts.path, &opts.bundle_path)?;

    let device_id = wkp_git::sync::device_id(&opts.path)?;
    wkp_git::sync::ensure_device_branch(&opts.path, &device_id)?;
    let own_branch = wkp_git::sync::device_branch_name(&device_id);
    wkp_git::checkout_branch(&opts.path, &own_branch, false)?;

    let own_bundle_ref = format!("refs/remotes/bundle/{own_branch}");
    let refs_to_merge: Vec<String> = wkp_git::refs_matching(&opts.path, "refs/remotes/bundle/")?
        .into_iter()
        .filter(|r| *r != own_bundle_ref)
        .collect();

    let merged = crate::sync_cmd::merge_refs_into_current_branch(&opts.path, refs_to_merge)?;
    Ok(BundleImportSummary { merged })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn parse_bundle_export_args_reads_bundle_path_since_and_path() {
        let opts = parse_bundle_export_args(args(&[
            "out.bundle",
            "--since",
            "abc123",
            "--path",
            "/tmp/store",
        ]))
        .expect("parse_bundle_export_args");
        assert_eq!(opts.bundle_path, PathBuf::from("out.bundle"));
        assert_eq!(opts.since.as_deref(), Some("abc123"));
        assert_eq!(opts.path, PathBuf::from("/tmp/store"));
    }

    #[test]
    fn parse_bundle_export_args_requires_a_bundle_path() {
        assert!(parse_bundle_export_args(args(&[])).is_err());
    }

    #[test]
    fn parse_bundle_import_args_reads_bundle_path_and_path() {
        let opts = parse_bundle_import_args(args(&["in.bundle", "--path", "/tmp/store"]))
            .expect("parse_bundle_import_args");
        assert_eq!(opts.bundle_path, PathBuf::from("in.bundle"));
        assert_eq!(opts.path, PathBuf::from("/tmp/store"));
    }

    #[test]
    fn parse_bundle_import_args_requires_a_bundle_path() {
        assert!(parse_bundle_import_args(args(&[])).is_err());
    }

    /// M3-5's own acceptance criterion, end to end: export from one repo,
    /// import into a second repo that never shared a remote (or a single
    /// commit) with it at all -- both must converge, neither loses data.
    /// Same `allowed_signers` seeding technique as the `wkp sync`
    /// convergence test, for the same reason (a real add/add divergence
    /// on that tracked, non-`.md` file across genuinely unrelated
    /// histories, sidestepped by seeding identical content up front).
    #[test]
    fn wkp_bundle_export_then_import_converges_two_devices_with_no_shared_remote() {
        let device_a = temp_dir("bundle-device-a");
        let device_b = temp_dir("bundle-device-b");
        test_init(device_a.path()).expect("run_init a");
        test_init(device_b.path()).expect("run_init b");

        let key = generate_test_key_and_register_in_both(
            device_a.path(),
            device_b.path(),
            "agent:claude-code@host",
        );

        let opts_a = remember_opts(device_a.path(), &key, "knowledge", "Item from device A");
        crate::remember::run_remember_with_body(&opts_a, "content from device A")
            .expect("remember on A");
        let opts_b = remember_opts(device_b.path(), &key, "knowledge", "Item from device B");
        crate::remember::run_remember_with_body(&opts_b, "content from device B")
            .expect("remember on B");

        let bundle_dir = temp_dir("bundle-file");
        let bundle_path = bundle_dir.path().join("export.bundle");
        let export_opts = BundleExportOptions {
            path: device_a.path().to_path_buf(),
            bundle_path: bundle_path.clone(),
            since: None,
        };
        let export_summary = run_bundle_export(&export_opts).expect("run_bundle_export");
        assert!(bundle_path.is_file());
        assert_eq!(
            export_summary.refs.len(),
            1,
            "device A never made a local main"
        );

        let import_opts = BundleImportOptions {
            path: device_b.path().to_path_buf(),
            bundle_path,
        };
        let import_summary = run_bundle_import(&import_opts).expect("run_bundle_import");
        assert_eq!(import_summary.merged.len(), 1);

        let device_id_b = wkp_git::sync::device_id(device_b.path()).expect("device_id b");
        assert_eq!(
            wkp_git::current_branch(device_b.path()).expect("current_branch"),
            wkp_git::sync::device_branch_name(&device_id_b)
        );

        let mut all_content = String::new();
        for entry in std::fs::read_dir(device_b.path().join("inbox")).expect("read inbox dir") {
            let path = entry.expect("dir entry").path();
            all_content.push_str(&std::fs::read_to_string(&path).expect("read inbox item"));
        }
        assert!(all_content.contains("content from device A"));
        assert!(all_content.contains("content from device B"));
    }

    /// M3-5's provenance-gating criterion, verified explicitly rather than
    /// assumed from the `wkp sync` test: an item that arrives via
    /// `wkp bundle import` is still agent-signed (never re-signed by the
    /// unsigned merge commit that lands it on the importing device's
    /// branch), so it must not reach tier 0 or tier 1 just by having been
    /// imported -- M2-6/M2-7's provenance rules apply unchanged.
    #[test]
    fn wkp_bundle_import_does_not_grant_tier_0_or_1_to_the_merged_in_item() {
        let device_a = temp_dir("bundle-tier-device-a");
        let device_b = temp_dir("bundle-tier-device-b");
        test_init(device_a.path()).expect("run_init a");
        test_init(device_b.path()).expect("run_init b");

        let key = generate_test_key_and_register_in_both(
            device_a.path(),
            device_b.path(),
            "agent:claude-code@host",
        );

        let opts_a = remember_opts(device_a.path(), &key, "project-state", "Imported item");
        crate::remember::run_remember_with_body(&opts_a, "imported content should stay tier 2")
            .expect("remember on A");
        // Device B needs at least one commit of its own before it has a
        // device branch to merge the bundle into.
        let opts_b = remember_opts(device_b.path(), &key, "knowledge", "Local item on B");
        crate::remember::run_remember_with_body(&opts_b, "local content on B")
            .expect("remember on B");

        let bundle_dir = temp_dir("bundle-tier-file");
        let bundle_path = bundle_dir.path().join("export.bundle");
        run_bundle_export(&BundleExportOptions {
            path: device_a.path().to_path_buf(),
            bundle_path: bundle_path.clone(),
            since: None,
        })
        .expect("run_bundle_export");
        run_bundle_import(&BundleImportOptions {
            path: device_b.path().to_path_buf(),
            bundle_path,
        })
        .expect("run_bundle_import");

        crate::index_cmd::run_index(device_b.path()).expect("run_index after import");
        for tier in [0u8, 1] {
            crate::materialize::run_materialize(&crate::materialize::MaterializeOptions {
                path: device_b.path().to_path_buf(),
                tier,
            })
            .expect("materialize after import");
            let content =
                std::fs::read_to_string(device_b.path().join(format!(".wkp/tier{tier}.md")))
                    .expect("read materialized tier file");
            assert!(
                !content.contains("imported content should stay tier 2"),
                "tier {tier} must not include a bundle-imported, still agent-signed item: {content}"
            );
        }
    }
}
