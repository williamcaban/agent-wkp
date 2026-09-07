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
}
