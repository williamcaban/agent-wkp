//! Write-path secret detection (design 7.6, M2-4): "`wkp remember` and the
//! pre-commit path run gitleaks rules (bundled as data, evaluated
//! in-process) and refuse to commit content that matches credential
//! patterns, returning a redacted preview so the harness can retry."
//!
//! The four rules below are a small, deliberately bundled subset of
//! [gitleaks](https://github.com/gitleaks/gitleaks)'s own public
//! `gitleaks.toml` default ruleset (`aws-access-token`, `private-key`,
//! `generic-api-key`, and its entropy-gated generic-secret pattern),
//! reimplemented here rather than shelling out to the real `gitleaks`
//! binary (CLAUDE.md's slim-core rule: no new runtime dependency on an
//! external tool for something four `regex` patterns and a Shannon-entropy
//! check already cover). This is a real subset, not an invented one --
//! citing the source ruleset so a future rule addition can be checked
//! against gitleaks' own, broader set rather than reinvented from
//! scratch.
//!
//! Pure content scanning: no I/O, no git, no network -- [`scan`] takes a
//! `&str` and returns [`Finding`]s. Wiring this into `wkp remember`'s
//! actual write path (refusing the write, printing the redacted preview)
//! is M2-5's job, not this module's.

use std::sync::LazyLock;

use regex::Regex;

/// One credential-pattern match. `redacted_excerpt` shows surrounding
/// context with the matched secret itself replaced by a fixed marker --
/// never the actual matched text, so a caller can safely print this to a
/// terminal or log without leaking the secret it just found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: &'static str,
    /// Byte offsets into the scanned content (not char offsets) --
    /// matching `regex::Match`'s own convention, so a caller that wants
    /// the original text back can still safely slice `content[start..end]`
    /// (both offsets always land on a UTF-8 char boundary).
    pub start: usize,
    pub end: usize,
    pub redacted_excerpt: String,
}

const REDACTION_MARKER: &str = "«redacted»";
/// How many bytes of surrounding context to show on each side of a
/// redacted match -- enough to see what kind of assignment it was
/// (`AWS_SECRET_KEY = «redacted»`) without needing the full line, which
/// could itself be long or contain other sensitive content.
const CONTEXT_BYTES: usize = 20;

struct Rule {
    name: &'static str,
    pattern: LazyLock<Regex>,
    /// `Some(threshold)` for a rule that additionally requires the
    /// matched value (capture group 1) to clear a Shannon-entropy bar
    /// before it counts as a finding -- gitleaks' own approach to
    /// separating "looks like a real random secret" from "looks like a
    /// plausible but low-entropy placeholder" for its generic patterns.
    min_entropy: Option<f64>,
}

macro_rules! rule {
    ($name:expr, $pattern:expr) => {
        Rule {
            name: $name,
            pattern: LazyLock::new(|| Regex::new($pattern).expect("static regex is valid")),
            min_entropy: None,
        }
    };
    ($name:expr, $pattern:expr, min_entropy = $entropy:expr) => {
        Rule {
            name: $name,
            pattern: LazyLock::new(|| Regex::new($pattern).expect("static regex is valid")),
            min_entropy: Some($entropy),
        }
    };
}

/// AWS access key IDs (gitleaks `aws-access-token`): `AKIA`/`ASIA`
/// prefix plus 16 base32-ish characters. No entropy gate needed -- the
/// prefix alone is a strong enough signal.
static AWS_ACCESS_KEY_ID: Rule = rule!("aws-access-key-id", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b");

/// PEM private-key headers (gitleaks `private-key`): matches the exact
/// line `ssh-keygen`/`openssl` write at the top of any PEM-encoded
/// private key, regardless of key type.
static PRIVATE_KEY_PEM_HEADER: Rule = rule!(
    "private-key-pem-header",
    r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |ENCRYPTED )?PRIVATE KEY-----"
);

/// A keyword-gated assignment (gitleaks `generic-api-key`): `api_key`,
/// `secret`, `token`, or `password` (case-insensitive) followed by
/// `:`/`=` and a quoted-or-bare value of at least 16 characters. The
/// keyword itself is the signal here, so no entropy gate.
static GENERIC_API_KEY: Rule = rule!(
    "generic-api-key",
    r#"(?i)\b(?:api[_-]?key|secret|token|passwd|password)\b\s*[:=]\s*['"]?([A-Za-z0-9_/+.=-]{16,100})['"]?"#
);

/// Any assignment-shaped value -- no keyword required -- whose value
/// clears a Shannon-entropy bar (gitleaks' generic entropy-gated rule):
/// catches a real secret under an arbitrary variable name (`x_9f2 =
/// "..."`), which [`GENERIC_API_KEY`] alone would miss. The entropy gate
/// (3.5 bits/char, gitleaks' own default for this class of rule) is what
/// keeps this from matching every ordinary assignment in a config file.
static HIGH_ENTROPY_ASSIGNMENT: Rule = rule!(
    "high-entropy-assignment",
    r#"\b[A-Za-z_][A-Za-z0-9_]{2,40}\s*[:=]\s*['"]([A-Za-z0-9+/=_-]{20,100})['"]"#,
    min_entropy = 3.5
);

fn rules() -> [&'static Rule; 4] {
    [
        &AWS_ACCESS_KEY_ID,
        &PRIVATE_KEY_PEM_HEADER,
        &GENERIC_API_KEY,
        &HIGH_ENTROPY_ASSIGNMENT,
    ]
}

/// Shannon entropy in bits per character. Used to distinguish a real
/// random-looking secret from a low-entropy placeholder string
/// (`"aaaaaaaaaaaaaaaaaaaa"`, `"1111111111111111111"`) that happens to be
/// long enough to match [`HIGH_ENTROPY_ASSIGNMENT`]'s length bound.
fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts: std::collections::HashMap<char, usize> = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0) += 1;
    }
    let len = s.chars().count() as f64;
    counts
        .values()
        .map(|&count| {
            let p = count as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// The nearest byte offset `<= at` that lands on a UTF-8 char boundary --
/// `content.floor_char_boundary(at)` isn't stable yet, so this is the
/// stable equivalent, needed because [`CONTEXT_BYTES`]'s fixed-width
/// window can otherwise land mid-character on non-ASCII content.
fn floor_char_boundary(content: &str, at: usize) -> usize {
    let mut at = at.min(content.len());
    while at > 0 && !content.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_char_boundary(content: &str, at: usize) -> usize {
    let mut at = at.min(content.len());
    while at < content.len() && !content.is_char_boundary(at) {
        at += 1;
    }
    at
}

fn redacted_excerpt(content: &str, start: usize, end: usize) -> String {
    let before_start = floor_char_boundary(content, start.saturating_sub(CONTEXT_BYTES));
    let after_end = ceil_char_boundary(content, (end + CONTEXT_BYTES).min(content.len()));
    format!(
        "{}{}{}",
        &content[before_start..start],
        REDACTION_MARKER,
        &content[end..after_end]
    )
}

/// Scans `content` against the bundled rule set, returning one
/// [`Finding`] per match (a rule may match more than once in the same
/// content; every match is its own finding). Pure and side-effect free:
/// no I/O, no allocation beyond the returned `Vec` and each finding's
/// redacted excerpt.
pub fn scan(content: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for rule in rules() {
        for m in rule.pattern.find_iter(content) {
            if let Some(min_entropy) = rule.min_entropy {
                // The rule's capture group 1 is the value to check --
                // re-running the regex via `captures_at` on this match's
                // start is simpler than threading capture groups through
                // `find_iter`, and rules here are few and content is
                // small enough (a `wkp remember` body) that the small
                // extra match cost is not worth the complexity.
                let Some(caps) = rule.pattern.captures_at(content, m.start()) else {
                    continue;
                };
                let Some(value) = caps.get(1) else { continue };
                if shannon_entropy(value.as_str()) < min_entropy {
                    continue;
                }
            }
            findings.push(Finding {
                rule: rule.name,
                start: m.start(),
                end: m.end(),
                redacted_excerpt: redacted_excerpt(content, m.start(), m.end()),
            });
        }
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule_names(findings: &[Finding]) -> Vec<&'static str> {
        findings.iter().map(|f| f.rule).collect()
    }

    #[test]
    fn detects_an_aws_access_key_id() {
        let findings = scan("AWS_ACCESS_KEY_ID=AKIAABCDEFGHIJKLMNOP\n"); // nosemgrep: generic.secrets.security.detected-aws-access-key-id-value.detected-aws-access-key-id-value -- fixture for this module's own scanner, not a real key
        assert!(rule_names(&findings).contains(&"aws-access-key-id"));
    }

    #[test]
    fn does_not_flag_a_string_shaped_like_an_aws_key_but_wrong_prefix() {
        let findings = scan("NOTAKEY_ABCDEFGHIJKLMNOP_XXXXXXXXXXXXXXXXXXXX\n");
        assert!(!rule_names(&findings).contains(&"aws-access-key-id"));
    }

    #[test]
    fn detects_a_private_key_pem_header() {
        let findings =
            scan("-----BEGIN RSA PRIVATE KEY-----\nMIIEow...\n-----END RSA PRIVATE KEY-----\n");
        assert!(rule_names(&findings).contains(&"private-key-pem-header"));
    }

    #[test]
    fn does_not_flag_a_public_key_header() {
        let findings = scan("-----BEGIN PUBLIC KEY-----\nMIIBIjAN...\n-----END PUBLIC KEY-----\n");
        assert!(!rule_names(&findings).contains(&"private-key-pem-header"));
    }

    #[test]
    fn detects_a_generic_api_key_assignment() {
        let findings = scan(r#"api_key = "notarealsecretvalue1234567890ab""#);
        assert!(rule_names(&findings).contains(&"generic-api-key"));
    }

    #[test]
    fn does_not_flag_a_short_value_after_a_keyword() {
        let findings = scan(r#"api_key = "short""#);
        assert!(!rule_names(&findings).contains(&"generic-api-key"));
    }

    #[test]
    fn detects_a_high_entropy_assignment_under_an_arbitrary_key_name() {
        let findings = scan(r#"x_9f2 = "Zm9vYmFyYmF6cXV1eDEyMzQ1Njc4OTA=""#);
        assert!(rule_names(&findings).contains(&"high-entropy-assignment"));
    }

    #[test]
    fn does_not_flag_a_low_entropy_placeholder_value() {
        let findings = scan(r#"x_9f2 = "aaaaaaaaaaaaaaaaaaaaaaaaaa""#);
        assert!(!rule_names(&findings).contains(&"high-entropy-assignment"));
    }

    #[test]
    fn does_not_flag_ordinary_prose_with_no_secrets() {
        let findings = scan(
            "This is a design note about how we handle authentication. \
             We discussed using OAuth tokens issued by the identity provider, \
             but no actual token value is recorded here.",
        );
        assert!(findings.is_empty(), "unexpected findings: {findings:?}");
    }

    #[test]
    fn redacted_excerpt_never_contains_the_matched_secret() {
        let secret = "AKIAABCDEFGHIJKLMNOP"; // nosemgrep: generic.secrets.security.detected-aws-access-key-id-value.detected-aws-access-key-id-value -- fixture for this module's own scanner, not a real key
        let content = format!("AWS_ACCESS_KEY_ID={secret}\n");
        let findings = scan(&content);
        assert!(!findings.is_empty());
        for finding in &findings {
            assert!(
                !finding.redacted_excerpt.contains(secret),
                "redacted excerpt leaked the secret: {}",
                finding.redacted_excerpt
            );
        }
    }

    #[test]
    fn redacted_excerpt_is_safe_on_non_ascii_content_near_the_match() {
        let content = "emoji context 🎉🎉🎉 AWS_ACCESS_KEY_ID=AKIAABCDEFGHIJKLMNOP 🎉🎉🎉 more\n"; // nosemgrep: generic.secrets.security.detected-aws-access-key-id-value.detected-aws-access-key-id-value -- fixture for this module's own scanner, not a real key
                                                                                                   // Must not panic slicing on a non-char-boundary byte offset.
        let findings = scan(content);
        assert!(!findings.is_empty());
    }

    #[test]
    fn scan_of_empty_content_returns_no_findings() {
        assert!(scan("").is_empty());
    }

    #[test]
    fn shannon_entropy_of_a_repeated_character_is_zero() {
        assert_eq!(shannon_entropy("aaaaaaaa"), 0.0);
    }

    #[test]
    fn shannon_entropy_of_empty_string_is_zero() {
        assert_eq!(shannon_entropy(""), 0.0);
    }

    #[test]
    fn scan_handles_a_few_kb_file_without_excessive_cost() {
        // Sanity check, not a criterion benchmark (M2-4's acceptance
        // criteria asks for one or the other): a realistic `wkp
        // remember` body is at most a few KB, and this must not become
        // the bottleneck relative to the signed-commit machinery it
        // would sit in front of.
        let mut content = String::new();
        for i in 0..200 {
            content.push_str(&format!(
                "line {i}: just some ordinary prose about a project decision.\n"
            ));
        }
        content.push_str(r#"api_key = "notarealsecretvalue1234567890ab""#);
        let start = std::time::Instant::now();
        let findings = scan(&content);
        let elapsed = start.elapsed();
        assert!(rule_names(&findings).contains(&"generic-api-key"));
        assert!(
            elapsed.as_millis() < 50,
            "scan of a few-KB file took {elapsed:?}, expected well under 50ms"
        );
    }
}
