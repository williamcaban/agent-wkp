//! `wkp promote`: moves an `inbox/` item into the durable tree with a
//! human-signed commit (design 7.4, M2-7).

use std::path::{Path, PathBuf};

pub(crate) struct PromoteOptions {
    pub(crate) path: PathBuf,
    pub(crate) inbox_path: String,
    pub(crate) to: Option<String>,
    pub(crate) principal: String,
    pub(crate) signing_key_file: PathBuf,
}

/// Parses `wkp promote <inbox-path> [--to <dest-path>] --principal
/// <principal> --signing-key-file <path> [--path <dir>]`.
pub(crate) fn parse_promote_args(
    mut args: impl Iterator<Item = String>,
) -> Result<PromoteOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut inbox_path: Option<String> = None;
    let mut to: Option<String> = None;
    let mut principal: Option<String> = None;
    let mut signing_key_file: Option<PathBuf> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--to" => to = Some(args.next().ok_or("--to requires a value")?),
            "--principal" => principal = Some(args.next().ok_or("--principal requires a value")?),
            "--signing-key-file" => {
                signing_key_file = Some(PathBuf::from(
                    args.next().ok_or("--signing-key-file requires a value")?,
                ));
            }
            "--path" => path = PathBuf::from(args.next().ok_or("--path requires a value")?),
            other if inbox_path.is_none() && !other.starts_with('-') => {
                inbox_path = Some(other.to_string());
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let inbox_path = inbox_path
        .ok_or_else(|| "promote requires a path, e.g. `wkp promote inbox/foo.md`".to_string())?;
    let principal =
        principal.ok_or_else(|| "promote requires --principal <principal>".to_string())?;
    let signing_key_file =
        signing_key_file.ok_or_else(|| "promote requires --signing-key-file <path>".to_string())?;

    Ok(PromoteOptions {
        path,
        inbox_path,
        to,
        principal,
        signing_key_file,
    })
}

/// `.wkp/config.toml`'s `[promote] auto = [...]` list (design 7.4's
/// documented, off-by-default escape hatch): principals allowed to
/// promote their own inbox items without a human-signed commit. A
/// missing file, section or key all mean "empty list" (the safe
/// default), not an error.
///
/// A deliberately minimal, single-line-array reader -- real, valid TOML
/// syntax a full parser would also accept, just narrowly hand-read
/// rather than adding a `toml` crate dependency for one array of strings
/// (CLAUDE.md's slim-core rule, and "the config is TOML" doesn't require
/// *this crate* to own a general TOML parser, only that the format
/// written to disk be real TOML). Does not support a multi-line array.
pub(crate) fn read_promote_auto_list(store_path: &Path) -> Vec<String> {
    let config_path = store_path.join(".wkp/config.toml");
    let Ok(contents) = std::fs::read_to_string(&config_path) else {
        return Vec::new();
    };
    let mut in_promote_section = false;
    let mut result = Vec::new();
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_promote_section = trimmed == "[promote]";
            continue;
        }
        if !in_promote_section {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("auto") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim();
        let Some(inner) = rest.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
            continue;
        };
        for part in inner.split(',') {
            let part = part.trim().trim_matches('"').trim_matches('\'');
            if !part.is_empty() {
                result.push(part.to_string());
            }
        }
    }
    result
}

/// Every immediate subdirectory of `store_path/projects/`, sorted --
/// used to infer a `scope: project` item's destination when there is
/// exactly one project and `--to` wasn't given.
fn list_project_dirs(store_path: &Path) -> Result<Vec<String>, String> {
    let projects_root = store_path.join("projects");
    if !projects_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(&projects_root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.path().is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                dirs.push(name.to_string());
            }
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Infers a `wkp promote` destination from `scope` (design 5.4's layout
/// convention) when `--to` wasn't given: `scope: user` -> `user/<name>`;
/// `scope: project` -> `projects/<name>/<name>` only when there is
/// exactly one existing project directory to infer as "current" (no
/// per-session "current project" context exists at this layer -- see
/// `compute_tier`'s own note on the same ambiguity); `scope: org`, an
/// unrecognized scope, or no scope at all all refuse and ask for
/// `--to` explicitly, per this task's own acceptance criteria.
pub(crate) fn infer_promote_destination(
    store_path: &Path,
    inbox_path: &str,
    scope: Option<&wkp_core::frontmatter::Scope>,
) -> Result<String, String> {
    let basename = Path::new(inbox_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{inbox_path} has no file name"))?;

    match scope {
        Some(wkp_core::frontmatter::Scope::User) => Ok(format!("user/{basename}")),
        Some(wkp_core::frontmatter::Scope::Project) => {
            let project_dirs = list_project_dirs(store_path)?;
            match project_dirs.as_slice() {
                [only] => Ok(format!("projects/{only}/{basename}")),
                [] => Err(
                    "scope: project but this store has no projects/<name>/ directory yet -- \
                     use --to to specify the destination"
                        .to_string(),
                ),
                _ => Err(format!(
                    "scope: project but this store has multiple projects ({}) -- \
                     use --to to specify which one",
                    project_dirs.join(", ")
                )),
            }
        }
        Some(wkp_core::frontmatter::Scope::Org) => {
            Err("scope: org has no default destination -- use --to to specify one".to_string())
        }
        Some(wkp_core::frontmatter::Scope::Other(other)) => Err(format!(
            "unrecognized scope: {other} -- use --to to specify a destination"
        )),
        None => Err(
            "no scope: field to infer a destination from -- use --to to specify one".to_string(),
        ),
    }
}

/// A one-line summary of what `wkp promote` did, for the CLI's stdout.
pub(crate) struct PromoteSummary {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) commit: wkp_git::signed_commit::CommitId,
}

impl std::fmt::Display for PromoteSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "wkp: promoted {} -> {} ({})",
            self.from, self.to, self.commit.0
        )
    }
}

/// `wkp promote <inbox-path>`: moves an `inbox/` item into the durable
/// tree with a human-signed commit (design 7.4, M2-7) -- refuses unless
/// the calling identity is `role: human`, or the store's
/// `.wkp/config.toml` has explicitly opted the calling principal into
/// `[promote] auto = [...]` (design 7.4's documented, off-by-default
/// escape hatch). Content moves byte-for-byte -- promotion is about the
/// commit's signer, never a rewrite of the file's own
/// `confidence`/`provenance` frontmatter, which still honestly records
/// who originally wrote it. The move is one signed commit: the old
/// path's removal (`wkp_git::remove_from_index`) and the new path's
/// addition (inside `signed_commit`'s own `write-tree`) land together,
/// so there is no window where the content exists at the destination
/// unsigned.
pub(crate) fn run_promote(opts: &PromoteOptions) -> Result<PromoteSummary, String> {
    if !opts.inbox_path.starts_with("inbox/") {
        return Err(format!(
            "{} is not under inbox/ -- wkp promote only moves items out of inbox/",
            opts.inbox_path
        ));
    }

    let role = wkp_git::allowed_signers::SignerRole::from_principal(&opts.principal);
    let allowed_auto = read_promote_auto_list(&opts.path);
    if !matches!(role, wkp_git::allowed_signers::SignerRole::Human)
        && !allowed_auto.iter().any(|p| p == &opts.principal)
    {
        return Err(format!(
            "principal {} is not role:human and is not in this store's [promote] auto list \
             (.wkp/config.toml) -- promotion requires a human-signed commit",
            opts.principal
        ));
    }

    let src = opts.path.join(&opts.inbox_path);
    let contents =
        std::fs::read_to_string(&src).map_err(|e| format!("reading {}: {e}", opts.inbox_path))?;
    let parsed = wkp_core::frontmatter::parse(&contents);

    let dest_relative = match &opts.to {
        Some(to) => to.clone(),
        None => infer_promote_destination(
            &opts.path,
            &opts.inbox_path,
            parsed.frontmatter.scope.as_ref(),
        )?,
    };

    let dest = opts.path.join(&dest_relative);
    if dest.exists() {
        return Err(format!(
            "{dest_relative} already exists -- refusing to overwrite"
        ));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    crate::atomic_write(&dest, &contents)?;
    std::fs::remove_file(&src).map_err(|e| format!("removing {}: {e}", opts.inbox_path))?;
    wkp_git::remove_from_index(&opts.path, &opts.inbox_path)?;

    let provenance = wkp_git::provenance::Provenance {
        actor: Some(opts.principal.clone()),
        session: None,
        source: None,
        confidence: None,
    };
    let subject = format!("promote: {} -> {}", opts.inbox_path, dest_relative);
    let commit = wkp_git::signed_commit::signed_commit(
        &opts.path,
        &[PathBuf::from(&dest_relative)],
        &subject,
        &opts.principal,
        &opts.signing_key_file,
        &provenance,
    )?;

    Ok(PromoteSummary {
        from: opts.inbox_path.clone(),
        to: dest_relative,
        commit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn parse_promote_args_requires_path_principal_and_signing_key_file() {
        assert!(parse_promote_args(args(&[])).is_err());
        assert!(parse_promote_args(args(&["inbox/a.md"])).is_err());
        assert!(parse_promote_args(args(&["inbox/a.md", "--principal", "human:alice"])).is_err());
    }

    #[test]
    fn parse_promote_args_reads_all_flags() {
        let opts = parse_promote_args(args(&[
            "inbox/a.md",
            "--to",
            "projects/wkp/a.md",
            "--principal",
            "human:alice",
            "--signing-key-file",
            "/tmp/key",
        ]))
        .expect("parse_promote_args");
        assert_eq!(opts.inbox_path, "inbox/a.md");
        assert_eq!(opts.to.as_deref(), Some("projects/wkp/a.md"));
        assert_eq!(opts.principal, "human:alice");
        assert_eq!(opts.signing_key_file, PathBuf::from("/tmp/key"));
    }

    #[test]
    fn read_promote_auto_list_is_empty_when_config_toml_is_missing() {
        let temp = temp_dir("promote-auto-missing");
        assert!(read_promote_auto_list(temp.path()).is_empty());
    }

    #[test]
    fn read_promote_auto_list_reads_the_promote_section() {
        let temp = temp_dir("promote-auto-present");
        std::fs::create_dir_all(temp.path().join(".wkp")).expect("create .wkp");
        std::fs::write(
            temp.path().join(".wkp/config.toml"),
            "[other]\nx = 1\n\n[promote]\nauto = [\"agent:claude-code@host\", \"agent:opencode\"]\n",
        )
        .expect("write config.toml");
        assert_eq!(
            read_promote_auto_list(temp.path()),
            vec!["agent:claude-code@host", "agent:opencode"]
        );
    }

    #[test]
    fn read_promote_auto_list_is_empty_without_a_promote_section() {
        let temp = temp_dir("promote-auto-no-section");
        std::fs::create_dir_all(temp.path().join(".wkp")).expect("create .wkp");
        std::fs::write(temp.path().join(".wkp/config.toml"), "[other]\nx = 1\n")
            .expect("write config.toml");
        assert!(read_promote_auto_list(temp.path()).is_empty());
    }

    #[test]
    fn infer_promote_destination_uses_user_scope() {
        let temp = temp_dir("infer-dest-user");
        let dest = infer_promote_destination(
            temp.path(),
            "inbox/a.md",
            Some(&wkp_core::frontmatter::Scope::User),
        )
        .expect("infer_promote_destination");
        assert_eq!(dest, "user/a.md");
    }

    #[test]
    fn infer_promote_destination_uses_the_sole_project_directory() {
        let temp = temp_dir("infer-dest-project-one");
        std::fs::create_dir_all(temp.path().join("projects/wkp")).expect("create projects/wkp");
        let dest = infer_promote_destination(
            temp.path(),
            "inbox/a.md",
            Some(&wkp_core::frontmatter::Scope::Project),
        )
        .expect("infer_promote_destination");
        assert_eq!(dest, "projects/wkp/a.md");
    }

    #[test]
    fn infer_promote_destination_refuses_with_zero_or_multiple_projects() {
        let temp = temp_dir("infer-dest-project-none");
        assert!(infer_promote_destination(
            temp.path(),
            "inbox/a.md",
            Some(&wkp_core::frontmatter::Scope::Project),
        )
        .is_err());

        let temp2 = temp_dir("infer-dest-project-many");
        std::fs::create_dir_all(temp2.path().join("projects/a")).expect("create projects/a");
        std::fs::create_dir_all(temp2.path().join("projects/b")).expect("create projects/b");
        assert!(infer_promote_destination(
            temp2.path(),
            "inbox/x.md",
            Some(&wkp_core::frontmatter::Scope::Project),
        )
        .is_err());
    }

    #[test]
    fn infer_promote_destination_refuses_org_and_no_scope() {
        let temp = temp_dir("infer-dest-org-and-none");
        assert!(infer_promote_destination(
            temp.path(),
            "inbox/a.md",
            Some(&wkp_core::frontmatter::Scope::Org),
        )
        .is_err());
        assert!(infer_promote_destination(temp.path(), "inbox/a.md", None).is_err());
    }

    /// M2-7's own integration-test acceptance criteria: promote an
    /// M2-5-written inbox item with a human test key, then verify `wkp
    /// materialize --tier 0/1` (M2-6's real gate) now includes it.
    #[test]
    fn run_promote_moves_item_with_a_human_signed_commit_and_it_reaches_tier_1() {
        let temp = temp_dir("promote-human-signed");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let agent_key = generate_test_key_and_register(dir, "agent:claude-code@host");
        let inbox_path = seed_inbox_item(dir, &agent_key, "project", "promoted content here");

        // Not reachable at tier 1 before promotion: still in inbox/,
        // still agent-signed.
        crate::index_cmd::run_index(dir).expect("run_index before promote");
        crate::materialize::run_materialize(&crate::materialize::MaterializeOptions {
            path: dir.to_path_buf(),
            tier: 1,
        })
        .expect("materialize tier 1 before promote");
        let before = std::fs::read_to_string(dir.join(".wkp/tier1.md")).expect("read tier1.md");
        assert!(!before.contains("promoted content here"));

        let human_key = generate_test_key_and_register(dir, "human:alice");
        let promote_opts = PromoteOptions {
            path: dir.to_path_buf(),
            inbox_path: inbox_path.clone(),
            to: Some("projects/wkp/promoted.md".to_string()),
            principal: "human:alice".to_string(),
            signing_key_file: human_key.private_path,
        };
        let summary = run_promote(&promote_opts).expect("run_promote");
        assert_eq!(summary.from, inbox_path);
        assert_eq!(summary.to, "projects/wkp/promoted.md");

        assert!(
            !dir.join(&inbox_path).exists(),
            "old inbox path must be gone"
        );
        assert!(dir.join("projects/wkp/promoted.md").is_file());
        wkp_git::signed_commit::verify_commit(dir, &summary.commit)
            .expect("promote commit must be validly signed");

        crate::index_cmd::run_index(dir).expect("run_index after promote");
        crate::materialize::run_materialize(&crate::materialize::MaterializeOptions {
            path: dir.to_path_buf(),
            tier: 1,
        })
        .expect("materialize tier 1 after promote");
        let after = std::fs::read_to_string(dir.join(".wkp/tier1.md")).expect("read tier1.md");
        assert!(
            after.contains("promoted content here"),
            "promoted item must now reach tier 1: {after}"
        );
    }

    /// M2-7's second acceptance-criteria test: promotion attempted with
    /// only an agent key must fail.
    #[test]
    fn run_promote_refuses_an_agent_only_principal() {
        let temp = temp_dir("promote-agent-refused");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let agent_key = generate_test_key_and_register(dir, "agent:claude-code@host");
        let inbox_path = seed_inbox_item(dir, &agent_key, "project", "should stay in inbox");

        let promote_opts = PromoteOptions {
            path: dir.to_path_buf(),
            inbox_path: inbox_path.clone(),
            to: Some("projects/wkp/x.md".to_string()),
            principal: "agent:claude-code@host".to_string(),
            signing_key_file: agent_key.private_path,
        };
        let result = run_promote(&promote_opts);
        assert!(
            result.is_err(),
            "an agent-only principal must not be able to promote"
        );
        assert!(
            dir.join(&inbox_path).is_file(),
            "the inbox item must be untouched"
        );
    }

    #[test]
    fn run_promote_allows_an_agent_principal_explicitly_configured_for_auto_promote() {
        let temp = temp_dir("promote-auto-allowed");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let agent_key = generate_test_key_and_register(dir, "agent:claude-code@host");
        let inbox_path = seed_inbox_item(dir, &agent_key, "project", "auto-promoted content");

        std::fs::write(
            dir.join(".wkp/config.toml"),
            "[promote]\nauto = [\"agent:claude-code@host\"]\n",
        )
        .expect("write config.toml");

        let promote_opts = PromoteOptions {
            path: dir.to_path_buf(),
            inbox_path: inbox_path.clone(),
            to: Some("projects/wkp/auto.md".to_string()),
            principal: "agent:claude-code@host".to_string(),
            signing_key_file: agent_key.private_path,
        };
        run_promote(&promote_opts).expect("run_promote with promote: auto configured");
        assert!(dir.join("projects/wkp/auto.md").is_file());
    }

    #[test]
    fn run_promote_refuses_a_path_not_under_inbox() {
        let temp = temp_dir("promote-not-inbox");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let key = generate_test_key_and_register(dir, "human:alice");
        std::fs::create_dir_all(dir.join("projects/wkp")).expect("create projects/wkp");
        std::fs::write(dir.join("projects/wkp/a.md"), "already promoted\n").expect("write a.md");

        let promote_opts = PromoteOptions {
            path: dir.to_path_buf(),
            inbox_path: "projects/wkp/a.md".to_string(),
            to: Some("projects/wkp/b.md".to_string()),
            principal: "human:alice".to_string(),
            signing_key_file: key.private_path,
        };
        assert!(run_promote(&promote_opts).is_err());
    }

    #[test]
    fn run_promote_refuses_to_overwrite_an_existing_destination() {
        let temp = temp_dir("promote-dest-exists");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let agent_key = generate_test_key_and_register(dir, "agent:claude-code@host");
        let inbox_path = seed_inbox_item(dir, &agent_key, "project", "new content");

        let human_key = generate_test_key_and_register(dir, "human:alice");
        std::fs::create_dir_all(dir.join("projects/wkp")).expect("create projects/wkp");
        std::fs::write(dir.join("projects/wkp/taken.md"), "already here\n")
            .expect("seed destination");

        let promote_opts = PromoteOptions {
            path: dir.to_path_buf(),
            inbox_path,
            to: Some("projects/wkp/taken.md".to_string()),
            principal: "human:alice".to_string(),
            signing_key_file: human_key.private_path,
        };
        assert!(run_promote(&promote_opts).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("projects/wkp/taken.md")).expect("read taken.md"),
            "already here\n",
            "the existing destination file must be untouched"
        );
    }

    /// Content moves byte-for-byte -- promotion must not rewrite the
    /// file's own `confidence`/`provenance` frontmatter.
    #[test]
    fn run_promote_does_not_rewrite_the_moved_files_frontmatter() {
        let temp = temp_dir("promote-content-unchanged");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let agent_key = generate_test_key_and_register(dir, "agent:claude-code@host");
        let inbox_path = seed_inbox_item(dir, &agent_key, "project", "unchanged body");
        let original = std::fs::read_to_string(dir.join(&inbox_path)).expect("read original");

        let human_key = generate_test_key_and_register(dir, "human:alice");
        let promote_opts = PromoteOptions {
            path: dir.to_path_buf(),
            inbox_path,
            to: Some("projects/wkp/moved.md".to_string()),
            principal: "human:alice".to_string(),
            signing_key_file: human_key.private_path,
        };
        run_promote(&promote_opts).expect("run_promote");

        let moved =
            std::fs::read_to_string(dir.join("projects/wkp/moved.md")).expect("read moved file");
        assert_eq!(
            original, moved,
            "promote must move content byte-for-byte, including confidence/provenance"
        );
        assert!(moved.contains("confidence: proposed"));
        assert!(moved.contains("provenance:"));
    }
}
