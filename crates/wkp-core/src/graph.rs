//! Explicit reference extraction for the knowledge graph (design 5.1, 5.4):
//! `refs:` frontmatter entries and `[[wikilink]]` mentions in a body.
//!
//! Pure parsing lives here; resolving a wikilink *name* to an actual store
//! *path* needs the current set of known paths, which lives in the index
//! (see `crate::index::resolve_wikilink`), so that part isn't in this
//! module.

use std::path::{Component, Path, PathBuf};

/// Extracts `[[name]]`, `[[name|alias]]`, and `[[name#section]]` mentions
/// from `body`, returning the bare `name` for each occurrence in order.
///
/// Hand-rolled rather than pulling in `regex` for one pattern (CLAUDE.md:
/// justify every new dependency) — matches the old Python tool's
/// `\[\[([^\]|#]+)[|\]#]` semantics for the common case, not nested
/// brackets or otherwise malformed markup.
pub fn extract_wikilink_names(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        let inner = &after[..end];
        let name = inner.split(['|', '#']).next().unwrap_or(inner).trim();
        if !name.is_empty() {
            names.push(name.to_string());
        }
        rest = &after[end + 2..];
    }
    names
}

/// Normalizes a `refs:` entry — a path relative to `source_path`'s own
/// directory (design 5.4) — into a store-root-relative path, resolving
/// `.`/`..` without touching the filesystem (the target need not exist on
/// disk to be recorded as an edge). Returns `None` if the reference would
/// escape the store root entirely.
pub fn normalize_ref(source_path: &str, raw_ref: &str) -> Option<String> {
    let source_dir = Path::new(source_path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let joined = source_dir.join(raw_ref);
    let normalized = normalize_path(&joined);
    let normalized_str = normalized.to_string_lossy().replace('\\', "/");
    if normalized_str == ".." || normalized_str.starts_with("../") {
        None
    } else {
        Some(normalized_str)
    }
}

/// `Component`-based `.`/`..` collapsing that never touches the
/// filesystem, unlike `fs::canonicalize` (which requires the path to
/// exist and resolves symlinks, neither of which applies here — a `refs:`
/// target may not exist yet).
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_plain_wikilink() {
        assert_eq!(
            extract_wikilink_names("see [[Other Item]] for more"),
            vec!["Other Item"]
        );
    }

    #[test]
    fn extracts_wikilink_with_alias() {
        assert_eq!(
            extract_wikilink_names("[[target|shown text]]"),
            vec!["target"]
        );
    }

    #[test]
    fn extracts_wikilink_with_section() {
        assert_eq!(extract_wikilink_names("[[target#section]]"), vec!["target"]);
    }

    #[test]
    fn extracts_multiple_wikilinks_in_order() {
        assert_eq!(
            extract_wikilink_names("[[a]] and then [[b]]"),
            vec!["a", "b"]
        );
    }

    #[test]
    fn ignores_unterminated_wikilink() {
        assert!(extract_wikilink_names("this has [[ no closing").is_empty());
    }

    #[test]
    fn ignores_body_with_no_wikilinks() {
        assert!(extract_wikilink_names("plain text, nothing special").is_empty());
    }

    #[test]
    fn ignores_empty_wikilink() {
        assert!(extract_wikilink_names("[[]]").is_empty());
    }

    #[test]
    fn normalize_ref_resolves_sibling_path() {
        assert_eq!(
            normalize_ref("projects/a.md", "b.md"),
            Some("projects/b.md".to_string())
        );
    }

    #[test]
    fn normalize_ref_resolves_parent_relative_path() {
        assert_eq!(
            normalize_ref("projects/sub/a.md", "../b.md"),
            Some("projects/b.md".to_string())
        );
    }

    #[test]
    fn normalize_ref_rejects_escaping_the_store_root() {
        assert_eq!(normalize_ref("a.md", "../outside.md"), None);
    }

    #[test]
    fn normalize_ref_handles_a_root_level_source() {
        assert_eq!(normalize_ref("a.md", "b.md"), Some("b.md".to_string()));
    }

    #[test]
    fn normalize_ref_collapses_current_dir_segments() {
        assert_eq!(
            normalize_ref("projects/a.md", "./b.md"),
            Some("projects/b.md".to_string())
        );
    }
}
