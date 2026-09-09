//! Tier computation (design 7.4, M2-6): the real, provenance-gated rule
//! that decides what a harness is allowed to see auto-injected at
//! session start.

use crate::frontmatter::{Frontmatter, ItemType};

/// The real, provenance-gated tier rule (design 7.4, M2-6) -- replaces
/// the M1-4 placeholder (a type/confidence-only heuristic; see the
/// milestone's own doc history) now that signed commits (M2-2) and the
/// `allowed_signers` identity model (M2-1) exist to check against. Tier
/// 0/1 requires *all* of:
///
/// - `human_signed`: the item's latest commit is signed by a `role:
///   human` principal. This alone is the actual design-7.4 gate; every
///   check below is an additional, independent guard on top of it.
/// - not under `inbox/` (design 5.4: "agent-written, unreviewed memory,
///   Tier 2 only until promoted") -- independent of `human_signed`,
///   because a human could otherwise directly commit a file under
///   `inbox/` without ever going through `wkp promote` (M2-7), which is
///   what this store's audit trail is supposed to require.
/// - `expires` (if set) has not passed (design 5.4: "the indexer excludes
///   expired items from Tier 0 and Tier 1 automatically" -- stored since
///   M1-1 but never actually checked until now).
/// - `type: instruction` additionally requires the path to be under
///   `user/` or anywhere under a `projects/<name>/` directory (design
///   7.4's extra scoping for instruction-like content). **Simplification**:
///   this checks "any `projects/*/`", not "the *current* project"
///   specifically -- tier is a property of the stored item computed at
///   index-build time, with no per-session "current project" context
///   available here; revisit if that distinction becomes load-bearing.
///
/// Otherwise: `project-state`/(scoped) `instruction` -> 0,
/// `feedback`/`knowledge` -> 1, everything else -> 2.
///
/// **Deliberately does not consult `confidence`.** An earlier version of
/// this gate treated `confidence: inferred`/`proposed` as a second,
/// independent "not yet reviewed" signal -- reasonable-looking while
/// `human_signed` didn't exist yet (the M1-4 placeholder used
/// `confidence` as its only proxy for "reviewed" at all), but wrong once
/// real signing landed: `wkp promote` (M2-7) explicitly moves an item
/// into the durable tree with a human-signed commit *without* rewriting
/// its `confidence`/`provenance` frontmatter (promotion is about the
/// commit's signer, not a content rewrite), so a promoted
/// `confidence: proposed` item would otherwise be permanently stuck at
/// tier 2 no matter who signs it -- caught by M2-7's own end-to-end
/// integration test actually promoting something, not by reasoning about
/// it in the abstract. Design 5.4 also never ties `confidence` to the
/// tier gate: it "mirrors the 'did the user say it' test," an epistemic
/// property of the *fact* (did the user state it, or did the agent infer
/// or propose it), orthogonal to whether the *item* has been reviewed and
/// signed. `human_signed` plus the `inbox/` check are design 7.4's actual,
/// complete gate.
///
/// Deliberately does **not** verify commits arriving via sync/fetch from
/// another machine (`wkp verify`) -- that's M3's "unsigned or
/// unknown-signer commits are excluded from Tier 0 and 1" exit-criterion
/// line, not this function's; `human_signed` here reflects whatever the
/// caller resolved regardless of a commit's origin.
pub(super) fn compute_tier(path: &str, fm: &Frontmatter, human_signed: bool) -> u8 {
    if path.starts_with("inbox/") || path.contains("/inbox/") {
        return 2;
    }
    if !human_signed {
        return 2;
    }
    if fm.expires.as_deref().is_some_and(is_expired) {
        return 2;
    }
    match fm.item_type {
        Some(ItemType::Instruction) => {
            if path.starts_with("user/") || path.contains("projects/") {
                0
            } else {
                2
            }
        }
        Some(ItemType::ProjectState) => 0,
        Some(ItemType::Feedback | ItemType::Knowledge) => 1,
        _ => 2,
    }
}

/// Whether `expires` (design 5.4's optional ISO date, e.g. `2026-01-01`)
/// is in the past, by lexicographic comparison against today's date in
/// the same `YYYY-MM-DD` format -- correct because zero-padded ISO 8601
/// dates sort identically as strings and as calendar dates, so no date
/// parsing/arithmetic is needed for the comparison itself. A value that
/// doesn't look like `YYYY-MM-DD` (too short) is treated as unparseable
/// and therefore not expired -- fail open, matching this parser's
/// tolerant posture elsewhere (`wkp-core::frontmatter`), rather than
/// accidentally demoting an item over a malformed date.
fn is_expired(expires: &str) -> bool {
    if expires.len() < 10 {
        return false;
    }
    expires[..10] < *today_iso_date()
}

/// Today's date as `YYYY-MM-DD`, computed from the system clock without a
/// calendar/date crate dependency (CLAUDE.md's slim-core rule): Howard
/// Hinnant's `civil_from_days` algorithm
/// (<http://howardhinnant.github.io/date_algorithms.html#civil_from_days>),
/// a well-known, allocation-free, leap-year-correct conversion from a day
/// count to a proleptic Gregorian calendar date. Verified against known
/// reference dates (including a leap day and a pre-epoch day) in this
/// module's tests, not merely transcribed and trusted.
fn today_iso_date() -> String {
    let days_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(days_since_epoch);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Falls back to roughly 4 characters per token (a common rough estimate
/// for English prose) when frontmatter doesn't carry an explicit `tokens:`
/// value. An estimate, not a real tokenizer count -- good enough for
/// budget truncation, not for billing or precise context-window math.
pub(super) fn estimate_tokens(fm: &Frontmatter, body: &str) -> u32 {
    fm.tokens
        .unwrap_or_else(|| (body.chars().count() as u32 / 4).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::Confidence;

    #[test]
    fn compute_tier_reflects_type_when_human_signed() {
        let mut fm = Frontmatter {
            item_type: Some(ItemType::ProjectState),
            ..Default::default()
        };
        assert_eq!(compute_tier("a.md", &fm, true), 0);

        fm.item_type = Some(ItemType::Feedback);
        assert_eq!(compute_tier("a.md", &fm, true), 1);

        fm.item_type = Some(ItemType::Reference);
        assert_eq!(compute_tier("a.md", &fm, true), 2);
    }

    /// M2-7 found this the hard way (an end-to-end `wkp promote` test
    /// failed until this was fixed): `wkp promote` explicitly never
    /// rewrites `confidence`, so a promoted item human-signs into the
    /// durable tree while still carrying `confidence: proposed` from
    /// when it was written. Tier eligibility must not depend on
    /// `confidence` at all -- only on `human_signed` and not being under
    /// `inbox/` (design 7.4's actual gate).
    #[test]
    fn compute_tier_ignores_confidence_once_human_signed_and_out_of_inbox() {
        let mut fm = Frontmatter {
            item_type: Some(ItemType::ProjectState),
            confidence: Some(Confidence::Proposed),
            ..Default::default()
        };
        assert_eq!(
            compute_tier("projects/wkp/promoted.md", &fm, true),
            0,
            "a human-signed, promoted item must reach tier 0 even with confidence: proposed"
        );

        fm.confidence = Some(Confidence::Inferred);
        assert_eq!(compute_tier("projects/wkp/promoted.md", &fm, true), 0);

        fm.confidence = Some(Confidence::Stated);
        assert_eq!(compute_tier("projects/wkp/promoted.md", &fm, true), 0);
    }

    /// M2-6's actual gate (design 7.4): the check the M1-4 placeholder
    /// could not make, since signed commits didn't exist yet.
    #[test]
    fn compute_tier_requires_a_human_signed_commit_for_tier_0_or_1() {
        let fm = Frontmatter {
            item_type: Some(ItemType::ProjectState),
            ..Default::default()
        };
        assert_eq!(
            compute_tier("a.md", &fm, false),
            2,
            "type: project-state alone must not reach tier 0/1 without a human-signed commit"
        );
        assert_eq!(
            compute_tier("a.md", &fm, true),
            0,
            "the same frontmatter with a human-signed commit does reach tier 0"
        );
    }

    #[test]
    fn compute_tier_forces_tier_2_for_anything_under_inbox() {
        let fm = Frontmatter {
            item_type: Some(ItemType::ProjectState),
            ..Default::default()
        };
        assert_eq!(
            compute_tier("inbox/import/claude-md.md", &fm, true),
            2,
            "an inbox/ item must not self-promote to tier 0 via its type, even if human-signed"
        );
        assert_eq!(
            compute_tier("projects/wkp/inbox/note.md", &fm, true),
            2,
            "a nested inbox/ directory anywhere in the path must also be caught"
        );
        assert_eq!(
            compute_tier("projects/wkp/decision.md", &fm, true),
            0,
            "a normal path with the same frontmatter is unaffected"
        );
    }

    #[test]
    fn compute_tier_instruction_type_requires_user_or_projects_path_scoping() {
        let fm = Frontmatter {
            item_type: Some(ItemType::Instruction),
            ..Default::default()
        };
        assert_eq!(
            compute_tier("org/wide-policy.md", &fm, true),
            2,
            "type: instruction outside user/ or projects/ must not reach tier 0"
        );
        assert_eq!(compute_tier("user/preferences.md", &fm, true), 0);
        assert_eq!(compute_tier("projects/wkp/agents.md", &fm, true), 0);
    }

    #[test]
    fn compute_tier_expired_item_is_tier_2_regardless_of_type_and_signing() {
        let mut fm = Frontmatter {
            item_type: Some(ItemType::ProjectState),
            ..Default::default()
        };
        fm.expires = Some("2000-01-01".to_string());
        assert_eq!(
            compute_tier("a.md", &fm, true),
            2,
            "an item past its expires date must not reach tier 0/1"
        );

        fm.expires = Some("9999-12-31".to_string());
        assert_eq!(
            compute_tier("a.md", &fm, true),
            0,
            "an item with a future expires date is unaffected"
        );

        fm.expires = None;
        assert_eq!(
            compute_tier("a.md", &fm, true),
            0,
            "no expires date at all is unaffected"
        );
    }

    #[test]
    fn is_expired_treats_a_too_short_value_as_unparseable_and_not_expired() {
        assert!(!is_expired("2000"));
        assert!(!is_expired(""));
    }

    #[test]
    fn civil_from_days_matches_known_reference_dates() {
        // Cross-checked against Python's datetime.date arithmetic,
        // including a leap day and a pre-epoch (negative day count) date
        // -- not just transcribed from the algorithm source and trusted.
        for (days, expected) in [
            (0i64, (1970, 1, 1)),
            (11_017, (2000, 3, 1)),
            (20_703, (2026, 9, 7)),
            (19_782, (2024, 2, 29)),
            (-1, (1969, 12, 31)),
        ] {
            assert_eq!(civil_from_days(days), expected, "days={days}");
        }
    }
}
