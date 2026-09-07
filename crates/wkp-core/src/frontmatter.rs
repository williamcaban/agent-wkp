//! OKF frontmatter parsing (design 5.4).
//!
//! The store's markdown files carry a YAML-*like* frontmatter block between
//! two `---` delimiter lines. This parser only ever needs the small, fixed
//! schema in design 5.4 (never arbitrary YAML), so it is a hand-rolled,
//! tolerant scanner rather than a general YAML parser: no new dependency,
//! and "tolerant of missing or malformed frontmatter" (M1-1 acceptance
//! criteria) is a parser-level guarantee rather than something layered on
//! top of a strict parser's errors.
//!
//! [`parse`] never fails and never panics on any UTF-8 input: missing,
//! empty, or malformed frontmatter degrades to [`Frontmatter::default`]
//! plus a human-readable warning, and the original body is always
//! recoverable.

use std::fmt;

/// The seven `type` values from design 5.4, plus a forward-compatible
/// fallback so an unrecognized value (e.g. a type added by a later,
/// additive-only frontmatter change per CLAUDE.md) is preserved rather than
/// discarded or treated as a parse error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemType {
    ProjectState,
    Knowledge,
    Reference,
    Feedback,
    Skill,
    Instruction,
    Memory,
    Other(String),
}

impl ItemType {
    fn parse(raw: &str) -> Self {
        match raw {
            "project-state" => ItemType::ProjectState,
            "knowledge" => ItemType::Knowledge,
            "reference" => ItemType::Reference,
            "feedback" => ItemType::Feedback,
            "skill" => ItemType::Skill,
            "instruction" => ItemType::Instruction,
            "memory" => ItemType::Memory,
            other => ItemType::Other(other.to_string()),
        }
    }
}

/// `scope` (design 5.4): who the item is about or for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    User,
    Project,
    Org,
    Other(String),
}

impl Scope {
    fn parse(raw: &str) -> Self {
        match raw {
            "user" => Scope::User,
            "project" => Scope::Project,
            "org" => Scope::Org,
            other => Scope::Other(other.to_string()),
        }
    }
}

/// `visibility` (design 5.4 / 7.2): gates client-side encryption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    Shared,
    Private,
    Other(String),
}

impl Visibility {
    fn parse(raw: &str) -> Self {
        match raw {
            "shared" => Visibility::Shared,
            "private" => Visibility::Private,
            other => Visibility::Other(other.to_string()),
        }
    }
}

/// `confidence` (design 5.4 / 7.4): mirrors "did the user say it".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confidence {
    Stated,
    Inferred,
    Proposed,
    Other(String),
}

impl Confidence {
    fn parse(raw: &str) -> Self {
        match raw {
            "stated" => Confidence::Stated,
            "inferred" => Confidence::Inferred,
            "proposed" => Confidence::Proposed,
            other => Confidence::Other(other.to_string()),
        }
    }
}

/// `provenance.source` (design 5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKind {
    Conversation,
    File,
    Tool,
    Import,
    Other(String),
}

impl SourceKind {
    fn parse(raw: &str) -> Self {
        match raw {
            "conversation" => SourceKind::Conversation,
            "file" => SourceKind::File,
            "tool" => SourceKind::Tool,
            "import" => SourceKind::Import,
            other => SourceKind::Other(other.to_string()),
        }
    }
}

/// `provenance` (design 5.4, new in v2): who wrote the item, from where.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provenance {
    /// `human:<id>` or `agent:<harness>[:<model>]`.
    pub actor: Option<String>,
    /// Opaque, harness-provided session identifier.
    pub session: Option<String>,
    pub source: Option<SourceKind>,
}

/// The full OKF frontmatter (design 5.4): the original agent-wkp fields
/// plus the v2 additions (`scope`, `provenance`, `confidence`, `expires`,
/// and the widened `type` enum). Every field is optional or defaulted:
/// there is no required field, because the parser must tolerate frontmatter
/// written before a field existed, or written by hand and missing one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
    pub title: Option<String>,
    pub item_type: Option<ItemType>,
    pub scope: Option<Scope>,
    pub workspace: Option<String>,
    pub visibility: Option<Visibility>,
    pub provenance: Provenance,
    pub confidence: Option<Confidence>,
    /// Raw ISO date string, or `None` for a literal `null`/missing value.
    /// Kept as the source text rather than parsed into a date type: the
    /// parser's job is tolerance, not calendar validation, and adding a
    /// date-handling dependency for this alone would violate the slim-core
    /// rule (CLAUDE.md).
    pub expires: Option<String>,
    /// The value as written (e.g. `~120`), preserved for display.
    pub tokens_raw: Option<String>,
    /// The digits from `tokens_raw`, parsed if present.
    pub tokens: Option<u32>,
    pub tags: Vec<String>,
    pub refs: Vec<String>,
    pub updated: Option<String>,
}

/// The result of parsing a whole markdown file: the frontmatter (possibly
/// all-default), the body with the frontmatter block removed, and any
/// warnings about malformed input encountered along the way. There is
/// deliberately no `Err` case; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResult {
    pub frontmatter: Frontmatter,
    pub body: String,
    pub warnings: Vec<String>,
}

impl fmt::Display for ParseResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.frontmatter)
    }
}

/// Parses a markdown file's leading frontmatter block, if any.
///
/// Never panics on any valid UTF-8 input, and always returns a usable
/// [`ParseResult`]: missing or malformed frontmatter yields
/// `Frontmatter::default()` (or a best-effort partial parse) plus a
/// warning, never a fatal error.
pub fn parse(input: &str) -> ParseResult {
    let mut warnings = Vec::new();

    let Some(after_open) = strip_opening_delimiter(input) else {
        // No frontmatter block at all: the whole input is body. This is a
        // normal, expected case (a plain markdown file), not a warning.
        return ParseResult {
            frontmatter: Frontmatter::default(),
            body: input.to_string(),
            warnings,
        };
    };

    let Some((block, body)) = split_at_closing_delimiter(after_open) else {
        warnings.push(
            "unterminated frontmatter block: no closing `---` line found; \
             treating entire file as body"
                .to_string(),
        );
        return ParseResult {
            frontmatter: Frontmatter::default(),
            body: input.to_string(),
            warnings,
        };
    };

    let frontmatter = parse_block(block, &mut warnings);
    ParseResult {
        frontmatter,
        body: body.to_string(),
        warnings,
    }
}

/// If `input` opens with a `---` delimiter line, returns the remainder
/// after that line. A delimiter line is exactly `---`, optionally
/// surrounded by trailing whitespace/CR.
fn strip_opening_delimiter(input: &str) -> Option<&str> {
    let mut lines = input.split_inclusive('\n');
    let first = lines.next()?;
    if is_delimiter_line(first) {
        Some(&input[first.len()..])
    } else {
        None
    }
}

fn is_delimiter_line(line: &str) -> bool {
    line.trim_end_matches(['\n', '\r']).trim() == "---"
}

/// Splits `after_open` at the first delimiter line, returning
/// `(frontmatter_block, body_after_delimiter)`. Returns `None` if no
/// closing delimiter is found.
fn split_at_closing_delimiter(after_open: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        if is_delimiter_line(line) {
            let block = &after_open[..offset];
            let body = &after_open[offset + line.len()..];
            return Some((block, body));
        }
        offset += line.len();
    }
    None
}

/// One logical line inside the frontmatter block, with its indentation
/// (in spaces) already measured.
struct Line<'a> {
    indent: usize,
    content: &'a str,
}

fn logical_lines(block: &str) -> Vec<Line<'_>> {
    block
        .lines()
        .filter_map(|raw| {
            let content = raw.trim_end_matches('\r');
            let trimmed = content.trim_start_matches(' ');
            let indent = content.len() - trimmed.len();
            let trimmed = trimmed.trim_end();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                None
            } else {
                Some(Line {
                    indent,
                    content: trimmed,
                })
            }
        })
        .collect()
}

fn parse_block(block: &str, warnings: &mut Vec<String>) -> Frontmatter {
    let lines = logical_lines(block);
    let mut fm = Frontmatter::default();

    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        if line.indent != 0 {
            // Stray indented line with no owning top-level key (e.g. a
            // corrupted block); skip it rather than fail the whole parse.
            warnings.push(format!(
                "ignoring unexpected indented line: {:?}",
                line.content
            ));
            i += 1;
            continue;
        }

        let Some((key, rest)) = split_key_value(line.content) else {
            warnings.push(format!("ignoring line with no `key:`: {:?}", line.content));
            i += 1;
            continue;
        };

        if rest.is_empty() {
            // Either a nested map (`provenance:`) or a block list
            // (`tags:` / `refs:`) follows, indented under this key.
            let mut j = i + 1;
            let mut children = Vec::new();
            while j < lines.len() && lines[j].indent > 0 {
                children.push(&lines[j]);
                j += 1;
            }
            apply_block_value(&mut fm, key, &children, warnings);
            i = j;
        } else {
            apply_scalar_value(&mut fm, key, rest, warnings);
            i += 1;
        }
    }

    fm
}

/// Splits `"key: value"` into `("key", "value")`. `value` is `""` when the
/// key has no inline value (a nested map or block list follows).
fn split_key_value(content: &str) -> Option<(&str, &str)> {
    let (key, rest) = content.split_once(':')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key, rest.trim()))
}

fn unquote(value: &str) -> &str {
    let v = value.trim();
    for quote in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(quote) && v.ends_with(quote) {
            return &v[1..v.len() - 1];
        }
    }
    v
}

fn is_null(value: &str) -> bool {
    matches!(value, "null" | "~" | "")
}

/// Parses an inline list `[a, b, c]` (or `[]`) into its trimmed,
/// unquoted items. Returns `None` if `value` isn't bracketed.
fn parse_inline_list(value: &str) -> Option<Vec<String>> {
    let inner = value.strip_prefix('[')?.strip_suffix(']')?;
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    Some(
        inner
            .split(',')
            .map(|item| unquote(item.trim()).to_string())
            .filter(|item| !item.is_empty())
            .collect(),
    )
}

fn parse_tokens(raw: &str) -> Option<u32> {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

fn apply_scalar_value(fm: &mut Frontmatter, key: &str, value: &str, warnings: &mut Vec<String>) {
    let unquoted = unquote(value);
    match key {
        "title" => fm.title = opt_string(unquoted),
        "type" => fm.item_type = opt_string(unquoted).map(|s| ItemType::parse(&s)),
        "scope" => fm.scope = opt_string(unquoted).map(|s| Scope::parse(&s)),
        "workspace" => fm.workspace = opt_string(unquoted),
        "visibility" => fm.visibility = opt_string(unquoted).map(|s| Visibility::parse(&s)),
        "confidence" => fm.confidence = opt_string(unquoted).map(|s| Confidence::parse(&s)),
        "expires" => fm.expires = opt_string(unquoted),
        "updated" => fm.updated = opt_string(unquoted),
        "tokens" => {
            if is_null(unquoted) {
                fm.tokens_raw = None;
                fm.tokens = None;
            } else {
                fm.tokens_raw = Some(unquoted.to_string());
                fm.tokens = parse_tokens(unquoted);
            }
        }
        "tags" => fm.tags = parse_inline_list(unquoted).unwrap_or_else(|| single_item(unquoted)),
        "refs" => fm.refs = parse_inline_list(unquoted).unwrap_or_else(|| single_item(unquoted)),
        "actor" | "session" | "source" => {
            // A `provenance.*` field written at top level (malformed
            // indentation): still capture it rather than discard it.
            warnings.push(format!(
                "`{key}` found at top level, expected under `provenance:`; using it anyway"
            ));
            apply_provenance_field(&mut fm.provenance, key, unquoted);
        }
        _ => {
            // Unknown field: frontmatter fields are additive-only
            // (CLAUDE.md), so an unrecognized key is expected forward
            // compatibility, not an error.
        }
    }
}

fn single_item(value: &str) -> Vec<String> {
    if is_null(value) {
        Vec::new()
    } else {
        vec![value.to_string()]
    }
}

fn opt_string(value: &str) -> Option<String> {
    if is_null(value) {
        None
    } else {
        Some(value.to_string())
    }
}

fn apply_provenance_field(prov: &mut Provenance, key: &str, value: &str) {
    match key {
        "actor" => prov.actor = opt_string(value),
        "session" => prov.session = opt_string(value),
        "source" => prov.source = opt_string(value).map(|s| SourceKind::parse(&s)),
        _ => {}
    }
}

fn apply_block_value(
    fm: &mut Frontmatter,
    key: &str,
    children: &[&Line<'_>],
    warnings: &mut Vec<String>,
) {
    match key {
        "provenance" => {
            for child in children {
                match split_key_value(child.content) {
                    Some((k, v)) => apply_provenance_field(&mut fm.provenance, k, unquote(v)),
                    None => warnings.push(format!(
                        "ignoring malformed `provenance` line: {:?}",
                        child.content
                    )),
                }
            }
        }
        "tags" | "refs" => {
            let items: Vec<String> = children
                .iter()
                .filter_map(|child| child.content.strip_prefix('-'))
                .map(|item| unquote(item.trim()).to_string())
                .filter(|item| !item.is_empty())
                .collect();
            if key == "tags" {
                fm.tags = items;
            } else {
                fm.refs = items;
            }
        }
        _ => {
            // Unknown nested block: additive-only forward compatibility,
            // same reasoning as the unknown scalar-key case.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_EXAMPLE: &str = r#"---
title: Postgres chosen for control plane
type: knowledge
scope: project
workspace: wkp-hub
visibility: private
provenance:
  actor: agent:claude-code
  session: 7f3c
  source: conversation
confidence: stated
expires: null
tokens: ~120
tags: [architecture, decision]
refs: []
updated: 2026-09-05
---
Body text follows.
"#;

    #[test]
    fn parses_full_v2_frontmatter() {
        let result = parse(FULL_EXAMPLE);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let fm = result.frontmatter;
        assert_eq!(
            fm.title.as_deref(),
            Some("Postgres chosen for control plane")
        );
        assert_eq!(fm.item_type, Some(ItemType::Knowledge));
        assert_eq!(fm.scope, Some(Scope::Project));
        assert_eq!(fm.workspace.as_deref(), Some("wkp-hub"));
        assert_eq!(fm.visibility, Some(Visibility::Private));
        assert_eq!(fm.provenance.actor.as_deref(), Some("agent:claude-code"));
        assert_eq!(fm.provenance.session.as_deref(), Some("7f3c"));
        assert_eq!(fm.provenance.source, Some(SourceKind::Conversation));
        assert_eq!(fm.confidence, Some(Confidence::Stated));
        assert_eq!(fm.expires, None);
        assert_eq!(fm.tokens_raw.as_deref(), Some("~120"));
        assert_eq!(fm.tokens, Some(120));
        assert_eq!(fm.tags, vec!["architecture", "decision"]);
        assert!(fm.refs.is_empty());
        assert_eq!(fm.updated.as_deref(), Some("2026-09-05"));
        assert_eq!(result.body, "Body text follows.\n");
    }

    #[test]
    fn missing_frontmatter_is_not_a_warning() {
        let result = parse("Just a plain markdown file.\n");
        assert_eq!(result.frontmatter, Frontmatter::default());
        assert!(result.warnings.is_empty());
        assert_eq!(result.body, "Just a plain markdown file.\n");
    }

    #[test]
    fn empty_frontmatter_block() {
        let result = parse("---\n---\nBody.\n");
        assert_eq!(result.frontmatter, Frontmatter::default());
        assert!(result.warnings.is_empty());
        assert_eq!(result.body, "Body.\n");
    }

    #[test]
    fn unknown_extra_fields_are_ignored_not_fatal() {
        let input = "---\ntitle: T\nfrobnicate: yes\nfuture_field: [1, 2]\n---\nBody\n";
        let result = parse(input);
        assert!(result.warnings.is_empty());
        assert_eq!(result.frontmatter.title.as_deref(), Some("T"));
    }

    #[test]
    fn unterminated_frontmatter_falls_back_to_whole_body() {
        let input = "---\ntitle: T\nno closing delimiter here\n";
        let result = parse(input);
        assert_eq!(result.frontmatter, Frontmatter::default());
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.body, input);
    }

    #[test]
    fn malformed_line_without_colon_is_skipped_not_fatal() {
        let input = "---\nthis has no colon\ntitle: T\n---\nBody\n";
        let result = parse(input);
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.frontmatter.title.as_deref(), Some("T"));
    }

    #[test]
    fn stray_indentation_is_skipped_not_fatal() {
        let input = "---\n  stray: indented\ntitle: T\n---\nBody\n";
        let result = parse(input);
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.frontmatter.title.as_deref(), Some("T"));
    }

    #[test]
    fn block_style_lists() {
        let input =
            "---\ntags:\n  - architecture\n  - decision\nrefs:\n  - path/one.md\n---\nBody\n";
        let result = parse(input);
        assert!(result.warnings.is_empty());
        assert_eq!(result.frontmatter.tags, vec!["architecture", "decision"]);
        assert_eq!(result.frontmatter.refs, vec!["path/one.md"]);
    }

    #[test]
    fn all_item_type_values_parse() {
        for (raw, expected) in [
            ("project-state", ItemType::ProjectState),
            ("knowledge", ItemType::Knowledge),
            ("reference", ItemType::Reference),
            ("feedback", ItemType::Feedback),
            ("skill", ItemType::Skill),
            ("instruction", ItemType::Instruction),
            ("memory", ItemType::Memory),
        ] {
            let input = format!("---\ntype: {raw}\n---\nBody\n");
            let result = parse(&input);
            assert_eq!(result.frontmatter.item_type, Some(expected), "raw={raw}");
        }
    }

    #[test]
    fn unrecognized_type_value_is_preserved_not_dropped() {
        let input = "---\ntype: some-future-type\n---\nBody\n";
        let result = parse(input);
        assert_eq!(
            result.frontmatter.item_type,
            Some(ItemType::Other("some-future-type".to_string()))
        );
    }

    #[test]
    fn provenance_written_flat_at_top_level_still_captured() {
        let input = "---\nactor: human:william\nsession: abc\nsource: file\n---\nBody\n";
        let result = parse(input);
        assert_eq!(result.warnings.len(), 3);
        assert_eq!(
            result.frontmatter.provenance.actor.as_deref(),
            Some("human:william")
        );
        assert_eq!(result.frontmatter.provenance.source, Some(SourceKind::File));
    }

    #[test]
    fn crlf_line_endings_are_handled() {
        let input = "---\r\ntitle: T\r\n---\r\nBody\r\n";
        let result = parse(input);
        assert!(result.warnings.is_empty());
        assert_eq!(result.frontmatter.title.as_deref(), Some("T"));
    }

    #[test]
    fn never_panics_on_arbitrary_strings() {
        let inputs = [
            "",
            "-",
            "--",
            "---",
            "---\n",
            "---\n---",
            "\0\0\0",
            "---\nkey\n---\n",
            "---\n:\n---\n",
            "---\n\t\ttitle:\ttabbed\n---\n",
            "title: 日本語のタイトル\n---\n",
            "---\ntags: [\n---\n",
            &"a".repeat(10_000),
            &format!("---\n{}\n---\nBody\n", "x".repeat(5_000)),
        ];
        for input in inputs {
            let _ = parse(input);
        }
    }

    // Cheap, dependency-free fuzz-style check: exercised locally and by
    // `fuzz/fuzz_targets/frontmatter.rs` under cargo-fuzz for continuous
    // coverage (design 9.4). This keeps a fast regression check in the
    // normal `cargo test` path without requiring a nightly toolchain.
    #[test]
    fn never_panics_on_pseudo_random_byte_soup() {
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = (next() % 300) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| (next() % 256) as u8).collect();
            let input = String::from_utf8_lossy(&bytes).into_owned();
            let _ = parse(&input);
        }
    }
}
