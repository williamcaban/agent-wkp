//! Custom git merge driver logic for OKF frontmatter (design 6.2, M3-2):
//! safe-mode semantics -- union of `tags`, max of `updated`, and (design's
//! own words) "keep both bodies with provenance markers when the body
//! conflicts." Pure content transformation, no git I/O at all: the
//! actual git merge-driver protocol (reading `%O`/`%A`/`%B` temp files,
//! writing the result back, choosing an exit code) is `wkp-cli`'s `wkp
//! merge-driver` subcommand; this module is what it calls.
//!
//! "Nothing is ever discarded" (design 6.2's safe-mode rule) is this
//! module's one hard invariant: every genuinely irreconcilable
//! divergence -- a scalar frontmatter field changed differently on both
//! sides, or the body itself diverging -- falls through to rendering
//! **both** original documents in full, clearly labeled, rather than
//! picking one side and losing the other. [`merge`] therefore always
//! succeeds (there is no case left over for git's own conflict markers):
//! the "ask" step of design 6.2's three-step strategy is `wkp sync
//! status` (M3-6) scanning materialized content for this module's own
//! [`CONFLICT_MARKER`], not a git-level conflicted file.

use crate::frontmatter;

/// Present in [`merge`]'s output exactly when it fell back to rendering
/// both original documents -- `wkp sync status` (M3-6) greps for this
/// rather than relying on git's own conflict state, since this driver
/// never leaves one.
pub const CONFLICT_MARKER: &str = "<!-- wkp-merge-conflict -->";

/// Merges three whole file contents (ancestor/ours/theirs -- matching
/// git's own `%O`/`%A`/`%B` merge-driver arguments) into one. For an
/// add/add conflict (no common ancestor), pass an empty string for
/// `base` -- parsing it yields `Frontmatter::default()` and an empty
/// body, which naturally makes both sides look like they diverged from
/// "nothing," landing in the same keep-both fallback the add/add
/// acceptance criteria asks for.
pub fn merge(base: &str, ours: &str, theirs: &str) -> String {
    let base_parsed = frontmatter::parse(base);
    let ours_parsed = frontmatter::parse(ours);
    let theirs_parsed = frontmatter::parse(theirs);

    let mut hard_conflict = false;
    let mut merged = ours_parsed.frontmatter.clone();

    // Always-mergeable fields (design 6.2): never contribute to a
    // conflict on their own.
    merged.tags = union(
        &ours_parsed.frontmatter.tags,
        &theirs_parsed.frontmatter.tags,
    );
    merged.updated = max_updated(
        ours_parsed.frontmatter.updated.as_deref(),
        theirs_parsed.frontmatter.updated.as_deref(),
    );
    // provenance is left as ours's own: differing provenance alone
    // (near-certain for any two independently-written versions -- a
    // different actor/session touched each one) is not itself treated
    // as a conflict, only a genuine *content* divergence is. Design
    // 6.2's "keep both provenance entries" is satisfied as a
    // consequence of the keep-both-documents fallback below when a
    // real conflict *does* occur -- each rendered copy still carries
    // its own original provenance intact -- not as an independent
    // frontmatter-level rule this schema (one `provenance:` block per
    // file) has room to express on its own.

    hard_conflict |= resolve_scalar(
        &mut merged.title,
        &base_parsed.frontmatter.title,
        &ours_parsed.frontmatter.title,
        &theirs_parsed.frontmatter.title,
    );
    hard_conflict |= resolve_scalar(
        &mut merged.item_type,
        &base_parsed.frontmatter.item_type,
        &ours_parsed.frontmatter.item_type,
        &theirs_parsed.frontmatter.item_type,
    );
    hard_conflict |= resolve_scalar(
        &mut merged.scope,
        &base_parsed.frontmatter.scope,
        &ours_parsed.frontmatter.scope,
        &theirs_parsed.frontmatter.scope,
    );
    hard_conflict |= resolve_scalar(
        &mut merged.workspace,
        &base_parsed.frontmatter.workspace,
        &ours_parsed.frontmatter.workspace,
        &theirs_parsed.frontmatter.workspace,
    );
    hard_conflict |= resolve_scalar(
        &mut merged.visibility,
        &base_parsed.frontmatter.visibility,
        &ours_parsed.frontmatter.visibility,
        &theirs_parsed.frontmatter.visibility,
    );
    hard_conflict |= resolve_scalar(
        &mut merged.confidence,
        &base_parsed.frontmatter.confidence,
        &ours_parsed.frontmatter.confidence,
        &theirs_parsed.frontmatter.confidence,
    );
    hard_conflict |= resolve_scalar(
        &mut merged.expires,
        &base_parsed.frontmatter.expires,
        &ours_parsed.frontmatter.expires,
        &theirs_parsed.frontmatter.expires,
    );

    let body_conflict = ours_parsed.body != base_parsed.body
        && theirs_parsed.body != base_parsed.body
        && ours_parsed.body != theirs_parsed.body;

    if hard_conflict || body_conflict {
        return render_conflict(ours, theirs);
    }

    let body = if ours_parsed.body != base_parsed.body {
        ours_parsed.body
    } else {
        theirs_parsed.body
    };
    format!("{}\n{}", frontmatter::render(&merged), body)
}

/// Standard 3-way scalar merge: unchanged on both sides, or changed on
/// exactly one, resolves without conflict; changed differently on both
/// sides is a conflict. Returns whether it was a conflict; on conflict,
/// `field` is left at whatever `merge` initialized it to (irrelevant,
/// since a conflict always routes to [`render_conflict`], which ignores
/// the merged frontmatter entirely).
fn resolve_scalar<T: Clone + PartialEq>(field: &mut T, base: &T, ours: &T, theirs: &T) -> bool {
    if ours == theirs {
        *field = ours.clone();
        false
    } else if ours == base {
        *field = theirs.clone();
        false
    } else if theirs == base {
        *field = ours.clone();
        false
    } else {
        true
    }
}

/// Union of two tag lists, order-preserving (ours's tags first, then
/// any of theirs's not already present) and de-duplicated.
fn union(ours: &[String], theirs: &[String]) -> Vec<String> {
    let mut result: Vec<String> = ours.to_vec();
    for tag in theirs {
        if !result.contains(tag) {
            result.push(tag.clone());
        }
    }
    result
}

/// The lexicographically later of two optional ISO date-like strings --
/// correct for `YYYY-MM-DD` (zero-padded ISO 8601 sorts identically as
/// strings and as dates, the same reasoning `wkp_core::index`'s
/// `is_expired` already relies on). `None` loses to `Some` unconditionally
/// (a later `updated` beats no `updated` at all).
fn max_updated(ours: Option<&str>, theirs: Option<&str>) -> Option<String> {
    match (ours, theirs) {
        (Some(o), Some(t)) => Some(if o >= t { o } else { t }.to_string()),
        (Some(o), None) => Some(o.to_string()),
        (None, Some(t)) => Some(t.to_string()),
        (None, None) => None,
    }
}

/// The keep-both-documents fallback (design 6.2): renders `ours` and
/// `theirs` in full, each labeled by its own provenance actor if it has
/// one, separated by [`CONFLICT_MARKER`] so `wkp sync status` (M3-6) can
/// find it later. Never drops either side.
fn render_conflict(ours: &str, theirs: &str) -> String {
    let ours_actor = frontmatter::parse(ours)
        .frontmatter
        .provenance
        .actor
        .unwrap_or_else(|| "unknown".to_string());
    let theirs_actor = frontmatter::parse(theirs)
        .frontmatter
        .provenance
        .actor
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "{CONFLICT_MARKER}\n\
         <!-- wkp: this item could not be merged structurally; both \
         versions are kept below. Resolve by hand and commit normally. -->\n\n\
         <!-- ours (provenance.actor: {ours_actor:?}) -->\n{}\n\n\
         <!-- theirs (provenance.actor: {theirs_actor:?}) -->\n{}\n",
        ours.trim_end(),
        theirs.trim_end(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::parse;

    fn item(title: &str, tags: &str, updated: &str, body: &str) -> String {
        format!("---\ntitle: {title}\ntags: {tags}\nupdated: {updated}\n---\n\n{body}\n")
    }

    #[test]
    fn tags_union_and_updated_max_merge_without_conflict() {
        let base = item("A", "[]", "2026-01-01", "body");
        let ours = item("A", "[foo]", "2026-01-01", "body");
        let theirs = item("A", "[bar]", "2026-02-01", "body");

        let merged = merge(&base, &ours, &theirs);
        assert!(
            !merged.contains(CONFLICT_MARKER),
            "tags/updated alone must not conflict: {merged}"
        );
        let fm = parse(&merged).frontmatter;
        assert_eq!(fm.tags, vec!["foo", "bar"]);
        assert_eq!(fm.updated.as_deref(), Some("2026-02-01"));
    }

    #[test]
    fn one_side_changing_a_scalar_field_resolves_without_conflict() {
        let base = "---\ntitle: A\n---\n\nbody\n";
        let ours = "---\ntitle: A New Title\n---\n\nbody\n";
        let theirs = "---\ntitle: A\n---\n\nbody\n";

        let merged = merge(base, ours, theirs);
        assert!(!merged.contains(CONFLICT_MARKER));
        assert_eq!(
            parse(&merged).frontmatter.title.as_deref(),
            Some("A New Title")
        );
    }

    #[test]
    fn both_sides_changing_a_scalar_field_differently_is_a_conflict() {
        let base = "---\ntitle: A\n---\n\nbody\n";
        let ours = "---\ntitle: Ours Title\n---\n\nbody\n";
        let theirs = "---\ntitle: Theirs Title\n---\n\nbody\n";

        let merged = merge(base, ours, theirs);
        assert!(merged.contains(CONFLICT_MARKER));
        assert!(merged.contains("Ours Title"));
        assert!(merged.contains("Theirs Title"));
    }

    #[test]
    fn body_diverging_on_both_sides_keeps_both_with_provenance_markers() {
        let base = "---\ntitle: A\nprovenance:\n  actor: human:alice\n---\n\noriginal\n";
        let ours = "---\ntitle: A\nprovenance:\n  actor: agent:x\n---\n\nours body\n";
        let theirs = "---\ntitle: A\nprovenance:\n  actor: agent:y\n---\n\ntheirs body\n";

        let merged = merge(base, ours, theirs);
        assert!(merged.contains(CONFLICT_MARKER));
        assert!(merged.contains("ours body"));
        assert!(merged.contains("theirs body"));
        assert!(merged.contains("agent:x"));
        assert!(merged.contains("agent:y"));
    }

    /// The acceptance-criteria add/add case: no common ancestor at all
    /// (git passes an empty %O), both sides add the same path with
    /// different bodies -- must keep both.
    #[test]
    fn add_add_with_no_common_ancestor_keeps_both_bodies() {
        let base = "";
        let ours = "---\ntitle: Ours\n---\n\nours content\n";
        let theirs = "---\ntitle: Theirs\n---\n\ntheirs content\n";

        let merged = merge(base, ours, theirs);
        assert!(merged.contains(CONFLICT_MARKER));
        assert!(merged.contains("ours content"));
        assert!(merged.contains("theirs content"));
    }

    #[test]
    fn identical_ours_and_theirs_merges_cleanly_even_if_both_differ_from_base() {
        let base = item("A", "[]", "2026-01-01", "old body");
        let ours = item("A", "[x]", "2026-02-01", "new body");
        let theirs = item("A", "[x]", "2026-02-01", "new body");

        let merged = merge(&base, &ours, &theirs);
        assert!(!merged.contains(CONFLICT_MARKER));
        assert!(merged.contains("new body"));
    }

    /// "Nothing is ever discarded" (design 6.2), checked broadly: for a
    /// range of synthetic tag-set/updated-date combinations, every value
    /// present on either side survives into the merge (either via the
    /// union/max rules directly, or -- if something else also
    /// conflicted -- via the keep-both-documents fallback).
    #[test]
    fn nothing_is_discarded_across_synthetic_tag_and_date_combinations() {
        let tag_sets = [
            (vec!["a"], vec!["b"]),
            (vec!["a", "b"], vec!["b", "c"]),
            (vec![], vec!["x"]),
            (vec!["shared"], vec!["shared"]),
        ];
        let dates = [("2026-01-01", "2026-06-01"), ("2026-06-01", "2026-01-01")];

        for (ours_tags, theirs_tags) in &tag_sets {
            for (ours_date, theirs_date) in &dates {
                let base = item("A", "[]", "2025-01-01", "body");
                let ours = item(
                    "A",
                    &format!("[{}]", ours_tags.join(", ")),
                    ours_date,
                    "body",
                );
                let theirs = item(
                    "A",
                    &format!("[{}]", theirs_tags.join(", ")),
                    theirs_date,
                    "body",
                );

                let merged = merge(&base, &ours, &theirs);
                let fm = parse(&merged).frontmatter;
                for tag in ours_tags.iter().chain(theirs_tags.iter()) {
                    assert!(
                        fm.tags.iter().any(|t| t == tag),
                        "tag {tag:?} lost merging {ours_tags:?} + {theirs_tags:?} -> {:?}",
                        fm.tags
                    );
                }
                let expected_date = if ours_date >= theirs_date {
                    ours_date
                } else {
                    theirs_date
                };
                assert_eq!(fm.updated.as_deref(), Some(*expected_date));
            }
        }
    }
}
