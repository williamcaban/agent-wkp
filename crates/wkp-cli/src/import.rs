//! `wkp import`/`wkp init`'s import step: pulling in existing harness
//! memory (design 10, M1-7).

use std::path::{Path, PathBuf};

/// A one-line summary of what `wkp import` did.
pub(crate) struct ImportSummary {
    pub(crate) imported: usize,
    pub(crate) skipped: usize,
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
pub(crate) fn claude_home_from_env() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| claude_home_dir(&h))
}

pub(crate) fn claude_home_dir(home: &std::ffi::OsStr) -> PathBuf {
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
pub(crate) fn run_import(
    store_path: &Path,
    claude_home: Option<&Path>,
) -> Result<ImportSummary, String> {
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

pub(crate) fn okf_frontmatter_block(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

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
}
