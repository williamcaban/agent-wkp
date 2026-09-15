//! `wkp search`: BM25 full-text search over `index.db` (design 5.3), with
//! optional hybrid (BM25 + vector) search via `--embed-url` (M1-9).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchFormat {
    Text,
    Paths,
    Json,
}

pub(crate) struct SearchOptions {
    pub(crate) path: PathBuf,
    pub(crate) query: String,
    pub(crate) tier: Option<u8>,
    pub(crate) budget: Option<u32>,
    pub(crate) limit: Option<usize>,
    pub(crate) format: SearchFormat,
    /// M1-9, `wkp search` only (`wkp context` rejects it -- see
    /// `crate::context::run_context`): opts into hybrid BM25+vector
    /// search against this OpenAI-compatible embeddings endpoint.
    pub(crate) embed_url: Option<String>,
    pub(crate) embed_model: Option<String>,
    pub(crate) embed_key_file: Option<PathBuf>,
}

/// Parses `wkp search <query> [--tier N] [--budget N] [-k/--limit N]
/// [--format text|paths|json] [--path DIR]`. No `clap` dependency yet
/// (M0-1 reserved it as an option, not a requirement): this flag surface
/// is still small enough that hand-rolled parsing is less than adding and
/// vetting a production dependency would cost, per CLAUDE.md's slim-core
/// rule. Revisit once more subcommands make this genuinely unwieldy.
pub(crate) fn parse_search_args(
    mut args: impl Iterator<Item = String>,
) -> Result<SearchOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut query: Option<String> = None;
    let mut tier = None;
    let mut budget = None;
    let mut limit = None;
    let mut format = SearchFormat::Text;
    let mut embed_url = None;
    let mut embed_model = None;
    let mut embed_key_file = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--embed-url" => {
                embed_url = Some(args.next().ok_or("--embed-url requires a value")?);
            }
            "--embed-model" => {
                embed_model = Some(args.next().ok_or("--embed-model requires a value")?);
            }
            "--embed-key-file" => {
                embed_key_file = Some(PathBuf::from(
                    args.next().ok_or("--embed-key-file requires a value")?,
                ));
            }
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
        embed_url,
        embed_model,
        embed_key_file,
    })
}

/// `wkp search <query>`: BM25 full-text search over `index.db` (design
/// 5.3), with tier and token-budget filters. Never touches the network
/// unless `--embed-url` is given (design 5.3 / CLAUDE.md: no embedding
/// calls in the *default* search path) -- with it, hybrid search falls
/// back to plain BM25 on any failure, so `wkp search` itself never fails
/// just because an embedding endpoint is unreachable.
pub(crate) fn run_search(opts: &SearchOptions) -> Result<String, String> {
    let index_path = opts.path.join(".wkp/index.db");
    let conn = wkp_core::index::open_index(&index_path).map_err(|e| e.to_string())?;
    let filter = wkp_core::index::SearchFilter {
        tier: opts.tier,
        budget: opts.budget,
        limit: opts.limit,
        ..Default::default()
    };
    let hits = match &opts.embed_url {
        Some(embed_url) => run_hybrid_or_fallback(
            &conn,
            &opts.query,
            embed_url,
            opts.embed_model.as_deref(),
            opts.embed_key_file.as_deref(),
            &filter,
        )?,
        None => wkp_core::index::search(&conn, &opts.query, &filter).map_err(|e| e.to_string())?,
    };
    Ok(match opts.format {
        SearchFormat::Text => format_text(&hits),
        SearchFormat::Paths => format_paths(&hits),
        SearchFormat::Json => format_json(&hits),
    })
}

/// Runs a hybrid (BM25 + vector similarity) search, falling back to plain
/// BM25 with a stderr warning on any failure: a network error, or the
/// embedding endpoint's vector dimension not matching what's stored in
/// the index (design 5.3: "remains available... but only when the user
/// configures an endpoint", never blocking the default search path).
#[cfg(feature = "embed")]
fn run_hybrid_or_fallback(
    conn: &wkp_core::index::Connection,
    query: &str,
    embed_url: &str,
    embed_model: Option<&str>,
    embed_key_file: Option<&Path>,
    filter: &wkp_core::index::SearchFilter,
) -> Result<Vec<wkp_core::index::SearchHit>, String> {
    let config = crate::index_cmd::build_embed_config(embed_url, embed_model, embed_key_file)?;
    let fallback_warning = |detail: &dyn std::fmt::Display| {
        eprintln!(
            "wkp: warning: semantic search unavailable ({detail}); falling back to BM25 keyword search."
        );
    };
    match wkp_core::embed::embed_remote(query, &config) {
        Ok(query_vector) => {
            match wkp_core::index::hybrid_search(conn, query, &query_vector, filter) {
                Ok(hits) => return Ok(hits),
                Err(e) => fallback_warning(&e),
            }
        }
        Err(e) => fallback_warning(&e),
    }
    wkp_core::index::search(conn, query, filter).map_err(|e| e.to_string())
}

#[cfg(not(feature = "embed"))]
fn run_hybrid_or_fallback(
    _conn: &wkp_core::index::Connection,
    _query: &str,
    _embed_url: &str,
    _embed_model: Option<&str>,
    _embed_key_file: Option<&Path>,
    _filter: &wkp_core::index::SearchFilter,
) -> Result<Vec<wkp_core::index::SearchHit>, String> {
    Err(
        "this binary was built without hybrid search support (rebuild with `--features embed`); \
         omit --embed-url to use BM25 keyword search"
            .to_string(),
    )
}

pub(crate) fn format_text(hits: &[wkp_core::index::SearchHit]) -> String {
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

pub(crate) fn format_paths(hits: &[wkp_core::index::SearchHit]) -> String {
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
pub(crate) fn format_json(hits: &[wkp_core::index::SearchHit]) -> String {
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

pub(crate) fn json_string(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{args, search_opts, temp_dir, test_init};

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

    #[test]
    fn parse_search_args_reads_embed_flags() {
        let opts = parse_search_args(args(&[
            "topic",
            "--embed-url",
            "http://localhost:11434/v1",
            "--embed-model",
            "nomic-embed-text",
            "--embed-key-file",
            "/tmp/key",
        ]))
        .expect("parse_search_args");
        assert_eq!(opts.embed_url.as_deref(), Some("http://localhost:11434/v1"));
        assert_eq!(opts.embed_model.as_deref(), Some("nomic-embed-text"));
        assert_eq!(opts.embed_key_file, Some(PathBuf::from("/tmp/key")));
    }

    #[test]
    fn parse_search_args_defaults_embed_flags_to_none() {
        let opts = parse_search_args(args(&["topic"])).expect("parse_search_args");
        assert_eq!(opts.embed_url, None);
        assert_eq!(opts.embed_model, None);
        assert_eq!(opts.embed_key_file, None);
    }

    /// The default (no `embed` feature) build must still parse and accept
    /// `--embed-url` at the CLI-argument level -- it fails later, inside
    /// `run_hybrid_or_fallback`/`embed_upserts`, with a clear message,
    /// rather than at argument parsing with "unrecognized argument".
    #[test]
    #[cfg(not(feature = "embed"))]
    fn run_search_with_embed_url_on_a_non_embed_build_fails_clearly_not_as_unrecognized_arg() {
        let temp = temp_dir("search-embed-url-no-feature");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let mut opts = search_opts(dir, "anything", SearchFormat::Text);
        opts.embed_url = Some("http://localhost:11434/v1".to_string());
        let err = run_search(&opts).expect_err("expected a clear feature-not-built error");
        assert!(
            err.contains("--features embed"),
            "expected a rebuild-with-features message, got: {err}"
        );
    }

    #[test]
    fn run_search_all_three_formats() {
        let temp = temp_dir("run-search");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::write(
            dir.join("a.md"),
            "---\ntitle: A Title\ntype: knowledge\n---\n\nfindable content\n",
        )
        .expect("write a.md");
        wkp_git::commit_all(dir, "seed").expect("commit_all");
        crate::index_cmd::run_index(dir).expect("run_index");

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
        test_init(dir).expect("run_init");

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
}
