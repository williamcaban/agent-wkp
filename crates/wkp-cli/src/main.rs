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
            let path = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().expect("wkp: cannot read cwd"));
            match run_index(&path) {
                Ok(summary) => println!("{summary}"),
                Err(msg) => {
                    eprintln!("wkp: index failed: {msg}");
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
    wkp_git::init_repo(path)?;
    wkp_git::apply_init_settings(path)?;

    let wkp_dir = path.join(".wkp");
    std::fs::create_dir_all(&wkp_dir).map_err(|e| e.to_string())?;

    ensure_gitignored(path, &[".wkp/index.db", ".wkp/tier0.md"])?;

    // An empty index at this point; `wkp index` (M1-3) populates it from
    // the store's markdown files.
    wkp_core::index::build_index(&wkp_dir.join("index.db"), &[]).map_err(|e| e.to_string())?;

    Ok(())
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
fn run_index(path: &Path) -> Result<IndexSummary, String> {
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
        upserts.push(wkp_core::index::Item {
            path: (*relative).clone(),
            frontmatter: parsed.frontmatter,
            body: parsed.body,
        });
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

fn path_to_store_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
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

    while let Some(arg) = args.next() {
        match arg.as_str() {
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
    })
}

/// `wkp search <query>`: BM25 full-text search over `index.db` (design
/// 5.3), with tier and token-budget filters. Never touches the network
/// (design 5.3 / CLAUDE.md: no embedding calls in the default search path).
fn run_search(opts: &SearchOptions) -> Result<String, String> {
    let index_path = opts.path.join(".wkp/index.db");
    let conn = wkp_core::index::open_index(&index_path).map_err(|e| e.to_string())?;
    let filter = wkp_core::index::SearchFilter {
        tier: opts.tier,
        budget: opts.budget,
        limit: opts.limit,
        ..Default::default()
    };
    let hits = wkp_core::index::search(&conn, &opts.query, &filter).map_err(|e| e.to_string())?;
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
/// same `--tier`/`--budget`/`--format` flags.
fn run_context(opts: &SearchOptions) -> Result<String, String> {
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

    #[test]
    fn run_init_creates_store_with_gitignored_index() {
        let temp = temp_dir("run-init");
        let dir = temp.path();
        run_init(dir).expect("run_init");

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

    #[test]
    fn run_init_is_idempotent_and_preserves_existing_gitignore_entries() {
        let temp = temp_dir("run-init-idempotent");
        let dir = temp.path();
        std::fs::write(dir.join(".gitignore"), "target/\n").expect("seed .gitignore");

        run_init(dir).expect("first run_init");
        run_init(dir).expect("second run_init");

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
        run_init(dir).expect("run_init");

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
        run_init(dir).expect("run_init");

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

    fn search_opts(dir: &Path, query: &str, format: SearchFormat) -> SearchOptions {
        SearchOptions {
            path: dir.to_path_buf(),
            query: query.to_string(),
            tier: None,
            budget: None,
            limit: None,
            format,
        }
    }

    #[test]
    fn run_search_all_three_formats() {
        let temp = temp_dir("run-search");
        let dir = temp.path();
        run_init(dir).expect("run_init");
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
        run_init(dir).expect("run_init");

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
        run_init(dir).expect("run_init");
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
        run_init(dir).expect("run_init");
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
        run_init(dir).expect("run_init");
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
        wkp_git::commit_all(dir, "seed").expect("commit_all");
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
        run_init(dir).expect("run_init");
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
        run_init(dir).expect("run_init");
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
}
