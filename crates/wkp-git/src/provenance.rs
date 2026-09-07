//! Commit-message provenance trailers (design 5.4, 7.4, M2-3): a
//! parallel, commit-level representation of who/what produced a commit,
//! read and written via git's own trailer machinery (`git
//! interpret-trailers`) rather than a wkp-invented message format --
//! letting git's own blank-line-separation and parsing rules handle the
//! subject/trailer-block boundary, on both the write and read side, is
//! exactly the "reuse git... don't reinvent" slim-core rule applied to
//! this specific problem.
//!
//! This mirrors design 5.4's frontmatter `provenance:`/`confidence:`
//! fields (`wkp_core::frontmatter::Provenance`), but is a deliberately
//! separate, `wkp-core`-independent type: `wkp-git` sits below `wkp-core`
//! in the crate layout (design 3.3) and must not gain a dependency on it.
//! A caller that has both (e.g. the future `wkp remember`, M2-5) is
//! responsible for keeping the two in sync; nothing here assumes they
//! agree.

use std::path::Path;

use crate::signed_commit::CommitId;

/// The `Wkp-*` trailer keys this module reads and writes -- every field
/// optional, since a caller may not always have all four (e.g. no
/// `session` for a commit produced outside any harness session).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// `human:<id>` or `agent:<harness>[:<model>]` (design 5.4's
    /// `provenance.actor` convention -- the same principal string
    /// [`crate::allowed_signers`] and [`crate::signed_commit`] use).
    pub actor: Option<String>,
    /// Opaque, harness-provided session identifier.
    pub session: Option<String>,
    /// `conversation` | `file` | `tool` | `import`, kept as a raw string
    /// here -- `wkp-core::frontmatter::SourceKind` owns validating it,
    /// this crate only ever carries it through unchanged.
    pub source: Option<String>,
    /// `stated` | `inferred` | `proposed`, also kept as a raw string for
    /// the same reason.
    pub confidence: Option<String>,
}

const TRAILER_ACTOR: &str = "Wkp-Actor";
const TRAILER_SESSION: &str = "Wkp-Session";
const TRAILER_SOURCE: &str = "Wkp-Source";
const TRAILER_CONFIDENCE: &str = "Wkp-Confidence";

impl Provenance {
    fn pairs(&self) -> Vec<(&'static str, &str)> {
        let mut out = Vec::new();
        if let Some(v) = &self.actor {
            out.push((TRAILER_ACTOR, v.as_str()));
        }
        if let Some(v) = &self.session {
            out.push((TRAILER_SESSION, v.as_str()));
        }
        if let Some(v) = &self.source {
            out.push((TRAILER_SOURCE, v.as_str()));
        }
        if let Some(v) = &self.confidence {
            out.push((TRAILER_CONFIDENCE, v.as_str()));
        }
        out
    }

    /// Formats `subject` plus this provenance's non-`None` fields as
    /// trailers, via `git interpret-trailers --trailer` (piped `subject`
    /// on stdin, no `<file>` argument). A `Provenance` with every field
    /// `None` formats to `subject` unchanged -- no empty trailing blank
    /// line or trailer block for a caller that has nothing to record.
    pub(crate) fn format_message(&self, repo_dir: &Path, subject: &str) -> Result<String, String> {
        let pairs = self.pairs();
        if pairs.is_empty() {
            return Ok(subject.to_string());
        }
        let mut trailer_args: Vec<String> = Vec::with_capacity(pairs.len());
        for (key, value) in &pairs {
            trailer_args.push(format!("{key}={value}"));
        }
        let mut args: Vec<&str> = vec!["interpret-trailers"];
        for t in &trailer_args {
            args.push("--trailer");
            args.push(t);
        }
        crate::run_git_with_stdin(repo_dir, &args, subject)
    }
}

/// Reads back the `Wkp-*` trailers from `commit`'s message (design 5.4,
/// 7.4). `Ok(None)` -- not an error -- for a commit with no trailers at
/// all (a commit `signed_commit` produced with an all-`None`
/// `Provenance`, or any commit from outside this system entirely, e.g. a
/// human's ordinary `git commit`).
pub fn read_provenance_trailers(
    repo_dir: &Path,
    commit: &CommitId,
) -> Result<Option<Provenance>, String> {
    let body = crate::run_git_stdout(repo_dir, &["log", "-1", "--format=%B", &commit.0])?;
    let trailers_block = crate::run_git_with_stdin(
        repo_dir,
        &["interpret-trailers", "--parse", "--only-trailers"],
        &body,
    )?;

    let mut provenance = Provenance::default();
    for line in trailers_block.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_string();
        match key.trim() {
            TRAILER_ACTOR => provenance.actor = Some(value),
            TRAILER_SESSION => provenance.session = Some(value),
            TRAILER_SOURCE => provenance.source = Some(value),
            TRAILER_CONFIDENCE => provenance.confidence = Some(value),
            _ => {}
        }
    }

    if provenance == Provenance::default() {
        Ok(None)
    } else {
        Ok(Some(provenance))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_message_appends_only_the_present_fields_as_trailers() {
        let dir = tempfile::Builder::new()
            .prefix("wkp-git-test-provenance-format-")
            .tempdir()
            .expect("create temp dir");
        crate::init_repo(dir.path()).expect("init_repo");

        let provenance = Provenance {
            actor: Some("agent:claude-code@host".to_string()),
            session: None,
            source: Some("conversation".to_string()),
            confidence: None,
        };
        let message = provenance
            .format_message(dir.path(), "a subject line")
            .expect("format_message");

        assert_eq!(
            message,
            "a subject line\n\nWkp-Actor: agent:claude-code@host\nWkp-Source: conversation\n"
        );
    }

    #[test]
    fn format_message_with_no_fields_returns_the_subject_unchanged() {
        let dir = tempfile::Builder::new()
            .prefix("wkp-git-test-provenance-format-empty-")
            .tempdir()
            .expect("create temp dir");
        crate::init_repo(dir.path()).expect("init_repo");

        let message = Provenance::default()
            .format_message(dir.path(), "just a subject")
            .expect("format_message");
        assert_eq!(message, "just a subject");
    }
}
