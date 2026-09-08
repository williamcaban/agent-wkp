#![forbid(unsafe_code)]

//! The `wkp` binary. Subcommands (`init`, `index`, `search`, `context`,
//! `materialize`, `hooks`, `remember`, `wkpd`, ...) land starting in M1;
//! see `docs/plan/milestones.md`.

use std::path::{Path, PathBuf};

fn main() {
    // Dispatching on a CLI flag, not a security-sensitive use of argv.
    let mut args = std::env::args().skip(1); // nosemgrep: rust.lang.security.args.args
    let command = args.next();

    match command.as_deref() {
        Some("--version" | "-V") => {
            println!("wkp {}", env!("CARGO_PKG_VERSION"));
        }
        Some("init") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            let path = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().expect("wkp: cannot read cwd"));
            match run_init(&path) {
                Ok(()) => println!("wkp: initialized store at {}", path.display()),
                Err(msg) => {
                    eprintln!("wkp: init failed: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("import") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            let path = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().expect("wkp: cannot read cwd"));
            let claude_home = claude_home_from_env();
            match run_import(&path, claude_home.as_deref()) {
                Ok(summary) => println!("{summary}"),
                Err(msg) => {
                    eprintln!("wkp: import failed: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("search") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match parse_search_args(args) {
                Ok(opts) => match run_search(&opts) {
                    Ok(output) => println!("{output}"),
                    Err(msg) => {
                        eprintln!("wkp: search failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("context") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match parse_search_args(args) {
                Ok(opts) => match run_context(&opts) {
                    Ok(output) => println!("{output}"),
                    Err(msg) => {
                        eprintln!("wkp: context failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("traverse") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match parse_traverse_args(args) {
                Ok(opts) => match run_traverse(&opts) {
                    Ok(output) => println!("{output}"),
                    Err(msg) => {
                        eprintln!("wkp: traverse failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("index") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match parse_index_args(args) {
                Ok(opts) => match run_index_cli(&opts) {
                    Ok(summary) => println!("{summary}"),
                    Err(msg) => {
                        eprintln!("wkp: index failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("materialize") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match parse_materialize_args(args) {
                Ok(opts) => match run_materialize(&opts) {
                    Ok(()) => {}
                    Err(msg) => {
                        eprintln!("wkp: materialize failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("remember") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match parse_remember_args(args) {
                Ok(opts) => match run_remember(&opts) {
                    Ok(summary) => println!("{summary}"),
                    Err(msg) => {
                        eprintln!("wkp: remember failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        // Deliberately no `ensure_min_git_version` check: `wkp hooks`
        // never touches git or the store, only prints static text (design
        // 3.3: "the binary never writes outside its own store" -- this
        // command doesn't write anywhere at all).
        Some("hooks") => match parse_hooks_args(args) {
            Ok(framework) => match render_hooks(&framework) {
                Ok(text) => println!("{text}"),
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            },
            Err(msg) => {
                eprintln!("wkp: {msg}");
                std::process::exit(1);
            }
        },
        _ => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            eprintln!("wkp: no subcommands implemented yet (see docs/plan/milestones.md)");
            std::process::exit(1);
        }
    }
}

/// `wkp init [path]`: makes `path` (default: cwd) a store — a git
/// repository with the settings design 5.1 wants, and a `.wkp/` directory
/// holding the derived, gitignored `index.db` (design 4.2, 5.2).
fn run_init(path: &Path) -> Result<(), String> {
    run_init_with_claude_home(path, claude_home_from_env().as_deref())
}

/// The actual body of `wkp init`, with `claude_home` injectable so tests
/// can pass a controlled, empty directory instead of silently scanning
/// whatever `$HOME/.claude` really contains on the machine running the
/// test suite -- exactly the mistake `claude_home_from_env`'s doc comment
/// warns about, this time on the *test* side rather than the CLI wiring.
fn run_init_with_claude_home(path: &Path, claude_home: Option<&Path>) -> Result<(), String> {
    wkp_git::init_repo(path)?;
    wkp_git::apply_init_settings(path)?;

    let wkp_dir = path.join(".wkp");
    std::fs::create_dir_all(&wkp_dir).map_err(|e| e.to_string())?;

    ensure_gitignored(path, &[".wkp/index.db", ".wkp/tier0.md"])?;

    // An empty index at this point; `wkp index` (M1-3) populates it from
    // the store's markdown files.
    wkp_core::index::build_index(&wkp_dir.join("index.db"), &[]).map_err(|e| e.to_string())?;

    // Import runs once at `wkp init` (design 10). Best-effort: a store
    // with nothing to import (no CLAUDE.md/AGENTS.md, no
    // ~/.claude/projects/*/memory/) must initialize cleanly with zero
    // imported items, not fail (M1-7's "net-new" acceptance criterion).
    run_import(path, claude_home)?;

    Ok(())
}

/// A one-line summary of what `wkp import` did.
struct ImportSummary {
    imported: usize,
    skipped: usize,
}

impl std::fmt::Display for ImportSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "wkp: imported {}, skipped {} (already present)",
            self.imported, self.skipped
        )
    }
}

/// Computes the real `claude_home` argument for [`run_import`] from the
/// process environment: `$HOME/.claude`, or `None` if `$HOME` isn't set.
/// Split out from the two call sites (`wkp init`, `wkp import`) as its own
/// pure, testable function specifically because getting this join wrong
/// (passing raw `$HOME` instead of `$HOME/.claude`) is an easy mistake
/// that unit tests calling `run_import` directly with a pre-built fixture
/// directory would never catch -- it only showed up testing the real
/// binary end to end.
fn claude_home_from_env() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| claude_home_dir(&h))
}

fn claude_home_dir(home: &std::ffi::OsStr) -> PathBuf {
    PathBuf::from(home).join(".claude")
}

/// Imports existing harness memory into `store_path`'s `inbox/import/`
/// (design 10, M1-7): `CLAUDE.md`/`AGENTS.md` at the store root, and every
/// `<claude_home>/projects/*/memory/*.md` file (Claude Code's own memory
/// store). `claude_home` is `$HOME/.claude` in the real CLI invocation,
/// injectable for tests so they don't scan whatever the *actual* sandbox's
/// home directory happens to contain.
///
/// Every imported item gets `confidence: proposed` unconditionally,
/// regardless of its mapped `type` -- CLAUDE.md's "agent-written memory
/// lands in inbox/ with confidence: proposed" rule, applied here to
/// "content that predates our provenance system" too: nothing here has
/// been through this tool's own review, so nothing here should be able to
/// reach tier 0/1 via `compute_tier`'s type-based heuristic (M1-4/M1-6).
///
/// Idempotent: a destination file that already exists is left alone
/// (skipped, not overwritten) -- a human may have edited their imported
/// copy since, and re-running `wkp init` must not clobber that.
fn run_import(store_path: &Path, claude_home: Option<&Path>) -> Result<ImportSummary, String> {
    let mut imported = 0;
    let mut skipped = 0;

    for (filename, slug) in [("CLAUDE.md", "claude-md"), ("AGENTS.md", "agents-md")] {
        let src = store_path.join(filename);
        if src.is_file() {
            let contents = std::fs::read_to_string(&src).map_err(|e| e.to_string())?;
            let block = okf_frontmatter_block(filename, "instruction", None, None);
            let rendered = format!("{block}\n{}\n", contents.trim());
            let dest_rel = format!("inbox/import/{slug}.md");
            if write_import_if_absent(store_path, &dest_rel, &rendered)? {
                imported += 1;
            } else {
                skipped += 1;
            }
        }
    }

    if let Some(home) = claude_home {
        let projects_root = home.join("projects");
        if projects_root.is_dir() {
            for entry in std::fs::read_dir(&projects_root).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let memory_dir = entry.path().join("memory");
                if !memory_dir.is_dir() {
                    continue;
                }
                for mem_entry in std::fs::read_dir(&memory_dir).map_err(|e| e.to_string())? {
                    let mem_path = mem_entry.map_err(|e| e.to_string())?.path();
                    if !mem_path.extension().is_some_and(|ext| ext == "md") {
                        continue;
                    }
                    let stem = mem_path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "memory".to_string());
                    let contents = std::fs::read_to_string(&mem_path).map_err(|e| e.to_string())?;
                    let claude_mem = parse_claude_memory_file(&contents);
                    let block = okf_frontmatter_block(
                        &claude_mem.title,
                        claude_mem.item_type,
                        claude_mem.scope,
                        claude_mem.updated.as_deref(),
                    );
                    let rendered = format!("{block}\n{}\n", claude_mem.body.trim());
                    let dest_rel = format!("inbox/import/claude-memory-{stem}.md");
                    if write_import_if_absent(store_path, &dest_rel, &rendered)? {
                        imported += 1;
                    } else {
                        skipped += 1;
                    }
                }
            }
        }
    }

    Ok(ImportSummary { imported, skipped })
}

fn write_import_if_absent(
    store_path: &Path,
    dest_rel: &str,
    contents: &str,
) -> Result<bool, String> {
    let dest = store_path.join(dest_rel);
    if dest.exists() {
        return Ok(false);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&dest, contents).map_err(|e| e.to_string())?;
    Ok(true)
}

fn okf_frontmatter_block(
    title: &str,
    item_type: &str,
    scope: Option<&str>,
    updated: Option<&str>,
) -> String {
    let mut block = format!("---\ntitle: {title:?}\ntype: {item_type}\n");
    if let Some(scope) = scope {
        block.push_str(&format!("scope: {scope}\n"));
    }
    block.push_str("provenance:\n  source: import\nconfidence: proposed\n");
    if let Some(updated) = updated {
        block.push_str(&format!("updated: {updated:?}\n"));
    }
    block.push_str("---\n");
    block
}

struct RememberOptions {
    path: PathBuf,
    item_type: String,
    scope: Option<String>,
    title: Option<String>,
    principal: String,
    signing_key_file: PathBuf,
    session: Option<String>,
}

/// Parses `wkp remember --type <type> --principal <principal>
/// --signing-key-file <path> [--scope <scope>] [--title <title>]
/// [--session <session>] [--path <dir>]`. `--principal`/
/// `--signing-key-file` stand in for the OS-keystore/`ssh-agent`
/// identity resolution design 7.3 describes and M2-1 explicitly deferred
/// -- neither is a secret (a principal string, a path), so passing them
/// on argv doesn't touch CLAUDE.md's "secrets never touch argv" rule; the
/// body itself, which might be, comes from stdin instead.
fn parse_remember_args(mut args: impl Iterator<Item = String>) -> Result<RememberOptions, String> {
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
/// itself. Distinct from [`okf_frontmatter_block`] (the importer's own
/// frontmatter shape, `source: import`, no `provenance.actor`/`session`).
fn remember_frontmatter_block(
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
fn slugify(input: &str) -> String {
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
struct RememberSummary {
    relative_path: String,
    commit: wkp_git::signed_commit::CommitId,
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
/// atomically and commits it via [`wkp_git::signed_commit::signed_commit`]
/// with [`wkp_git::provenance::Provenance`] trailers -- this is
/// CLAUDE.md's "agent-written memory lands in inbox/ with confidence:
/// proposed" hard rule made real, not just a frontmatter convention: the
/// commit itself is signed by the calling agent's own key, never a human
/// one, so `wkp_core::index::compute_tier`'s M2-6 replacement (the real
/// provenance gate) has something genuine to check.
fn run_remember(opts: &RememberOptions) -> Result<RememberSummary, String> {
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
fn run_remember_with_body(opts: &RememberOptions, body: &str) -> Result<RememberSummary, String> {
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
    atomic_write(&dest, &content)?;

    let provenance = wkp_git::provenance::Provenance {
        actor: Some(opts.principal.clone()),
        session: opts.session.clone(),
        source: Some("conversation".to_string()),
        confidence: Some("proposed".to_string()),
    };
    let subject = format!("remember: {title}");
    let commit = wkp_git::signed_commit::signed_commit(
        &opts.path,
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

struct ClaudeMemoryItem {
    title: String,
    item_type: &'static str,
    scope: Option<&'static str>,
    updated: Option<String>,
    body: String,
}

/// Parses Claude Code's own memory frontmatter (`name`/`description`/
/// nested `metadata.type`/`metadata.modified`) -- a different schema from
/// this project's own OKF frontmatter (`wkp_core::frontmatter`), so it
/// gets its own small parser here rather than reusing that module's
/// private field-mapping logic for an unrelated schema.
fn parse_claude_memory_file(contents: &str) -> ClaudeMemoryItem {
    let Some(after_open) = contents.strip_prefix("---\n") else {
        return ClaudeMemoryItem {
            title: "Imported memory".to_string(),
            item_type: "knowledge",
            scope: None,
            updated: None,
            body: contents.trim().to_string(),
        };
    };
    let Some(end_idx) = after_open.find("\n---") else {
        return ClaudeMemoryItem {
            title: "Imported memory".to_string(),
            item_type: "knowledge",
            scope: None,
            updated: None,
            body: contents.trim().to_string(),
        };
    };
    let block = &after_open[..end_idx];
    let rest = &after_open[end_idx..];
    let body = rest
        .strip_prefix("\n---")
        .unwrap_or(rest)
        .trim_start_matches('\n');

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut meta_type: Option<String> = None;
    let mut modified: Option<String> = None;
    let mut in_metadata = false;

    for raw_line in block.lines() {
        let trimmed = raw_line.trim_start();
        let indent = raw_line.len() - trimmed.len();
        if indent == 0 {
            in_metadata = trimmed.starts_with("metadata:");
            if let Some(v) = trimmed.strip_prefix("name:") {
                name = Some(unquote_yaml_scalar(v.trim()));
            } else if let Some(v) = trimmed.strip_prefix("description:") {
                description = Some(unquote_yaml_scalar(v.trim()));
            }
        } else if in_metadata {
            let trimmed = trimmed.trim();
            if let Some(v) = trimmed.strip_prefix("type:") {
                meta_type = Some(unquote_yaml_scalar(v.trim()));
            } else if let Some(v) = trimmed.strip_prefix("modified:") {
                modified = Some(unquote_yaml_scalar(v.trim()));
            }
        }
    }

    let (item_type, scope) = map_claude_memory_type(meta_type.as_deref());
    ClaudeMemoryItem {
        title: description
            .or(name)
            .unwrap_or_else(|| "Imported memory".to_string()),
        item_type,
        scope,
        updated: modified,
        body: body.trim().to_string(),
    }
}

fn unquote_yaml_scalar(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Claude Code's memory `metadata.type` (`user`/`feedback`/`project`/
/// `reference`) overloads what OKF frontmatter splits into `type` (design
/// 5.4's item-kind enum) and `scope` (who it's about). Best-effort
/// mapping, not a lossless round-trip.
fn map_claude_memory_type(t: Option<&str>) -> (&'static str, Option<&'static str>) {
    match t {
        Some("project") => ("project-state", Some("project")),
        Some("feedback") => ("feedback", Some("project")),
        Some("reference") => ("reference", Some("project")),
        Some("user") => ("knowledge", Some("user")),
        _ => ("knowledge", None),
    }
}

/// A one-line summary of what `wkp index` did, for the CLI's stdout.
struct IndexSummary {
    added: usize,
    modified: usize,
    deleted: usize,
}

impl std::fmt::Display for IndexSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.added == 0 && self.modified == 0 && self.deleted == 0 {
            write!(f, "wkp: index up to date")
        } else {
            write!(
                f,
                "wkp: indexed {} added, {} modified, {} deleted",
                self.added, self.modified, self.deleted
            )
        }
    }
}

/// `wkp index [path]`: re-indexes only what actually needs it (design
/// 5.1, M1-3) -- markdown files new to the index (a fresh clone, or the
/// first run after `wkp init`, where git may see nothing "changed" at all
/// even though `index.db` has never seen these files), files git's own
/// stat cache reports as modified, and files no longer present -- rather
/// than reading every file in the store on every run.
///
/// This needs two signals from git, not one: [`wkp_git::detect_changes`]
/// alone only reports drift *since the last commit*, which is empty right
/// after a commit even though the index may still be catching up to it.
/// [`wkp_git::list_tracked_files`] plus `detect_changes`'s untracked
/// entries gives the full current path universe, compared against
/// [`wkp_core::index::known_paths`] (what the index already has a row
/// for) to find genuinely new and genuinely gone paths.
/// Test-only convenience wrapper: every existing test predates M1-9's
/// `--embed-url` trio and just wants plain, non-embedding indexing.
#[cfg(test)]
fn run_index(path: &Path) -> Result<IndexSummary, String> {
    run_index_impl(path, None, None, None)
}

/// The actual body of `wkp index`, with the `--embed-url` trio injectable
/// so every existing test calling the plain [`run_index`] (no embeddings
/// involved) is unaffected by M1-9's addition. Embeddings, when
/// requested, are computed and attached to each upserted [`wkp_core::index::Item`]
/// *before* [`wkp_core::index::update_index`] runs -- landing in the same
/// atomic temp-file-then-rename write as everything else, never as a
/// separate write against the already-published `index.db` (CLAUDE.md's
/// "never write a file a harness reads in place").
fn run_index_impl(
    path: &Path,
    embed_url: Option<&str>,
    embed_model: Option<&str>,
    embed_key_file: Option<&Path>,
) -> Result<IndexSummary, String> {
    let changes = wkp_git::detect_changes(path)?;
    let tracked = wkp_git::list_tracked_files(path)?;
    let is_markdown = |p: &Path| p.extension().is_some_and(|ext| ext == "md");

    let mut current: std::collections::HashSet<String> = tracked
        .iter()
        .filter(|p| is_markdown(p))
        .map(|p| path_to_store_string(p))
        .collect();
    current.extend(
        changes
            .added
            .iter()
            .filter(|p| is_markdown(p))
            .map(|p| path_to_store_string(p)),
    );
    current.extend(
        changes
            .renamed
            .iter()
            .map(|r| &r.to)
            .filter(|p| is_markdown(p))
            .map(|p| path_to_store_string(p)),
    );
    for gone in changes
        .deleted
        .iter()
        .chain(changes.renamed.iter().map(|r| &r.from))
    {
        current.remove(&path_to_store_string(gone));
    }

    let index_path = path.join(".wkp/index.db");
    let known = {
        let conn = wkp_core::index::open_index(&index_path).map_err(|e| e.to_string())?;
        wkp_core::index::known_paths(&conn).map_err(|e| e.to_string())?
    };

    let added_paths: Vec<&String> = current.difference(&known).collect();
    let dirty: std::collections::HashSet<String> = changes
        .modified
        .iter()
        .chain(changes.renamed.iter().map(|r| &r.to))
        .filter(|p| is_markdown(p))
        .map(|p| path_to_store_string(p))
        .collect();
    let modified_paths: Vec<&String> = current
        .intersection(&known)
        .filter(|p| dirty.contains(*p))
        .collect();
    let deleted_paths: Vec<String> = known.difference(&current).cloned().collect();

    let mut upserts = Vec::new();
    for relative in added_paths.iter().chain(&modified_paths) {
        let contents = std::fs::read_to_string(path.join(relative))
            .map_err(|e| format!("reading {relative}: {e}"))?;
        let parsed = wkp_core::frontmatter::parse(&contents);
        // Design 7.4 / M2-6: `wkp-core` stays git-agnostic (design 3.3 --
        // it and `wkp-git` are siblings, neither depends on the other),
        // so this is where the real provenance gate's only external
        // input gets resolved: one `git log` per changed path, proportional
        // to change count (M1-3's own incremental-update cost model), not
        // to corpus size for an ordinary `wkp remember` + `wkp index`
        // cycle -- but a fresh clone's *first* `wkp index` run treats
        // every item as "added", so that one run pays one subprocess per
        // item. Not optimized in this task (a single batched `git log`
        // walk resolving every path's signer in one process would avoid
        // it); flagged here rather than silently absorbed, since M1-3/
        // M1-4's own benches don't exercise this wkp-cli-level path at
        // all and so can't catch a regression here.
        let human_signed = matches!(
            wkp_git::allowed_signers::last_signer_for_path(path, relative),
            Some((_, wkp_git::allowed_signers::SignerRole::Human))
        );
        upserts.push(wkp_core::index::Item {
            path: (*relative).clone(),
            frontmatter: parsed.frontmatter,
            body: parsed.body,
            embedding: None,
            human_signed,
        });
    }

    if let Some(embed_url) = embed_url {
        embed_upserts(&mut upserts, embed_url, embed_model, embed_key_file)?;
    }

    let summary = IndexSummary {
        added: added_paths.len(),
        modified: modified_paths.len(),
        deleted: deleted_paths.len(),
    };
    wkp_core::index::update_index(&index_path, &upserts, &deleted_paths)
        .map_err(|e| e.to_string())?;

    Ok(summary)
}

/// Parses `wkp index [path] [--embed-url URL] [--embed-model NAME]
/// [--embed-key-file PATH]`. The three `--embed-*` flags are always
/// parsed regardless of whether this binary was built with the `embed`
/// Cargo feature, so passing them to a non-`embed` build fails with a
/// clear message from [`embed_upserts`] rather than "unrecognized
/// argument".
fn parse_index_args(mut args: impl Iterator<Item = String>) -> Result<IndexOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut embed_url = None;
    let mut embed_model = None;
    let mut embed_key_file = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--embed-url" => {
                embed_url = Some(args.next().ok_or("--embed-url requires a value")?);
            }
            "--embed-model" => {
                embed_model = Some(args.next().ok_or("--embed-model requires a value")?);
            }
            "--embed-key-file" => {
                embed_key_file = Some(PathBuf::from(
                    args.next().ok_or("--embed-key-file requires a value")?,
                ));
            }
            other if !other.starts_with('-') => path = PathBuf::from(other),
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(IndexOptions {
        path,
        embed_url,
        embed_model,
        embed_key_file,
    })
}

struct IndexOptions {
    path: PathBuf,
    embed_url: Option<String>,
    embed_model: Option<String>,
    embed_key_file: Option<PathBuf>,
}

fn run_index_cli(opts: &IndexOptions) -> Result<IndexSummary, String> {
    run_index_impl(
        &opts.path,
        opts.embed_url.as_deref(),
        opts.embed_model.as_deref(),
        opts.embed_key_file.as_deref(),
    )
}

fn path_to_store_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Reads an embedding-endpoint API key from a file. CLAUDE.md's hard rule
/// ("secrets never touch argv or environment variables") rules out both
/// `--embed-api-key <key>` on argv and an env var like agent-wkp's
/// `WKP_EMBED_API_KEY` -- a `0600` file (or, for a local no-auth endpoint
/// like Ollama/llama.cpp, no key at all) is the only supported path.
#[cfg(feature = "embed")]
fn read_embed_key_file(path: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let meta =
        std::fs::metadata(path).map_err(|e| format!("--embed-key-file {}: {e}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(format!(
            "--embed-key-file {} must be mode 0600 (found {mode:03o}); run `chmod 600 {}`",
            path.display(),
            path.display()
        ));
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("reading --embed-key-file {}: {e}", path.display()))?;
    Ok(contents.trim().to_string())
}

#[cfg(feature = "embed")]
fn build_embed_config(
    embed_url: &str,
    embed_model: Option<&str>,
    embed_key_file: Option<&Path>,
) -> Result<wkp_core::embed::EmbedConfig, String> {
    let api_key = match embed_key_file {
        Some(p) => Some(read_embed_key_file(p)?),
        None => None,
    };
    Ok(wkp_core::embed::EmbedConfig {
        url: embed_url.to_string(),
        api_key,
        model: embed_model.map(str::to_string),
    })
}

/// Embeds `title\n\nbody` for every item in `items` via the configured
/// endpoint (design 5.3, M1-9), in place. A single item's embedding
/// failure is a warning on stderr, not a fatal error for the whole `wkp
/// index` run -- one unreachable/misconfigured endpoint mid-run shouldn't
/// stop the (already-working) plain text index from being updated.
#[cfg(feature = "embed")]
fn embed_upserts(
    items: &mut [wkp_core::index::Item],
    embed_url: &str,
    embed_model: Option<&str>,
    embed_key_file: Option<&Path>,
) -> Result<(), String> {
    let config = build_embed_config(embed_url, embed_model, embed_key_file)?;
    for item in items.iter_mut() {
        let text = format!(
            "{}\n\n{}",
            item.frontmatter.title.clone().unwrap_or_default(),
            item.body
        );
        match wkp_core::embed::embed_remote(&text, &config) {
            Ok(vector) => item.embedding = Some(vector),
            Err(e) => eprintln!("wkp: warning: could not embed {}: {e}", item.path),
        }
    }
    Ok(())
}

#[cfg(not(feature = "embed"))]
fn embed_upserts(
    _items: &mut [wkp_core::index::Item],
    _embed_url: &str,
    _embed_model: Option<&str>,
    _embed_key_file: Option<&Path>,
) -> Result<(), String> {
    Err(
        "this binary was built without hybrid search support (rebuild with `--features embed`); \
         omit --embed-url to index without embeddings"
            .to_string(),
    )
}

/// Runs a hybrid (BM25 + vector similarity) search, falling back to plain
/// BM25 with a stderr warning on any failure: a network error, or the
/// embedding endpoint's vector dimension not matching what's stored in
/// the index (design 5.3: "remains available... but only when the user
/// configures an endpoint", never blocking the default search path).
#[cfg(feature = "embed")]
fn run_hybrid_or_fallback(
    conn: &wkp_core::index::Connection,
    query: &str,
    embed_url: &str,
    embed_model: Option<&str>,
    embed_key_file: Option<&Path>,
    filter: &wkp_core::index::SearchFilter,
) -> Result<Vec<wkp_core::index::SearchHit>, String> {
    let config = build_embed_config(embed_url, embed_model, embed_key_file)?;
    let fallback_warning = |detail: &dyn std::fmt::Display| {
        eprintln!(
            "wkp: warning: semantic search unavailable ({detail}); falling back to BM25 keyword search."
        );
    };
    match wkp_core::embed::embed_remote(query, &config) {
        Ok(query_vector) => {
            match wkp_core::index::hybrid_search(conn, query, &query_vector, filter) {
                Ok(hits) => return Ok(hits),
                Err(e) => fallback_warning(&e),
            }
        }
        Err(e) => fallback_warning(&e),
    }
    wkp_core::index::search(conn, query, filter).map_err(|e| e.to_string())
}

#[cfg(not(feature = "embed"))]
fn run_hybrid_or_fallback(
    _conn: &wkp_core::index::Connection,
    _query: &str,
    _embed_url: &str,
    _embed_model: Option<&str>,
    _embed_key_file: Option<&Path>,
    _filter: &wkp_core::index::SearchFilter,
) -> Result<Vec<wkp_core::index::SearchHit>, String> {
    Err(
        "this binary was built without hybrid search support (rebuild with `--features embed`); \
         omit --embed-url to use BM25 keyword search"
            .to_string(),
    )
}

struct MaterializeOptions {
    path: PathBuf,
    tier: u8,
}

/// Parses `wkp materialize --tier N [--path DIR]`. `--tier` is required
/// (no default): materializing "whatever tier" by accident is exactly the
/// kind of silent behavior CLAUDE.md's "no shortcuts" list warns against
/// for tier promotion.
fn parse_materialize_args(
    mut args: impl Iterator<Item = String>,
) -> Result<MaterializeOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut tier: Option<u8> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tier" => {
                let v = args.next().ok_or("--tier requires a value")?;
                tier = Some(
                    v.parse::<u8>()
                        .map_err(|_| format!("invalid --tier value: {v}"))?,
                );
            }
            "--path" => {
                let v = args.next().ok_or("--path requires a value")?;
                path = PathBuf::from(v);
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let tier = tier.ok_or_else(|| {
        "materialize requires --tier N, e.g. `wkp materialize --tier 0`".to_string()
    })?;
    Ok(MaterializeOptions { path, tier })
}

/// `wkp materialize --tier N`: writes `.wkp/tier{N}.md` (design 4.2, M1-6),
/// atomically -- a temp file in the same directory, renamed into place
/// (CLAUDE.md hard rule: `tier0.md` is a file a harness reads, same
/// atomicity requirement as `index.db`). Design 4.3's session-start
/// injection target (`cat tier0.md` < 1ms/2ms) is trivially met once this
/// file exists: it's a plain file read, no `wkp` code runs on that path at
/// all.
fn run_materialize(opts: &MaterializeOptions) -> Result<(), String> {
    let index_path = opts.path.join(".wkp/index.db");
    let conn = wkp_core::index::open_index(&index_path).map_err(|e| e.to_string())?;
    let content = wkp_core::index::materialize(&conn, opts.tier).map_err(|e| e.to_string())?;
    let dest = opts.path.join(format!(".wkp/tier{}.md", opts.tier));
    atomic_write(&dest, &content)
}

/// Writes `content` to `dest` via a temp file in the same directory,
/// renamed into place -- never in place (CLAUDE.md hard rule).
fn atomic_write(dest: &Path, content: &str) -> Result<(), String> {
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "materialized.md".to_string());
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dest.with_file_name(format!(".{file_name}.tmp-{pid}-{nanos}"));
    let result = std::fs::write(&tmp, content).map_err(|e| e.to_string());
    match result {
        Ok(()) => std::fs::rename(&tmp, dest).map_err(|e| e.to_string()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Parses `wkp hooks --framework <name>`.
fn parse_hooks_args(mut args: impl Iterator<Item = String>) -> Result<String, String> {
    let mut framework: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--framework" => {
                framework = Some(args.next().ok_or("--framework requires a value")?);
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    framework.ok_or_else(|| {
        "hooks requires --framework <name>, e.g. `wkp hooks --framework claude_code`".to_string()
    })
}

/// `wkp hooks --framework claude_code`: prints the exact Claude Code
/// `SessionStart` hook text (design 3.3: "the only 'installer'... prints
/// the exact hook text for an agent or a human to apply") -- applying it
/// to `.claude/settings.local.json` is left to the caller. Re-indexes
/// quietly and best-effort (`|| true`: a broken index must never block
/// session start), then `cat`s `tier0.md`, also best-effort (a store that
/// hasn't run `wkp materialize` yet has no `tier0.md`, which must not be
/// an error either).
fn render_hooks(framework: &str) -> Result<String, String> {
    match framework {
        "claude_code" => Ok(CLAUDE_CODE_HOOK.to_string()),
        other => Err(format!(
            "unknown --framework value: {other} (expected: claude_code)"
        )),
    }
}

const CLAUDE_CODE_HOOK: &str = r#"{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "wkp index >/dev/null 2>&1 || true; cat .wkp/tier0.md 2>/dev/null || true"
          }
        ]
      }
    ]
  }
}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchFormat {
    Text,
    Paths,
    Json,
}

struct SearchOptions {
    path: PathBuf,
    query: String,
    tier: Option<u8>,
    budget: Option<u32>,
    limit: Option<usize>,
    format: SearchFormat,
    /// M1-9, `wkp search` only (`wkp context` rejects it -- see
    /// `run_context`): opts into hybrid BM25+vector search against this
    /// OpenAI-compatible embeddings endpoint.
    embed_url: Option<String>,
    embed_model: Option<String>,
    embed_key_file: Option<PathBuf>,
}

/// Parses `wkp search <query> [--tier N] [--budget N] [-k/--limit N]
/// [--format text|paths|json] [--path DIR]`. No `clap` dependency yet
/// (M0-1 reserved it as an option, not a requirement): this flag surface
/// is still small enough that hand-rolled parsing is less than adding and
/// vetting a production dependency would cost, per CLAUDE.md's slim-core
/// rule. Revisit once more subcommands make this genuinely unwieldy.
fn parse_search_args(mut args: impl Iterator<Item = String>) -> Result<SearchOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut query: Option<String> = None;
    let mut tier = None;
    let mut budget = None;
    let mut limit = None;
    let mut format = SearchFormat::Text;
    let mut embed_url = None;
    let mut embed_model = None;
    let mut embed_key_file = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--embed-url" => {
                embed_url = Some(args.next().ok_or("--embed-url requires a value")?);
            }
            "--embed-model" => {
                embed_model = Some(args.next().ok_or("--embed-model requires a value")?);
            }
            "--embed-key-file" => {
                embed_key_file = Some(PathBuf::from(
                    args.next().ok_or("--embed-key-file requires a value")?,
                ));
            }
            "--tier" => {
                let v = args.next().ok_or("--tier requires a value")?;
                tier = Some(
                    v.parse::<u8>()
                        .map_err(|_| format!("invalid --tier value: {v}"))?,
                );
            }
            "--budget" => {
                let v = args.next().ok_or("--budget requires a value")?;
                budget = Some(
                    v.parse::<u32>()
                        .map_err(|_| format!("invalid --budget value: {v}"))?,
                );
            }
            "-k" | "--limit" => {
                let v = args.next().ok_or("--limit requires a value")?;
                limit = Some(
                    v.parse::<usize>()
                        .map_err(|_| format!("invalid --limit value: {v}"))?,
                );
            }
            "--format" => {
                let v = args.next().ok_or("--format requires a value")?;
                format = match v.as_str() {
                    "text" => SearchFormat::Text,
                    "paths" => SearchFormat::Paths,
                    "json" => SearchFormat::Json,
                    other => {
                        return Err(format!(
                            "invalid --format value: {other} (expected text|paths|json)"
                        ))
                    }
                };
            }
            "--path" => {
                let v = args.next().ok_or("--path requires a value")?;
                path = PathBuf::from(v);
            }
            other if query.is_none() && !other.starts_with('-') => {
                query = Some(other.to_string());
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    let query =
        query.ok_or_else(|| "search requires a query, e.g. `wkp search \"topic\"`".to_string())?;
    Ok(SearchOptions {
        path,
        query,
        tier,
        budget,
        limit,
        format,
        embed_url,
        embed_model,
        embed_key_file,
    })
}

/// `wkp search <query>`: BM25 full-text search over `index.db` (design
/// 5.3), with tier and token-budget filters. Never touches the network
/// unless `--embed-url` is given (design 5.3 / CLAUDE.md: no embedding
/// calls in the *default* search path) -- with it, hybrid search falls
/// back to plain BM25 on any failure, so `wkp search` itself never fails
/// just because an embedding endpoint is unreachable.
fn run_search(opts: &SearchOptions) -> Result<String, String> {
    let index_path = opts.path.join(".wkp/index.db");
    let conn = wkp_core::index::open_index(&index_path).map_err(|e| e.to_string())?;
    let filter = wkp_core::index::SearchFilter {
        tier: opts.tier,
        budget: opts.budget,
        limit: opts.limit,
        ..Default::default()
    };
    let hits = match &opts.embed_url {
        Some(embed_url) => run_hybrid_or_fallback(
            &conn,
            &opts.query,
            embed_url,
            opts.embed_model.as_deref(),
            opts.embed_key_file.as_deref(),
            &filter,
        )?,
        None => wkp_core::index::search(&conn, &opts.query, &filter).map_err(|e| e.to_string())?,
    };
    Ok(match opts.format {
        SearchFormat::Text => format_text(&hits),
        SearchFormat::Paths => format_paths(&hits),
        SearchFormat::Json => format_json(&hits),
    })
}

/// `wkp context <topic>`: BM25 search plus graph traversal from the top
/// hits (design 5.1/5.4, M1-5), combined under one token budget. Reuses
/// `parse_search_args`/`SearchOptions` -- `context`'s "topic" plays the
/// same positional-argument role as `search`'s "query", and both take the
/// same `--tier`/`--budget`/`--format` flags. `--embed-url` is parsed (it's
/// a `SearchOptions` field) but not supported here -- M1-9 scoped hybrid
/// search to `wkp search` only; combining it with `context`'s own graph
/// traversal is deliberately left to a later task rather than silently
/// ignoring the flag.
fn run_context(opts: &SearchOptions) -> Result<String, String> {
    if opts.embed_url.is_some() {
        return Err(
            "--embed-url is not supported by `wkp context` yet (only `wkp search`)".to_string(),
        );
    }
    let index_path = opts.path.join(".wkp/index.db");
    let conn = wkp_core::index::open_index(&index_path).map_err(|e| e.to_string())?;
    let filter = wkp_core::index::SearchFilter {
        tier: opts.tier,
        budget: opts.budget,
        ..Default::default()
    };
    let hits = wkp_core::index::context(&conn, &opts.query, &filter).map_err(|e| e.to_string())?;
    Ok(match opts.format {
        SearchFormat::Text => format_text(&hits),
        SearchFormat::Paths => format_paths(&hits),
        SearchFormat::Json => format_json(&hits),
    })
}

struct TraverseOptions {
    path: PathBuf,
    start_path: String,
    depth: u32,
    format: SearchFormat,
}

/// Parses `wkp traverse <path> [--depth N] [--format text|paths|json]
/// [--path DIR]`. `--depth` defaults to 2, matching `context`'s own
/// traversal depth from a search hit.
fn parse_traverse_args(mut args: impl Iterator<Item = String>) -> Result<TraverseOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut start_path: Option<String> = None;
    let mut depth: u32 = 2;
    let mut format = SearchFormat::Text;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--depth" => {
                let v = args.next().ok_or("--depth requires a value")?;
                depth = v
                    .parse::<u32>()
                    .map_err(|_| format!("invalid --depth value: {v}"))?;
            }
            "--format" => {
                let v = args.next().ok_or("--format requires a value")?;
                format = match v.as_str() {
                    "text" => SearchFormat::Text,
                    "paths" => SearchFormat::Paths,
                    "json" => SearchFormat::Json,
                    other => {
                        return Err(format!(
                            "invalid --format value: {other} (expected text|paths|json)"
                        ))
                    }
                };
            }
            "--path" => {
                let v = args.next().ok_or("--path requires a value")?;
                path = PathBuf::from(v);
            }
            other if start_path.is_none() && !other.starts_with('-') => {
                start_path = Some(other.to_string());
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let start_path = start_path.ok_or_else(|| {
        "traverse requires a starting path, e.g. `wkp traverse memory/foo.md`".to_string()
    })?;
    Ok(TraverseOptions {
        path,
        start_path,
        depth,
        format,
    })
}

/// `wkp traverse <path>`: follows explicit `refs:`/`[[wikilink]]` edges
/// from `path` up to `--depth` hops (design 5.1, M1-5), without a search
/// query.
fn run_traverse(opts: &TraverseOptions) -> Result<String, String> {
    let index_path = opts.path.join(".wkp/index.db");
    let conn = wkp_core::index::open_index(&index_path).map_err(|e| e.to_string())?;
    let hits = wkp_core::index::traverse(&conn, &opts.start_path, opts.depth)
        .map_err(|e| e.to_string())?;
    Ok(match opts.format {
        SearchFormat::Text => format_text(&hits),
        SearchFormat::Paths => format_paths(&hits),
        SearchFormat::Json => format_json(&hits),
    })
}

fn format_text(hits: &[wkp_core::index::SearchHit]) -> String {
    if hits.is_empty() {
        return "(no results)".to_string();
    }
    hits.iter()
        .map(|h| {
            let hop = if h.hop_distance > 0 {
                format!(" +{}", h.hop_distance)
            } else {
                String::new()
            };
            format!(
                "[T{}{hop}] {}  (score={:.3}, ~{}t)\n     {}",
                h.tier, h.title, h.score, h.tokens, h.path
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_paths(hits: &[wkp_core::index::SearchHit]) -> String {
    hits.iter()
        .map(|h| h.path.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A stable, documented contract (per the M1-4 acceptance criteria): each
/// object has exactly the fields `path` (string), `title` (string),
/// `score` (number), `tier` (integer), `tokens` (integer, estimated),
/// `hop_distance` (integer, `0` for a direct search match, M1-5). Hand-rolled
/// rather than pulling in `serde_json` for five scalar fields — see
/// `parse_search_args`'s doc comment on the same slim-core reasoning for
/// not adding `clap` yet.
fn format_json(hits: &[wkp_core::index::SearchHit]) -> String {
    let items: Vec<String> = hits
        .iter()
        .map(|h| {
            format!(
                r#"{{"path":{},"title":{},"score":{},"tier":{},"tokens":{},"hop_distance":{}}}"#,
                json_string(&h.path),
                json_string(&h.title),
                h.score,
                h.tier,
                h.tokens,
                h.hop_distance
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Appends `patterns` to `<path>/.gitignore`, skipping any pattern already
/// present as an exact line. `index.db` and `tier0.md` are derived
/// artifacts a harness reads (CLAUDE.md: written via temp-file-then-rename,
/// never synced) and must never be committed to the store.
fn ensure_gitignored(path: &Path, patterns: &[&str]) -> Result<(), String> {
    let gitignore_path = path.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore_path).unwrap_or_default();
    let existing_lines: std::collections::HashSet<&str> = existing.lines().collect();

    let missing: Vec<&&str> = patterns
        .iter()
        .filter(|p| !existing_lines.contains(*p))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    for pattern in missing {
        updated.push_str(pattern);
        updated.push('\n');
    }
    std::fs::write(&gitignore_path, updated).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tempfile` rather than `std::env::temp_dir()` + a predictable name:
    /// the latter is flagged by this repo's semgrep gate as an
    /// insecure-temp-file pattern (a shared temp directory with a
    /// guessable name invites symlink/TOCTOU races).
    fn temp_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("wkp-cli-test-{name}-"))
            .tempdir()
            .expect("create temp dir")
    }

    /// `run_init`, but with an empty, guaranteed-`projects/`-less
    /// `claude_home` instead of the real `$HOME/.claude` -- otherwise
    /// every test calling this would non-deterministically import
    /// whatever real Claude Code memory files happen to exist on the
    /// machine running the test suite (this repo's own memory files,
    /// found the hard way when `claude_home_from_env`'s `$HOME` vs.
    /// `$HOME/.claude` bug was fixed and previously-passing tests started
    /// failing because real content suddenly showed up in `inbox/import/`).
    fn test_init(dir: &Path) -> Result<(), String> {
        let empty_claude_home = temp_dir("test-init-empty-claude-home");
        run_init_with_claude_home(dir, Some(empty_claude_home.path()))
    }

    #[test]
    fn run_init_creates_store_with_gitignored_index() {
        let temp = temp_dir("run-init");
        let dir = temp.path();
        test_init(dir).expect("run_init");

        assert!(dir.join(".git").is_dir());
        assert!(dir.join(".wkp/index.db").is_file());

        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).expect("read .gitignore");
        assert!(gitignore.contains(".wkp/index.db"));
        assert!(gitignore.contains(".wkp/tier0.md"));

        // The freshly built index is a valid, queryable database.
        let conn =
            wkp_core::index::open_index(&dir.join(".wkp/index.db")).expect("open fresh index");
        let hits =
            wkp_core::index::search(&conn, "anything", &wkp_core::index::SearchFilter::default())
                .expect("search empty index");
        assert!(hits.is_empty());
    }

    /// M2-1 acceptance criterion: `wkp init` wires up SSH commit signing --
    /// the store carries an `allowed_signers` file (not gitignored, unlike
    /// `.wkp/index.db`/`tier0.md`: this one needs to sync with the store)
    /// and local git config points `gpg.ssh.allowedSignersFile` at it.
    #[test]
    fn run_init_wires_ssh_signing_config() {
        let temp = temp_dir("run-init-ssh-signing");
        let dir = temp.path();
        test_init(dir).expect("run_init");

        assert!(dir.join("allowed_signers").is_file());

        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).expect("read .gitignore");
        assert!(
            !gitignore.contains("allowed_signers"),
            "allowed_signers must be tracked, not gitignored -- the store carries it"
        );
    }

    #[test]
    fn run_init_is_idempotent_and_preserves_existing_gitignore_entries() {
        let temp = temp_dir("run-init-idempotent");
        let dir = temp.path();
        std::fs::write(dir.join(".gitignore"), "target/\n").expect("seed .gitignore");

        test_init(dir).expect("first run_init");
        test_init(dir).expect("second run_init");

        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).expect("read .gitignore");
        assert_eq!(gitignore.matches(".wkp/index.db").count(), 1);
        assert!(gitignore.contains("target/"));
    }

    fn search_paths(dir: &Path, query: &str) -> Vec<String> {
        let conn = wkp_core::index::open_index(&dir.join(".wkp/index.db")).expect("open index");
        wkp_core::index::search(&conn, query, &wkp_core::index::SearchFilter::default())
            .expect("search")
            .into_iter()
            .map(|h| h.path)
            .collect()
    }

    #[test]
    fn run_index_only_touches_changed_markdown_files() {
        let temp = temp_dir("run-index");
        let dir = temp.path();
        test_init(dir).expect("run_init");

        std::fs::write(dir.join("a.md"), "---\ntitle: A\n---\n\nfirst content\n")
            .expect("write a.md");
        std::fs::write(dir.join("b.md"), "---\ntitle: B\n---\n\nsecond content\n")
            .expect("write b.md");
        // Non-markdown files in the store must never reach the index.
        std::fs::write(dir.join("notes.txt"), "not markdown\n").expect("write notes.txt");
        wkp_git::commit_all(dir, "seed").expect("commit_all");

        let summary = run_index(dir).expect("first run_index");
        assert_eq!(summary.added, 2);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.deleted, 0);

        let mut paths = search_paths(dir, "content");
        paths.sort_unstable();
        assert_eq!(paths, vec!["a.md", "b.md"]);
        assert!(search_paths(dir, "markdown").is_empty());

        // A second run with nothing changed touches nothing.
        let summary = run_index(dir).expect("second run_index");
        assert_eq!(summary.added, 0);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.deleted, 0);
    }

    #[test]
    fn run_index_handles_modify_and_delete() {
        let temp = temp_dir("run-index-modify-delete");
        let dir = temp.path();
        test_init(dir).expect("run_init");

        std::fs::write(dir.join("a.md"), "---\ntitle: A\n---\n\noriginal\n").expect("write a.md");
        std::fs::write(dir.join("b.md"), "---\ntitle: B\n---\n\nkept\n").expect("write b.md");
        wkp_git::commit_all(dir, "seed").expect("commit_all");
        run_index(dir).expect("initial index");

        std::fs::write(dir.join("a.md"), "---\ntitle: A\n---\n\nupdated\n").expect("modify a.md");
        std::fs::remove_file(dir.join("b.md")).expect("delete b.md");

        let summary = run_index(dir).expect("second run_index");
        assert_eq!(summary.added, 0);
        assert_eq!(summary.modified, 1);
        assert_eq!(summary.deleted, 1);

        assert_eq!(search_paths(dir, "updated"), vec!["a.md"]);
        assert!(search_paths(dir, "original").is_empty());
        assert!(search_paths(dir, "kept").is_empty());
    }

    fn args(parts: &[&str]) -> impl Iterator<Item = String> {
        parts
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parse_search_args_reads_query_and_flags() {
        let opts = parse_search_args(args(&[
            "topic", "--tier", "1", "--budget", "500", "-k", "5", "--format", "json",
        ]))
        .expect("parse_search_args");
        assert_eq!(opts.query, "topic");
        assert_eq!(opts.tier, Some(1));
        assert_eq!(opts.budget, Some(500));
        assert_eq!(opts.limit, Some(5));
        assert_eq!(opts.format, SearchFormat::Json);
    }

    #[test]
    fn parse_search_args_defaults_to_text_format_and_no_filters() {
        let opts = parse_search_args(args(&["topic"])).expect("parse_search_args");
        assert_eq!(opts.format, SearchFormat::Text);
        assert_eq!(opts.tier, None);
        assert_eq!(opts.budget, None);
        assert_eq!(opts.limit, None);
    }

    #[test]
    fn parse_search_args_rejects_missing_query() {
        let result = parse_search_args(args(&["--tier", "1"]));
        assert!(result.is_err());
    }

    #[test]
    fn parse_search_args_rejects_invalid_format() {
        let result = parse_search_args(args(&["topic", "--format", "xml"]));
        assert!(result.is_err());
    }

    #[test]
    fn parse_search_args_reads_embed_flags() {
        let opts = parse_search_args(args(&[
            "topic",
            "--embed-url",
            "http://localhost:11434/v1",
            "--embed-model",
            "nomic-embed-text",
            "--embed-key-file",
            "/tmp/key",
        ]))
        .expect("parse_search_args");
        assert_eq!(opts.embed_url.as_deref(), Some("http://localhost:11434/v1"));
        assert_eq!(opts.embed_model.as_deref(), Some("nomic-embed-text"));
        assert_eq!(opts.embed_key_file, Some(PathBuf::from("/tmp/key")));
    }

    #[test]
    fn parse_search_args_defaults_embed_flags_to_none() {
        let opts = parse_search_args(args(&["topic"])).expect("parse_search_args");
        assert_eq!(opts.embed_url, None);
        assert_eq!(opts.embed_model, None);
        assert_eq!(opts.embed_key_file, None);
    }

    #[test]
    fn parse_index_args_reads_path_and_embed_flags() {
        let opts = parse_index_args(args(&[
            "/tmp/store",
            "--embed-url",
            "http://localhost:11434/v1",
            "--embed-model",
            "m",
            "--embed-key-file",
            "/tmp/key",
        ]))
        .expect("parse_index_args");
        assert_eq!(opts.path, PathBuf::from("/tmp/store"));
        assert_eq!(opts.embed_url.as_deref(), Some("http://localhost:11434/v1"));
        assert_eq!(opts.embed_model.as_deref(), Some("m"));
        assert_eq!(opts.embed_key_file, Some(PathBuf::from("/tmp/key")));
    }

    #[test]
    fn parse_index_args_defaults_to_cwd_and_no_embed_flags() {
        let opts = parse_index_args(args(&[])).expect("parse_index_args");
        assert_eq!(opts.embed_url, None);
        assert_eq!(opts.embed_model, None);
        assert_eq!(opts.embed_key_file, None);
    }

    #[test]
    fn run_context_rejects_embed_url() {
        let temp = temp_dir("context-rejects-embed-url");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let mut opts = search_opts(dir, "anything", SearchFormat::Text);
        opts.embed_url = Some("http://localhost:11434/v1".to_string());
        let result = run_context(&opts);
        assert!(
            result.is_err(),
            "wkp context must reject --embed-url (M1-9 scoped hybrid search to wkp search only)"
        );
    }

    /// The default (no `embed` feature) build must still parse and accept
    /// `--embed-url` at the CLI-argument level -- it fails later, inside
    /// `run_hybrid_or_fallback`/`embed_upserts`, with a clear message,
    /// rather than at argument parsing with "unrecognized argument".
    #[test]
    #[cfg(not(feature = "embed"))]
    fn run_search_with_embed_url_on_a_non_embed_build_fails_clearly_not_as_unrecognized_arg() {
        let temp = temp_dir("search-embed-url-no-feature");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let mut opts = search_opts(dir, "anything", SearchFormat::Text);
        opts.embed_url = Some("http://localhost:11434/v1".to_string());
        let err = run_search(&opts).expect_err("expected a clear feature-not-built error");
        assert!(
            err.contains("--features embed"),
            "expected a rebuild-with-features message, got: {err}"
        );
    }

    fn search_opts(dir: &Path, query: &str, format: SearchFormat) -> SearchOptions {
        SearchOptions {
            path: dir.to_path_buf(),
            query: query.to_string(),
            tier: None,
            budget: None,
            limit: None,
            format,
            embed_url: None,
            embed_model: None,
            embed_key_file: None,
        }
    }

    #[test]
    fn run_search_all_three_formats() {
        let temp = temp_dir("run-search");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::write(
            dir.join("a.md"),
            "---\ntitle: A Title\ntype: knowledge\n---\n\nfindable content\n",
        )
        .expect("write a.md");
        wkp_git::commit_all(dir, "seed").expect("commit_all");
        run_index(dir).expect("run_index");

        let output = run_search(&search_opts(dir, "findable", SearchFormat::Paths))
            .expect("run_search paths");
        assert_eq!(output, "a.md");

        let output =
            run_search(&search_opts(dir, "findable", SearchFormat::Text)).expect("run_search text");
        assert!(output.contains("A Title"));
        assert!(output.contains("a.md"));
        assert!(output.starts_with("[T"));

        let output =
            run_search(&search_opts(dir, "findable", SearchFormat::Json)).expect("run_search json");
        assert!(output.starts_with('['));
        assert!(output.contains(r#""path":"a.md""#));
        assert!(output.contains(r#""title":"A Title""#));
    }

    #[test]
    fn run_search_empty_results_per_format() {
        let temp = temp_dir("run-search-empty");
        let dir = temp.path();
        test_init(dir).expect("run_init");

        let query = "nothing matches this";
        assert_eq!(
            run_search(&search_opts(dir, query, SearchFormat::Text)).expect("text"),
            "(no results)"
        );
        assert_eq!(
            run_search(&search_opts(dir, query, SearchFormat::Paths)).expect("paths"),
            ""
        );
        assert_eq!(
            run_search(&search_opts(dir, query, SearchFormat::Json)).expect("json"),
            "[]"
        );
    }

    #[test]
    fn json_string_escapes_quotes_and_control_characters() {
        assert_eq!(json_string("plain"), "\"plain\"");
        assert_eq!(json_string("has \"quotes\""), "\"has \\\"quotes\\\"\"");
        assert_eq!(json_string("line\nbreak"), "\"line\\nbreak\"");
    }

    #[test]
    fn run_context_combines_search_and_traversal() {
        let temp = temp_dir("run-context");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::write(
            dir.join("hit.md"),
            "---\ntitle: Hit\ntype: knowledge\nrefs: [neighbor.md]\n---\n\ndistinctive topic\n",
        )
        .expect("write hit.md");
        std::fs::write(
            dir.join("neighbor.md"),
            "---\ntitle: Neighbor\n---\n\nreached only via refs\n",
        )
        .expect("write neighbor.md");
        wkp_git::commit_all(dir, "seed").expect("commit_all");
        run_index(dir).expect("run_index");

        let output = run_context(&search_opts(dir, "distinctive", SearchFormat::Paths))
            .expect("run_context");
        let mut paths: Vec<&str> = output.lines().collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["hit.md", "neighbor.md"]);
    }

    #[test]
    fn parse_traverse_args_reads_path_and_flags() {
        let opts = parse_traverse_args(args(&["memory/a.md", "--depth", "3", "--format", "json"]))
            .expect("parse_traverse_args");
        assert_eq!(opts.start_path, "memory/a.md");
        assert_eq!(opts.depth, 3);
        assert_eq!(opts.format, SearchFormat::Json);
    }

    #[test]
    fn parse_traverse_args_defaults_depth_to_two() {
        let opts = parse_traverse_args(args(&["a.md"])).expect("parse_traverse_args");
        assert_eq!(opts.depth, 2);
        assert_eq!(opts.format, SearchFormat::Text);
    }

    #[test]
    fn parse_traverse_args_rejects_missing_path() {
        assert!(parse_traverse_args(args(&["--depth", "1"])).is_err());
    }

    #[test]
    fn run_traverse_follows_refs_and_reports_hop_distance() {
        let temp = temp_dir("run-traverse");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::write(
            dir.join("a.md"),
            "---\ntitle: A\nrefs: [b.md]\n---\n\nstart\n",
        )
        .expect("write a.md");
        std::fs::write(dir.join("b.md"), "---\ntitle: B\n---\n\ntarget\n").expect("write b.md");
        wkp_git::commit_all(dir, "seed").expect("commit_all");
        run_index(dir).expect("run_index");

        let opts = TraverseOptions {
            path: dir.to_path_buf(),
            start_path: "a.md".to_string(),
            depth: 1,
            format: SearchFormat::Text,
        };
        let output = run_traverse(&opts).expect("run_traverse");
        assert!(
            output.contains("+1"),
            "expected hop distance in output: {output}"
        );
        assert!(output.contains("b.md"));

        let json_opts = TraverseOptions {
            format: SearchFormat::Json,
            ..TraverseOptions {
                path: dir.to_path_buf(),
                start_path: "a.md".to_string(),
                depth: 1,
                format: SearchFormat::Text,
            }
        };
        let json_output = run_traverse(&json_opts).expect("run_traverse json");
        assert!(json_output.contains(r#""hop_distance":1"#));
    }

    /// Golden test (M1-5/M1-8 groundwork): locks down the exact text-format
    /// shape so a future change to it is a deliberate, reviewed diff, not
    /// an accident. Update this string deliberately if the format changes.
    #[test]
    fn text_format_output_shape_is_locked_down() {
        let mut hit = wkp_core::index::SearchHit {
            path: "a.md".to_string(),
            title: "A Title".to_string(),
            score: 0.5,
            tier: 1,
            tokens: 42,
            hop_distance: 0,
        };
        let neighbor = wkp_core::index::SearchHit {
            path: "b.md".to_string(),
            title: "B Title".to_string(),
            score: 0.25,
            tier: 2,
            tokens: 7,
            hop_distance: 1,
        };
        let rendered = format_text(std::slice::from_ref(&hit));
        assert_eq!(rendered, "[T1] A Title  (score=0.500, ~42t)\n     a.md");

        hit.hop_distance = 0;
        let rendered_pair = format_text(&[hit, neighbor]);
        assert_eq!(
            rendered_pair,
            "[T1] A Title  (score=0.500, ~42t)\n     a.md\n\
             [T2 +1] B Title  (score=0.250, ~7t)\n     b.md"
        );
    }

    #[test]
    fn parse_materialize_args_requires_tier() {
        assert!(parse_materialize_args(args(&[])).is_err());
    }

    #[test]
    fn parse_materialize_args_reads_tier_and_path() {
        let opts = parse_materialize_args(args(&["--tier", "1", "--path", "/tmp/x"]))
            .expect("parse_materialize_args");
        assert_eq!(opts.tier, 1);
        assert_eq!(opts.path, PathBuf::from("/tmp/x"));
    }

    #[test]
    fn run_materialize_writes_tier0_and_tier1_content() {
        let temp = temp_dir("materialize");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::write(
            dir.join("a.md"),
            "---\ntitle: A\ntype: project-state\n---\n\ntier zero body\n",
        )
        .expect("write a.md");
        std::fs::write(
            dir.join("b.md"),
            "---\ntitle: B\ntype: feedback\n---\n\ntier one body\n",
        )
        .expect("write b.md");
        // M2-6: tier 0/1 now requires a human-signed commit, not just
        // the right `type:` -- an unsigned `commit_all` would leave
        // both items at tier 2 regardless of frontmatter.
        let key = generate_test_key_and_register(dir, "human:alice");
        wkp_git::signed_commit::signed_commit(
            dir,
            &[PathBuf::from("a.md"), PathBuf::from("b.md")],
            "seed",
            "human:alice",
            &key.private_path,
            &wkp_git::provenance::Provenance::default(),
        )
        .expect("signed_commit");
        run_index(dir).expect("run_index");

        run_materialize(&MaterializeOptions {
            path: dir.to_path_buf(),
            tier: 0,
        })
        .expect("materialize tier 0");
        let tier0 = std::fs::read_to_string(dir.join(".wkp/tier0.md")).expect("read tier0.md");
        assert!(tier0.starts_with("<wkp-context tier=\"0\">"));
        assert!(tier0.contains("tier zero body"));
        assert!(!tier0.contains("tier one body"));

        run_materialize(&MaterializeOptions {
            path: dir.to_path_buf(),
            tier: 1,
        })
        .expect("materialize tier 1");
        let tier1 = std::fs::read_to_string(dir.join(".wkp/tier1.md")).expect("read tier1.md");
        assert!(tier1.contains("tier one body"));
        assert!(!tier1.contains("tier zero body"));
    }

    #[test]
    fn run_materialize_never_includes_inbox_items() {
        let temp = temp_dir("materialize-inbox");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::create_dir_all(dir.join("inbox/import")).expect("create inbox dir");
        std::fs::write(
            dir.join("inbox/import/claude-md.md"),
            "---\ntitle: Imported\ntype: project-state\nconfidence: proposed\n---\n\nshould never be auto-injected\n",
        )
        .expect("write inbox item");
        wkp_git::commit_all(dir, "seed").expect("commit_all");
        run_index(dir).expect("run_index");

        run_materialize(&MaterializeOptions {
            path: dir.to_path_buf(),
            tier: 0,
        })
        .expect("materialize tier 0");
        let tier0 = std::fs::read_to_string(dir.join(".wkp/tier0.md")).expect("read tier0.md");
        assert!(!tier0.contains("should never be auto-injected"));
    }

    #[test]
    fn run_materialize_is_atomic_on_a_failed_write() {
        let temp = temp_dir("materialize-atomic");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        run_materialize(&MaterializeOptions {
            path: dir.to_path_buf(),
            tier: 0,
        })
        .expect("initial materialize");
        let original = std::fs::read(dir.join(".wkp/tier0.md")).expect("read original tier0.md");

        // Force the temp-file write to fail: make the .wkp/ directory
        // read-only so `atomic_write`'s initial `fs::write` to a new temp
        // path inside it cannot succeed. Restore the original mode
        // explicitly afterward rather than `set_readonly(false)`, which on
        // Unix would leave the directory world-writable (0o777).
        use std::os::unix::fs::PermissionsExt;
        let wkp_dir = dir.join(".wkp");
        let original_mode = std::fs::metadata(&wkp_dir).unwrap().permissions().mode();
        std::fs::set_permissions(&wkp_dir, std::fs::Permissions::from_mode(0o555))
            .expect("make .wkp/ read-only");

        let result = run_materialize(&MaterializeOptions {
            path: dir.to_path_buf(),
            tier: 0,
        });

        std::fs::set_permissions(&wkp_dir, std::fs::Permissions::from_mode(original_mode))
            .expect("restore .wkp/ original permissions");

        assert!(
            result.is_err(),
            "expected the write to a read-only dir to fail"
        );
        let after = std::fs::read(dir.join(".wkp/tier0.md")).expect("read tier0.md after failure");
        assert_eq!(
            original, after,
            "a failed materialize must not touch the existing tier0.md"
        );
    }

    #[test]
    fn render_hooks_claude_code_matches_golden_output() {
        let output = render_hooks("claude_code").expect("render_hooks");
        assert_eq!(output, CLAUDE_CODE_HOOK);
        assert!(output.contains("SessionStart"));
        assert!(output.contains("wkp index"));
        assert!(output.contains("tier0.md"));
    }

    #[test]
    fn render_hooks_rejects_unknown_framework() {
        assert!(render_hooks("opencode").is_err());
    }

    #[test]
    fn hooks_never_writes_any_file() {
        let temp = temp_dir("hooks-no-write");
        let dir = temp.path();
        let before = std::fs::read_dir(dir).unwrap().count();

        // render_hooks takes no path at all, so there is structurally
        // nothing for it to write to; this asserts the observable
        // consequence (no new file appears anywhere near it) rather than
        // relying solely on the type signature.
        let _ = render_hooks("claude_code");

        let after = std::fs::read_dir(dir).unwrap().count();
        assert_eq!(before, after, "wkp hooks must never write a file");
    }

    #[test]
    fn claude_home_dir_joins_dot_claude_onto_home() {
        // Regression test: the real CLI wiring once passed raw $HOME
        // straight to run_import, silently finding zero memory files on
        // every real invocation (caught only by testing the actual
        // binary, not by unit tests calling run_import directly).
        assert_eq!(
            claude_home_dir(std::ffi::OsStr::new("/home/alice")),
            PathBuf::from("/home/alice/.claude")
        );
    }

    #[test]
    fn run_import_net_new_store_imports_nothing_without_error() {
        let temp = temp_dir("import-net-new");
        let dir = temp.path();
        std::fs::create_dir_all(dir).expect("create dir");
        let claude_home = temp_dir("import-net-new-home"); // exists but has no projects/

        let summary = run_import(dir, Some(claude_home.path())).expect("run_import");
        assert_eq!(summary.imported, 0);
        assert_eq!(summary.skipped, 0);
    }

    #[test]
    fn run_import_with_no_claude_home_at_all_imports_nothing_without_error() {
        let temp = temp_dir("import-no-home");
        let dir = temp.path();
        std::fs::create_dir_all(dir).expect("create dir");

        let summary = run_import(dir, None).expect("run_import with claude_home=None");
        assert_eq!(summary.imported, 0);
        assert_eq!(summary.skipped, 0);
    }

    #[test]
    fn run_import_wraps_claude_md_and_agents_md_as_instruction_type() {
        let temp = temp_dir("import-claude-agents-md");
        let dir = temp.path();
        std::fs::create_dir_all(dir).expect("create dir");
        std::fs::write(
            dir.join("CLAUDE.md"),
            "# Working agreement\n\nDo X, not Y.\n",
        )
        .expect("write CLAUDE.md");
        std::fs::write(
            dir.join("AGENTS.md"),
            "# Agent instructions\n\nCall wkp search.\n",
        )
        .expect("write AGENTS.md");

        let summary = run_import(dir, None).expect("run_import");
        assert_eq!(summary.imported, 2);
        assert_eq!(summary.skipped, 0);

        let claude_imported = std::fs::read_to_string(dir.join("inbox/import/claude-md.md"))
            .expect("read imported CLAUDE.md");
        assert!(claude_imported.contains("type: instruction"));
        assert!(claude_imported.contains("confidence: proposed"));
        assert!(claude_imported.contains("source: import"));
        assert!(claude_imported.contains("Do X, not Y."));

        assert!(dir.join("inbox/import/agents-md.md").is_file());
    }

    #[test]
    fn run_import_is_idempotent_and_does_not_overwrite_an_edited_copy() {
        let temp = temp_dir("import-idempotent");
        let dir = temp.path();
        std::fs::create_dir_all(dir).expect("create dir");
        std::fs::write(dir.join("CLAUDE.md"), "original content\n").expect("write CLAUDE.md");

        let first = run_import(dir, None).expect("first run_import");
        assert_eq!(first.imported, 1);

        // Simulate a human having edited their imported copy.
        std::fs::write(dir.join("inbox/import/claude-md.md"), "human-edited copy\n")
            .expect("simulate human edit");

        let second = run_import(dir, None).expect("second run_import");
        assert_eq!(second.imported, 0);
        assert_eq!(second.skipped, 1);
        let content =
            std::fs::read_to_string(dir.join("inbox/import/claude-md.md")).expect("read file");
        assert_eq!(
            content, "human-edited copy\n",
            "must not clobber an edited import"
        );
    }

    #[test]
    fn run_import_maps_claude_code_memory_frontmatter_to_okf() {
        let temp = temp_dir("import-claude-memory");
        let dir = temp.path();
        std::fs::create_dir_all(dir).expect("create dir");
        let claude_home = temp_dir("import-claude-memory-home");
        let memory_dir = claude_home.path().join("projects/some-project/memory");
        std::fs::create_dir_all(&memory_dir).expect("create memory dir");
        std::fs::write(
            memory_dir.join("project_kickoff.md"),
            "---\nname: project-kickoff\ndescription: \"A project note\"\nmetadata:\n  node_type: memory\n  type: project\n  modified: 2026-09-07T15:38:16.314Z\n---\n\nSome project content here.\n",
        )
        .expect("write claude memory file");

        let summary = run_import(dir, Some(claude_home.path())).expect("run_import");
        assert_eq!(summary.imported, 1);

        let imported =
            std::fs::read_to_string(dir.join("inbox/import/claude-memory-project_kickoff.md"))
                .expect("read imported memory file");
        assert!(imported.contains("title: \"A project note\""));
        assert!(imported.contains("type: project-state"));
        assert!(imported.contains("scope: project"));
        assert!(imported.contains("confidence: proposed"));
        assert!(imported.contains("Some project content here."));
    }

    #[test]
    fn run_import_falls_back_gracefully_on_a_claude_memory_file_with_no_frontmatter() {
        let temp = temp_dir("import-claude-memory-no-frontmatter");
        let dir = temp.path();
        std::fs::create_dir_all(dir).expect("create dir");
        let claude_home = temp_dir("import-claude-memory-no-frontmatter-home");
        let memory_dir = claude_home.path().join("projects/p/memory");
        std::fs::create_dir_all(&memory_dir).expect("create memory dir");
        std::fs::write(
            memory_dir.join("plain.md"),
            "just plain content, no frontmatter\n",
        )
        .expect("write plain memory file");

        let summary = run_import(dir, Some(claude_home.path())).expect("run_import");
        assert_eq!(summary.imported, 1);
        let imported = std::fs::read_to_string(dir.join("inbox/import/claude-memory-plain.md"))
            .expect("read imported file");
        assert!(imported.contains("just plain content"));
        assert!(imported.contains("confidence: proposed"));
    }

    /// M1-7's "start from scratch, or import from previous versions, or
    /// net new" scope: a v0-python-style store (frontmatter with only the
    /// original OKF fields -- no `provenance`/`confidence`/`scope`, which
    /// didn't exist in that tool) must index and search correctly under
    /// this tool without any special-case migration code. Verifies the
    /// compatibility CLAUDE.md already promises ("agent-wkp's OKF
    /// frontmatter stays as-is for compatibility") rather than just
    /// asserting it.
    #[test]
    fn v0_python_style_frontmatter_indexes_and_searches_correctly() {
        let temp = temp_dir("v0-python-compat");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::create_dir_all(dir.join("memory")).expect("create memory dir");
        std::fs::write(
            dir.join("memory/old_style_note.md"),
            "---\ntitle: Old Style Note\ntype: knowledge\nworkspace: eval-hub\nvisibility: shared\ntokens: 42\ntags: [legacy]\nrefs: []\nupdated: 2025-01-01\n---\n\nThis note predates the provenance and confidence fields entirely.\n",
        )
        .expect("write v0-python-style file");
        wkp_git::commit_all(dir, "seed").expect("commit_all");

        let summary = run_index(dir).expect("run_index on a v0-python-style store");
        assert_eq!(summary.added, 1);

        let hits = search_paths(dir, "predates");
        assert_eq!(hits, vec!["memory/old_style_note.md"]);
    }

    #[cfg(feature = "embed")]
    #[test]
    fn read_embed_key_file_rejects_a_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let temp = temp_dir("embed-key-file-perms");
        let path = temp.path().join("key");
        std::fs::write(&path, "secret\n").expect("write key file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod 644");
        let err = read_embed_key_file(&path).expect_err("expected a permission error");
        assert!(err.contains("0600"), "expected a 0600 hint, got: {err}");
    }

    #[cfg(feature = "embed")]
    #[test]
    fn read_embed_key_file_reads_a_0600_file_and_trims_whitespace() {
        use std::os::unix::fs::PermissionsExt;
        let temp = temp_dir("embed-key-file-ok");
        let path = temp.path().join("key");
        std::fs::write(&path, "secret-key\n").expect("write key file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
        let key = read_embed_key_file(&path).expect("read_embed_key_file");
        assert_eq!(key, "secret-key");
    }

    /// A minimal, hand-rolled HTTP/1.1 server on loopback that answers
    /// `connections` requests with the same fixed embedding response, then
    /// stops -- exercises `run_index_cli`/`run_search`'s real network path
    /// end to end without a real network call or a mocking dependency.
    #[cfg(feature = "embed")]
    fn serve_fixed_embedding(response_json: &'static str, connections: usize) -> String {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("local_addr");
        std::thread::spawn(move || {
            for stream in listener.incoming().take(connections) {
                let stream = stream.expect("accept connection");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request_line = String::new();
                reader
                    .read_line(&mut request_line)
                    .expect("read request line");
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read header line");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let mut stream = stream;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_json.len(),
                    response_json
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });
        format!("http://{addr}")
    }

    /// End-to-end (M1-9): `wkp index --embed-url` stores an embedding as
    /// part of the same atomic write as everything else, and `wkp search
    /// --embed-url` then uses it via `hybrid_search` instead of falling
    /// back to BM25 -- exercised through the same `run_index_cli`/
    /// `run_search` entry points the CLI dispatch calls, against a real
    /// (loopback) HTTP server, not just the pure-Rust fusion math already
    /// covered in `wkp-core`.
    #[cfg(feature = "embed")]
    #[test]
    fn end_to_end_index_and_search_with_embed_url_uses_hybrid_search() {
        let temp = temp_dir("embed-end-to-end");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::write(
            dir.join("a.md"),
            "---\ntitle: A\ntype: knowledge\n---\n\nfindable content\n",
        )
        .expect("write a.md");
        wkp_git::commit_all(dir, "seed").expect("commit_all");

        // One embedding request from `wkp index`, one more from `wkp
        // search`'s own query embedding.
        let url = serve_fixed_embedding(r#"{"data":[{"embedding":[1.0,0.0,0.0]}]}"#, 2);

        let index_opts = IndexOptions {
            path: dir.to_path_buf(),
            embed_url: Some(url.clone()),
            embed_model: None,
            embed_key_file: None,
        };
        let summary = run_index_cli(&index_opts).expect("run_index_cli with --embed-url");
        assert_eq!(summary.added, 1);

        let mut search_options = search_opts(dir, "findable", SearchFormat::Paths);
        search_options.embed_url = Some(url);
        let output = run_search(&search_options).expect("run_search with --embed-url");
        assert_eq!(output, "a.md");
    }

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

    /// A throwaway, passphrase-less ed25519 keypair for tests -- same
    /// approach `wkp-git`'s own `signed_commit` tests use, duplicated
    /// here rather than shared: there is no common test-utility crate,
    /// and this is a handful of lines.
    struct TestKey {
        private_path: PathBuf,
    }

    fn generate_test_key_and_register(dir: &Path, principal: &str) -> TestKey {
        let private_path = dir.join("test-key");
        let status = std::process::Command::new("ssh-keygen")
            .args([
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "test",
                "-f",
                &private_path.to_string_lossy(),
                "-q",
            ])
            .status()
            .expect("run ssh-keygen");
        assert!(status.success(), "ssh-keygen failed");
        let public_line = std::fs::read_to_string(dir.join("test-key.pub"))
            .expect("read generated public key")
            .trim()
            .to_string();
        let mut fields = public_line.split_whitespace();
        let key_type = fields.next().expect("key type field").to_string();
        let key_base64 = fields.next().expect("base64 field").to_string();

        wkp_git::allowed_signers::append(
            &dir.join("allowed_signers"),
            &wkp_git::allowed_signers::SignerEntry {
                principal: principal.to_string(),
                role: wkp_git::allowed_signers::SignerRole::Agent,
                key_type,
                key_base64,
            },
        )
        .expect("append signer entry");

        TestKey { private_path }
    }

    fn remember_opts(dir: &Path, key: &TestKey, item_type: &str, title: &str) -> RememberOptions {
        RememberOptions {
            path: dir.to_path_buf(),
            item_type: item_type.to_string(),
            scope: Some("project".to_string()),
            title: Some(title.to_string()),
            principal: "agent:claude-code@host".to_string(),
            signing_key_file: key.private_path.clone(),
            session: Some("sess-abc".to_string()),
        }
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
