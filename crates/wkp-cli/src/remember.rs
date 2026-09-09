//! `wkp remember`: the write path (design 7.4, 7.6, M2-5).

use std::path::PathBuf;

pub(crate) struct RememberOptions {
    pub(crate) path: PathBuf,
    pub(crate) item_type: String,
    pub(crate) scope: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) principal: String,
    pub(crate) signing_key_file: PathBuf,
    pub(crate) session: Option<String>,
}

/// Parses `wkp remember --type <type> --principal <principal>
/// --signing-key-file <path> [--scope <scope>] [--title <title>]
/// [--session <session>] [--path <dir>]`. `--principal`/
/// `--signing-key-file` stand in for the OS-keystore/`ssh-agent`
/// identity resolution design 7.3 describes and M2-1 explicitly deferred
/// -- neither is a secret (a principal string, a path), so passing them
/// on argv doesn't touch CLAUDE.md's "secrets never touch argv" rule; the
/// body itself, which might be, comes from stdin instead.
pub(crate) fn parse_remember_args(
    mut args: impl Iterator<Item = String>,
) -> Result<RememberOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut item_type: Option<String> = None;
    let mut scope: Option<String> = None;
    let mut title: Option<String> = None;
    let mut principal: Option<String> = None;
    let mut signing_key_file: Option<PathBuf> = None;
    let mut session: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--type" => item_type = Some(args.next().ok_or("--type requires a value")?),
            "--scope" => scope = Some(args.next().ok_or("--scope requires a value")?),
            "--title" => title = Some(args.next().ok_or("--title requires a value")?),
            "--principal" => principal = Some(args.next().ok_or("--principal requires a value")?),
            "--signing-key-file" => {
                signing_key_file = Some(PathBuf::from(
                    args.next().ok_or("--signing-key-file requires a value")?,
                ));
            }
            "--session" => session = Some(args.next().ok_or("--session requires a value")?),
            "--path" => path = PathBuf::from(args.next().ok_or("--path requires a value")?),
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let item_type = item_type.ok_or_else(|| "remember requires --type <type>".to_string())?;
    let principal =
        principal.ok_or_else(|| "remember requires --principal <principal>".to_string())?;
    let signing_key_file = signing_key_file
        .ok_or_else(|| "remember requires --signing-key-file <path>".to_string())?;

    Ok(RememberOptions {
        path,
        item_type,
        scope,
        title,
        principal,
        signing_key_file,
        session,
    })
}

/// Frontmatter for a `wkp remember`-written item (design 5.4, 7.4):
/// always `confidence: proposed` and `provenance.source: conversation` --
/// unconditionally, regardless of `item_type` -- matching CLAUDE.md's
/// hard rule that agent-written memory lands in `inbox/` with
/// `confidence: proposed` and is never allowed to claim otherwise of
/// itself. Distinct from [`crate::import::okf_frontmatter_block`] (the
/// importer's own frontmatter shape, `source: import`, no
/// `provenance.actor`/`session`).
pub(crate) fn remember_frontmatter_block(
    title: &str,
    item_type: &str,
    scope: Option<&str>,
    actor: &str,
    session: Option<&str>,
) -> String {
    let mut block = format!("---\ntitle: {title:?}\ntype: {item_type}\n");
    if let Some(scope) = scope {
        block.push_str(&format!("scope: {scope}\n"));
    }
    block.push_str("provenance:\n");
    block.push_str(&format!("  actor: {actor:?}\n"));
    if let Some(session) = session {
        block.push_str(&format!("  session: {session:?}\n"));
    }
    block.push_str("  source: conversation\n");
    block.push_str("confidence: proposed\n---\n");
    block
}

/// Lowercase, hyphen-separated slug from `input` (a `--title`, or a
/// fallback like an item type when no title was given): keeps only
/// ASCII alphanumerics, collapses any run of other characters to a
/// single `-`, and trims leading/trailing `-`. Not required to be
/// unique on its own -- [`run_remember`] appends a per-call nanosecond
/// suffix for that.
pub(crate) fn slugify(input: &str) -> String {
    let mut slug = String::with_capacity(input.len());
    let mut last_was_dash = false;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !slug.is_empty() {
            slug.push('-');
            last_was_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "memory".to_string()
    } else {
        slug
    }
}

/// A one-line summary of what `wkp remember` did, for the CLI's stdout.
pub(crate) struct RememberSummary {
    pub(crate) relative_path: String,
    pub(crate) commit: wkp_git::signed_commit::CommitId,
}

impl std::fmt::Display for RememberSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "wkp: remembered {} ({})",
            self.relative_path, self.commit.0
        )
    }
}

/// `wkp remember`: the write path (design 7.4, 7.6, M2-5) -- reads the
/// body from stdin, refuses (no partial file, no commit) on a
/// [`wkp_core::secrets::scan`] hit, otherwise writes `inbox/<slug>.md`
/// atomically and commits it via [`wkp_git::sync::commit_to_device_branch`]
/// (M3-4: onto this device's own `sync/<device-id>` branch, design 6.2
/// point 1's "avoid" layer -- two devices then never contend for the same
/// ref) with [`wkp_git::provenance::Provenance`] trailers -- this is
/// CLAUDE.md's "agent-written memory lands in inbox/ with confidence:
/// proposed" hard rule made real, not just a frontmatter convention: the
/// commit itself is signed by the calling agent's own key, never a human
/// one, so `wkp_core::index::compute_tier`'s M2-6 replacement (the real
/// provenance gate) has something genuine to check.
pub(crate) fn run_remember(opts: &RememberOptions) -> Result<RememberSummary, String> {
    use std::io::Read;
    let mut body = String::new();
    std::io::stdin()
        .read_to_string(&mut body)
        .map_err(|e| format!("reading stdin: {e}"))?;
    run_remember_with_body(opts, &body)
}

/// [`run_remember`]'s actual logic, with the body passed in rather than
/// read from `std::io::stdin()` directly -- split out so tests can supply
/// a controlled string instead of needing a real piped-stdin subprocess,
/// the same reasoning `run_index`/`run_index_impl` already split on for
/// `$HOME`-reading.
pub(crate) fn run_remember_with_body(
    opts: &RememberOptions,
    body: &str,
) -> Result<RememberSummary, String> {
    let findings = wkp_core::secrets::scan(body);
    if !findings.is_empty() {
        let mut msg = String::from("refusing to write: content matches a credential pattern\n");
        for finding in &findings {
            msg.push_str(&format!(
                "  [{}] ...{}...\n",
                finding.rule, finding.redacted_excerpt
            ));
        }
        return Err(msg);
    }

    let title = opts.title.clone().unwrap_or_else(|| opts.item_type.clone());
    let slug_base = slugify(&title);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let relative_path = format!("inbox/{slug_base}-{nanos}.md");

    let frontmatter = remember_frontmatter_block(
        &title,
        &opts.item_type,
        opts.scope.as_deref(),
        &opts.principal,
        opts.session.as_deref(),
    );
    let content = format!("{frontmatter}\n{}\n", body.trim());
    let dest = opts.path.join(&relative_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    crate::atomic_write(&dest, &content)?;

    let provenance = wkp_git::provenance::Provenance {
        actor: Some(opts.principal.clone()),
        session: opts.session.clone(),
        source: Some("conversation".to_string()),
        confidence: Some("proposed".to_string()),
    };
    let subject = format!("remember: {title}");
    let device_id = wkp_git::sync::device_id(&opts.path)?;
    let commit = wkp_git::sync::commit_to_device_branch(
        &opts.path,
        &device_id,
        &[PathBuf::from(&relative_path)],
        &subject,
        &opts.principal,
        &opts.signing_key_file,
        &provenance,
    )?;

    Ok(RememberSummary {
        relative_path,
        commit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn slugify_lowercases_and_hyphenates() {
        assert_eq!(slugify("A Project Decision!"), "a-project-decision");
    }

    #[test]
    fn slugify_collapses_repeated_separators_and_trims_edges() {
        assert_eq!(slugify("  --weird__title--  "), "weird-title");
    }

    #[test]
    fn slugify_of_an_empty_or_all_punctuation_string_falls_back_to_memory() {
        assert_eq!(slugify(""), "memory");
        assert_eq!(slugify("!!!"), "memory");
    }

    #[test]
    fn remember_frontmatter_block_includes_provenance_and_confidence() {
        let block = remember_frontmatter_block(
            "A Title",
            "knowledge",
            Some("project"),
            "agent:claude-code@host",
            Some("sess-123"),
        );
        assert!(block.contains("title: \"A Title\""));
        assert!(block.contains("type: knowledge"));
        assert!(block.contains("scope: project"));
        assert!(block.contains("actor: \"agent:claude-code@host\""));
        assert!(block.contains("session: \"sess-123\""));
        assert!(block.contains("source: conversation"));
        assert!(block.contains("confidence: proposed"));
    }

    #[test]
    fn remember_frontmatter_block_omits_session_when_none() {
        let block = remember_frontmatter_block("A Title", "knowledge", None, "human:alice", None);
        assert!(!block.contains("session:"));
        assert!(!block.contains("scope:"));
    }

    #[test]
    fn parse_remember_args_requires_type_principal_and_signing_key_file() {
        assert!(parse_remember_args(args(&[])).is_err());
        assert!(parse_remember_args(args(&["--type", "knowledge"])).is_err());
        assert!(
            parse_remember_args(args(&["--type", "knowledge", "--principal", "agent:x"])).is_err()
        );
    }

    #[test]
    fn parse_remember_args_reads_all_flags() {
        let opts = parse_remember_args(args(&[
            "--type",
            "knowledge",
            "--scope",
            "project",
            "--title",
            "A Title",
            "--principal",
            "agent:claude-code@host",
            "--signing-key-file",
            "/tmp/key",
            "--session",
            "sess-123",
        ]))
        .expect("parse_remember_args");
        assert_eq!(opts.item_type, "knowledge");
        assert_eq!(opts.scope.as_deref(), Some("project"));
        assert_eq!(opts.title.as_deref(), Some("A Title"));
        assert_eq!(opts.principal, "agent:claude-code@host");
        assert_eq!(opts.signing_key_file, PathBuf::from("/tmp/key"));
        assert_eq!(opts.session.as_deref(), Some("sess-123"));
    }

    /// M2-5's own integration-test acceptance criterion: end to end
    /// against a temp store, verify the file, its frontmatter, the
    /// commit's signature, and its provenance trailers all agree.
    #[test]
    fn run_remember_writes_signed_commit_with_matching_frontmatter_and_trailers() {
        let temp = temp_dir("remember-end-to-end");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let key = generate_test_key_and_register(dir, "agent:claude-code@host");

        let opts = remember_opts(dir, &key, "knowledge", "A Remembered Thing");
        let summary = run_remember_with_body(&opts, "distinctive remembered content\n")
            .expect("run_remember");

        assert!(summary
            .relative_path
            .starts_with("inbox/a-remembered-thing-"));
        assert!(summary.relative_path.ends_with(".md"));

        let written =
            std::fs::read_to_string(dir.join(&summary.relative_path)).expect("read written file");
        assert!(written.contains("title: \"A Remembered Thing\""));
        assert!(written.contains("type: knowledge"));
        assert!(written.contains("scope: project"));
        assert!(written.contains("confidence: proposed"));
        assert!(written.contains("distinctive remembered content"));

        wkp_git::signed_commit::verify_commit(dir, &summary.commit)
            .expect("commit must have a valid signature");

        let trailers = wkp_git::provenance::read_provenance_trailers(dir, &summary.commit)
            .expect("read_provenance_trailers")
            .expect("expected Some(Provenance)");
        assert_eq!(trailers.actor.as_deref(), Some("agent:claude-code@host"));
        assert_eq!(trailers.session.as_deref(), Some("sess-abc"));
        assert_eq!(trailers.source.as_deref(), Some("conversation"));
        assert_eq!(trailers.confidence.as_deref(), Some("proposed"));
    }

    #[test]
    fn run_remember_refuses_content_matching_a_secret_pattern() {
        let temp = temp_dir("remember-secret-refused");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let key = generate_test_key_and_register(dir, "agent:claude-code@host");

        let opts = remember_opts(dir, &key, "knowledge", "Has A Secret");
        let result = run_remember_with_body(&opts, "AWS_ACCESS_KEY_ID=AKIAABCDEFGHIJKLMNOP\n");

        assert!(result.is_err(), "expected a secret-scan refusal");
        let inbox = dir.join("inbox");
        assert!(
            !inbox.exists() || std::fs::read_dir(&inbox).unwrap().next().is_none(),
            "no file must be written when the scan refuses the content -- the scan runs \
             before any file I/O or signed_commit call in run_remember_with_body"
        );
    }
}
