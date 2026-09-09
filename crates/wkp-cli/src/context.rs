//! `wkp context`/`wkp traverse`: graph-aware retrieval built on top of
//! `wkp search`'s options and formatting (design 5.1/5.4, M1-5).

use std::path::PathBuf;

use crate::search::{format_json, format_paths, format_text, SearchFormat, SearchOptions};

/// `wkp context <topic>`: BM25 search plus graph traversal from the top
/// hits (design 5.1/5.4, M1-5), combined under one token budget. Reuses
/// `parse_search_args`/`SearchOptions` -- `context`'s "topic" plays the
/// same positional-argument role as `search`'s "query", and both take the
/// same `--tier`/`--budget`/`--format` flags. `--embed-url` is parsed (it's
/// a `SearchOptions` field) but not supported here -- M1-9 scoped hybrid
/// search to `wkp search` only; combining it with `context`'s own graph
/// traversal is deliberately left to a later task rather than silently
/// ignoring the flag.
pub(crate) fn run_context(opts: &SearchOptions) -> Result<String, String> {
    if opts.embed_url.is_some() {
        return Err(
            "--embed-url is not supported by `wkp context` yet (only `wkp search`)".to_string(),
        );
    }
    let index_path = opts.path.join(".wkp/index.db");
    let conn = wkp_core::index::open_index(&index_path).map_err(|e| e.to_string())?;
    let filter = wkp_core::index::SearchFilter {
        tier: opts.tier,
        budget: opts.budget,
        ..Default::default()
    };
    let hits = wkp_core::index::context(&conn, &opts.query, &filter).map_err(|e| e.to_string())?;
    Ok(match opts.format {
        SearchFormat::Text => format_text(&hits),
        SearchFormat::Paths => format_paths(&hits),
        SearchFormat::Json => format_json(&hits),
    })
}

pub(crate) struct TraverseOptions {
    pub(crate) path: PathBuf,
    pub(crate) start_path: String,
    pub(crate) depth: u32,
    pub(crate) format: SearchFormat,
}

/// Parses `wkp traverse <path> [--depth N] [--format text|paths|json]
/// [--path DIR]`. `--depth` defaults to 2, matching `context`'s own
/// traversal depth from a search hit.
pub(crate) fn parse_traverse_args(
    mut args: impl Iterator<Item = String>,
) -> Result<TraverseOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut start_path: Option<String> = None;
    let mut depth: u32 = 2;
    let mut format = SearchFormat::Text;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--depth" => {
                let v = args.next().ok_or("--depth requires a value")?;
                depth = v
                    .parse::<u32>()
                    .map_err(|_| format!("invalid --depth value: {v}"))?;
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
            other if start_path.is_none() && !other.starts_with('-') => {
                start_path = Some(other.to_string());
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let start_path = start_path.ok_or_else(|| {
        "traverse requires a starting path, e.g. `wkp traverse memory/foo.md`".to_string()
    })?;
    Ok(TraverseOptions {
        path,
        start_path,
        depth,
        format,
    })
}

/// `wkp traverse <path>`: follows explicit `refs:`/`[[wikilink]]` edges
/// from `path` up to `--depth` hops (design 5.1, M1-5), without a search
/// query.
pub(crate) fn run_traverse(opts: &TraverseOptions) -> Result<String, String> {
    let index_path = opts.path.join(".wkp/index.db");
    let conn = wkp_core::index::open_index(&index_path).map_err(|e| e.to_string())?;
    let hits = wkp_core::index::traverse(&conn, &opts.start_path, opts.depth)
        .map_err(|e| e.to_string())?;
    Ok(match opts.format {
        SearchFormat::Text => format_text(&hits),
        SearchFormat::Paths => format_paths(&hits),
        SearchFormat::Json => format_json(&hits),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{args, search_opts, temp_dir, test_init};

    #[test]
    fn run_context_rejects_embed_url() {
        let temp = temp_dir("context-rejects-embed-url");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let mut opts = search_opts(dir, "anything", SearchFormat::Text);
        opts.embed_url = Some("http://localhost:11434/v1".to_string());
        let result = run_context(&opts);
        assert!(
            result.is_err(),
            "wkp context must reject --embed-url (M1-9 scoped hybrid search to wkp search only)"
        );
    }

    #[test]
    fn run_context_combines_search_and_traversal() {
        let temp = temp_dir("run-context");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::write(
            dir.join("hit.md"),
            "---\ntitle: Hit\ntype: knowledge\nrefs: [neighbor.md]\n---\n\ndistinctive topic\n",
        )
        .expect("write hit.md");
        std::fs::write(
            dir.join("neighbor.md"),
            "---\ntitle: Neighbor\n---\n\nreached only via refs\n",
        )
        .expect("write neighbor.md");
        wkp_git::commit_all(dir, "seed").expect("commit_all");
        crate::index_cmd::run_index(dir).expect("run_index");

        let output = run_context(&search_opts(dir, "distinctive", SearchFormat::Paths))
            .expect("run_context");
        let mut paths: Vec<&str> = output.lines().collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["hit.md", "neighbor.md"]);
    }

    #[test]
    fn parse_traverse_args_reads_path_and_flags() {
        let opts = parse_traverse_args(args(&["memory/a.md", "--depth", "3", "--format", "json"]))
            .expect("parse_traverse_args");
        assert_eq!(opts.start_path, "memory/a.md");
        assert_eq!(opts.depth, 3);
        assert_eq!(opts.format, SearchFormat::Json);
    }

    #[test]
    fn parse_traverse_args_defaults_depth_to_two() {
        let opts = parse_traverse_args(args(&["a.md"])).expect("parse_traverse_args");
        assert_eq!(opts.depth, 2);
        assert_eq!(opts.format, SearchFormat::Text);
    }

    #[test]
    fn parse_traverse_args_rejects_missing_path() {
        assert!(parse_traverse_args(args(&["--depth", "1"])).is_err());
    }

    #[test]
    fn run_traverse_follows_refs_and_reports_hop_distance() {
        let temp = temp_dir("run-traverse");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::write(
            dir.join("a.md"),
            "---\ntitle: A\nrefs: [b.md]\n---\n\nstart\n",
        )
        .expect("write a.md");
        std::fs::write(dir.join("b.md"), "---\ntitle: B\n---\n\ntarget\n").expect("write b.md");
        wkp_git::commit_all(dir, "seed").expect("commit_all");
        crate::index_cmd::run_index(dir).expect("run_index");

        let opts = TraverseOptions {
            path: dir.to_path_buf(),
            start_path: "a.md".to_string(),
            depth: 1,
            format: SearchFormat::Text,
        };
        let output = run_traverse(&opts).expect("run_traverse");
        assert!(
            output.contains("+1"),
            "expected hop distance in output: {output}"
        );
        assert!(output.contains("b.md"));

        let json_opts = TraverseOptions {
            format: SearchFormat::Json,
            ..TraverseOptions {
                path: dir.to_path_buf(),
                start_path: "a.md".to_string(),
                depth: 1,
                format: SearchFormat::Text,
            }
        };
        let json_output = run_traverse(&json_opts).expect("run_traverse json");
        assert!(json_output.contains(r#""hop_distance":1"#));
    }
}
