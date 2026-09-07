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

    fn temp_dir(name: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("wkp-cli-test-{name}-{pid}-{nanos}"))
    }

    #[test]
    fn run_init_creates_store_with_gitignored_index() {
        let dir = temp_dir("run-init");
        run_init(&dir).expect("run_init");

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

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_init_is_idempotent_and_preserves_existing_gitignore_entries() {
        let dir = temp_dir("run-init-idempotent");
        std::fs::create_dir_all(&dir).expect("create dir");
        std::fs::write(dir.join(".gitignore"), "target/\n").expect("seed .gitignore");

        run_init(&dir).expect("first run_init");
        run_init(&dir).expect("second run_init");

        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).expect("read .gitignore");
        assert_eq!(gitignore.matches(".wkp/index.db").count(), 1);
        assert!(gitignore.contains("target/"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
