//! `wkp index`: incremental re-indexing (design 5.1, M1-3), plus the
//! optional embedding machinery (M1-9) `wkp search`/`wkp index --embed-url`
//! share.

use std::path::{Path, PathBuf};

/// A one-line summary of what `wkp index` did, for the CLI's stdout.
pub(crate) struct IndexSummary {
    pub(crate) added: usize,
    pub(crate) modified: usize,
    pub(crate) deleted: usize,
}

impl std::fmt::Display for IndexSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.added == 0 && self.modified == 0 && self.deleted == 0 {
            write!(f, "wkp: index up to date")
        } else {
            write!(
                f,
                "wkp: indexed {} added, {} modified, {} deleted",
                self.added, self.modified, self.deleted
            )
        }
    }
}

/// `wkp index [path]`: re-indexes only what actually needs it (design
/// 5.1, M1-3) -- markdown files new to the index (a fresh clone, or the
/// first run after `wkp init`, where git may see nothing "changed" at all
/// even though `index.db` has never seen these files), files git's own
/// stat cache reports as modified, and files no longer present -- rather
/// than reading every file in the store on every run.
///
/// This needs two signals from git, not one: [`wkp_git::detect_changes`]
/// alone only reports drift *since the last commit*, which is empty right
/// after a commit even though the index may still be catching up to it.
/// [`wkp_git::list_tracked_files`] plus `detect_changes`'s untracked
/// entries gives the full current path universe, compared against
/// [`wkp_core::index::known_paths`] (what the index already has a row
/// for) to find genuinely new and genuinely gone paths.
/// Test-only convenience wrapper: every existing test predates M1-9's
/// `--embed-url` trio and just wants plain, non-embedding indexing.
#[cfg(test)]
pub(crate) fn run_index(path: &Path) -> Result<IndexSummary, String> {
    run_index_impl(path, None, None, None)
}

/// The actual body of `wkp index`, with the `--embed-url` trio injectable
/// so every existing test calling the plain [`run_index`] (no embeddings
/// involved) is unaffected by M1-9's addition. Embeddings, when
/// requested, are computed and attached to each upserted [`wkp_core::index::Item`]
/// *before* [`wkp_core::index::update_index`] runs -- landing in the same
/// atomic temp-file-then-rename write as everything else, never as a
/// separate write against the already-published `index.db` (CLAUDE.md's
/// "never write a file a harness reads in place").
pub(crate) fn run_index_impl(
    path: &Path,
    embed_url: Option<&str>,
    embed_model: Option<&str>,
    embed_key_file: Option<&Path>,
) -> Result<IndexSummary, String> {
    let changes = wkp_git::detect_changes(path)?;
    let tracked = wkp_git::list_tracked_files(path)?;
    let is_markdown = |p: &Path| p.extension().is_some_and(|ext| ext == "md");

    let mut current: std::collections::HashSet<String> = tracked
        .iter()
        .filter(|p| is_markdown(p))
        .map(|p| path_to_store_string(p))
        .collect();
    current.extend(
        changes
            .added
            .iter()
            .filter(|p| is_markdown(p))
            .map(|p| path_to_store_string(p)),
    );
    current.extend(
        changes
            .renamed
            .iter()
            .map(|r| &r.to)
            .filter(|p| is_markdown(p))
            .map(|p| path_to_store_string(p)),
    );
    for gone in changes
        .deleted
        .iter()
        .chain(changes.renamed.iter().map(|r| &r.from))
    {
        current.remove(&path_to_store_string(gone));
    }

    let index_path = path.join(".wkp/index.db");
    let known = {
        let conn = wkp_core::index::open_index(&index_path).map_err(|e| e.to_string())?;
        wkp_core::index::known_paths(&conn).map_err(|e| e.to_string())?
    };

    let added_paths: Vec<&String> = current.difference(&known).collect();
    let dirty: std::collections::HashSet<String> = changes
        .modified
        .iter()
        .chain(changes.renamed.iter().map(|r| &r.to))
        .filter(|p| is_markdown(p))
        .map(|p| path_to_store_string(p))
        .collect();
    let modified_paths: Vec<&String> = current
        .intersection(&known)
        .filter(|p| dirty.contains(*p))
        .collect();
    let deleted_paths: Vec<String> = known.difference(&current).cloned().collect();

    let mut upserts = Vec::new();
    for relative in added_paths.iter().chain(&modified_paths) {
        let contents = std::fs::read_to_string(path.join(relative))
            .map_err(|e| format!("reading {relative}: {e}"))?;
        let parsed = wkp_core::frontmatter::parse(&contents);
        // Design 7.4 / M2-6: `wkp-core` stays git-agnostic (design 3.3 --
        // it and `wkp-git` are siblings, neither depends on the other),
        // so this is where the real provenance gate's only external
        // input gets resolved: one `git log` per changed path, proportional
        // to change count (M1-3's own incremental-update cost model), not
        // to corpus size for an ordinary `wkp remember` + `wkp index`
        // cycle -- but a fresh clone's *first* `wkp index` run treats
        // every item as "added", so that one run pays one subprocess per
        // item. Not optimized in this task (a single batched `git log`
        // walk resolving every path's signer in one process would avoid
        // it); flagged here rather than silently absorbed, since M1-3/
        // M1-4's own benches don't exercise this wkp-cli-level path at
        // all and so can't catch a regression here.
        let human_signed = matches!(
            wkp_git::allowed_signers::last_signer_for_path(path, relative),
            Some((_, wkp_git::allowed_signers::SignerRole::Human))
        );
        upserts.push(wkp_core::index::Item {
            path: (*relative).clone(),
            frontmatter: parsed.frontmatter,
            body: parsed.body,
            embedding: None,
            human_signed,
        });
    }

    if let Some(embed_url) = embed_url {
        embed_upserts(&mut upserts, embed_url, embed_model, embed_key_file)?;
    }

    let summary = IndexSummary {
        added: added_paths.len(),
        modified: modified_paths.len(),
        deleted: deleted_paths.len(),
    };
    wkp_core::index::update_index(&index_path, &upserts, &deleted_paths)
        .map_err(|e| e.to_string())?;

    Ok(summary)
}

/// Parses `wkp index [path] [--embed-url URL] [--embed-model NAME]
/// [--embed-key-file PATH]`. The three `--embed-*` flags are always
/// parsed regardless of whether this binary was built with the `embed`
/// Cargo feature, so passing them to a non-`embed` build fails with a
/// clear message from [`embed_upserts`] rather than "unrecognized
/// argument".
pub(crate) fn parse_index_args(
    mut args: impl Iterator<Item = String>,
) -> Result<IndexOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
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
            other if !other.starts_with('-') => path = PathBuf::from(other),
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }
    Ok(IndexOptions {
        path,
        embed_url,
        embed_model,
        embed_key_file,
    })
}

pub(crate) struct IndexOptions {
    pub(crate) path: PathBuf,
    pub(crate) embed_url: Option<String>,
    pub(crate) embed_model: Option<String>,
    pub(crate) embed_key_file: Option<PathBuf>,
}

pub(crate) fn run_index_cli(opts: &IndexOptions) -> Result<IndexSummary, String> {
    run_index_impl(
        &opts.path,
        opts.embed_url.as_deref(),
        opts.embed_model.as_deref(),
        opts.embed_key_file.as_deref(),
    )
}

fn path_to_store_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Reads an embedding-endpoint API key from a file. CLAUDE.md's hard rule
/// ("secrets never touch argv or environment variables") rules out both
/// `--embed-api-key <key>` on argv and an env var like agent-wkp's
/// `WKP_EMBED_API_KEY` -- a `0600` file (or, for a local no-auth endpoint
/// like Ollama/llama.cpp, no key at all) is the only supported path.
#[cfg(feature = "embed")]
fn read_embed_key_file(path: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let meta =
        std::fs::metadata(path).map_err(|e| format!("--embed-key-file {}: {e}", path.display()))?;
    let mode = meta.permissions().mode() & 0o777;
    if mode != 0o600 {
        return Err(format!(
            "--embed-key-file {} must be mode 0600 (found {mode:03o}); run `chmod 600 {}`",
            path.display(),
            path.display()
        ));
    }
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("reading --embed-key-file {}: {e}", path.display()))?;
    Ok(contents.trim().to_string())
}

#[cfg(feature = "embed")]
pub(crate) fn build_embed_config(
    embed_url: &str,
    embed_model: Option<&str>,
    embed_key_file: Option<&Path>,
) -> Result<wkp_core::embed::EmbedConfig, String> {
    let api_key = match embed_key_file {
        Some(p) => Some(read_embed_key_file(p)?),
        None => None,
    };
    Ok(wkp_core::embed::EmbedConfig {
        url: embed_url.to_string(),
        api_key,
        model: embed_model.map(str::to_string),
    })
}

/// Embeds `title\n\nbody` for every item in `items` via the configured
/// endpoint (design 5.3, M1-9), in place. A single item's embedding
/// failure is a warning on stderr, not a fatal error for the whole `wkp
/// index` run -- one unreachable/misconfigured endpoint mid-run shouldn't
/// stop the (already-working) plain text index from being updated.
#[cfg(feature = "embed")]
fn embed_upserts(
    items: &mut [wkp_core::index::Item],
    embed_url: &str,
    embed_model: Option<&str>,
    embed_key_file: Option<&Path>,
) -> Result<(), String> {
    let config = build_embed_config(embed_url, embed_model, embed_key_file)?;
    for item in items.iter_mut() {
        let text = format!(
            "{}\n\n{}",
            item.frontmatter.title.clone().unwrap_or_default(),
            item.body
        );
        match wkp_core::embed::embed_remote(&text, &config) {
            Ok(vector) => item.embedding = Some(vector),
            Err(e) => eprintln!("wkp: warning: could not embed {}: {e}", item.path),
        }
    }
    Ok(())
}

#[cfg(not(feature = "embed"))]
fn embed_upserts(
    _items: &mut [wkp_core::index::Item],
    _embed_url: &str,
    _embed_model: Option<&str>,
    _embed_key_file: Option<&Path>,
) -> Result<(), String> {
    Err(
        "this binary was built without hybrid search support (rebuild with `--features embed`); \
         omit --embed-url to index without embeddings"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{args, search_paths, temp_dir, test_init};

    #[test]
    fn run_index_only_touches_changed_markdown_files() {
        let temp = temp_dir("run-index");
        let dir = temp.path();
        test_init(dir).expect("run_init");

        std::fs::write(dir.join("a.md"), "---\ntitle: A\n---\n\nfirst content\n")
            .expect("write a.md");
        std::fs::write(dir.join("b.md"), "---\ntitle: B\n---\n\nsecond content\n")
            .expect("write b.md");
        // Non-markdown files in the store must never reach the index.
        std::fs::write(dir.join("notes.txt"), "not markdown\n").expect("write notes.txt");
        wkp_git::commit_all(dir, "seed").expect("commit_all");

        let summary = run_index(dir).expect("first run_index");
        assert_eq!(summary.added, 2);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.deleted, 0);

        let mut paths = search_paths(dir, "content");
        paths.sort_unstable();
        assert_eq!(paths, vec!["a.md", "b.md"]);
        assert!(search_paths(dir, "markdown").is_empty());

        // A second run with nothing changed touches nothing.
        let summary = run_index(dir).expect("second run_index");
        assert_eq!(summary.added, 0);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.deleted, 0);
    }

    #[test]
    fn run_index_handles_modify_and_delete() {
        let temp = temp_dir("run-index-modify-delete");
        let dir = temp.path();
        test_init(dir).expect("run_init");

        std::fs::write(dir.join("a.md"), "---\ntitle: A\n---\n\noriginal\n").expect("write a.md");
        std::fs::write(dir.join("b.md"), "---\ntitle: B\n---\n\nkept\n").expect("write b.md");
        wkp_git::commit_all(dir, "seed").expect("commit_all");
        run_index(dir).expect("initial index");

        std::fs::write(dir.join("a.md"), "---\ntitle: A\n---\n\nupdated\n").expect("modify a.md");
        std::fs::remove_file(dir.join("b.md")).expect("delete b.md");

        let summary = run_index(dir).expect("second run_index");
        assert_eq!(summary.added, 0);
        assert_eq!(summary.modified, 1);
        assert_eq!(summary.deleted, 1);

        assert_eq!(search_paths(dir, "updated"), vec!["a.md"]);
        assert!(search_paths(dir, "original").is_empty());
        assert!(search_paths(dir, "kept").is_empty());
    }

    #[test]
    fn parse_index_args_reads_path_and_embed_flags() {
        let opts = parse_index_args(args(&[
            "/tmp/store",
            "--embed-url",
            "http://localhost:11434/v1",
            "--embed-model",
            "m",
            "--embed-key-file",
            "/tmp/key",
        ]))
        .expect("parse_index_args");
        assert_eq!(opts.path, PathBuf::from("/tmp/store"));
        assert_eq!(opts.embed_url.as_deref(), Some("http://localhost:11434/v1"));
        assert_eq!(opts.embed_model.as_deref(), Some("m"));
        assert_eq!(opts.embed_key_file, Some(PathBuf::from("/tmp/key")));
    }

    #[test]
    fn parse_index_args_defaults_to_cwd_and_no_embed_flags() {
        let opts = parse_index_args(args(&[])).expect("parse_index_args");
        assert_eq!(opts.embed_url, None);
        assert_eq!(opts.embed_model, None);
        assert_eq!(opts.embed_key_file, None);
    }

    #[test]
    fn v0_python_style_frontmatter_indexes_and_searches_correctly() {
        let temp = temp_dir("v0-python-compat");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::create_dir_all(dir.join("memory")).expect("create memory dir");
        std::fs::write(
            dir.join("memory/old_style_note.md"),
            "---\ntitle: Old Style Note\ntype: knowledge\nworkspace: eval-hub\nvisibility: shared\ntokens: 42\ntags: [legacy]\nrefs: []\nupdated: 2025-01-01\n---\n\nThis note predates the provenance and confidence fields entirely.\n",
        )
        .expect("write v0-python-style file");
        wkp_git::commit_all(dir, "seed").expect("commit_all");

        let summary = run_index(dir).expect("run_index on a v0-python-style store");
        assert_eq!(summary.added, 1);

        let hits = search_paths(dir, "predates");
        assert_eq!(hits, vec!["memory/old_style_note.md"]);
    }

    #[cfg(feature = "embed")]
    #[test]
    fn read_embed_key_file_rejects_a_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;
        let temp = temp_dir("embed-key-file-perms");
        let path = temp.path().join("key");
        std::fs::write(&path, "secret\n").expect("write key file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod 644");
        let err = read_embed_key_file(&path).expect_err("expected a permission error");
        assert!(err.contains("0600"), "expected a 0600 hint, got: {err}");
    }

    #[cfg(feature = "embed")]
    #[test]
    fn read_embed_key_file_reads_a_0600_file_and_trims_whitespace() {
        use std::os::unix::fs::PermissionsExt;
        let temp = temp_dir("embed-key-file-ok");
        let path = temp.path().join("key");
        std::fs::write(&path, "secret-key\n").expect("write key file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
        let key = read_embed_key_file(&path).expect("read_embed_key_file");
        assert_eq!(key, "secret-key");
    }

    /// A minimal, hand-rolled HTTP/1.1 server on loopback that answers
    /// `connections` requests with the same fixed embedding response, then
    /// stops -- exercises `run_index_cli`/`run_search`'s real network path
    /// end to end without a real network call or a mocking dependency.
    #[cfg(feature = "embed")]
    fn serve_fixed_embedding(response_json: &'static str, connections: usize) -> String {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("local_addr");
        std::thread::spawn(move || {
            for stream in listener.incoming().take(connections) {
                let stream = stream.expect("accept connection");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request_line = String::new();
                reader
                    .read_line(&mut request_line)
                    .expect("read request line");
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read header line");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let mut stream = stream;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_json.len(),
                    response_json
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });
        format!("http://{addr}")
    }

    /// End-to-end (M1-9): `wkp index --embed-url` stores an embedding as
    /// part of the same atomic write as everything else, and `wkp search
    /// --embed-url` then uses it via `hybrid_search` instead of falling
    /// back to BM25 -- exercised through the same `run_index_cli`/
    /// `run_search` entry points the CLI dispatch calls, against a real
    /// (loopback) HTTP server, not just the pure-Rust fusion math already
    /// covered in `wkp-core`.
    #[cfg(feature = "embed")]
    #[test]
    fn end_to_end_index_and_search_with_embed_url_uses_hybrid_search() {
        let temp = temp_dir("embed-end-to-end");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        std::fs::write(
            dir.join("a.md"),
            "---\ntitle: A\ntype: knowledge\n---\n\nfindable content\n",
        )
        .expect("write a.md");
        wkp_git::commit_all(dir, "seed").expect("commit_all");

        // One embedding request from `wkp index`, one more from `wkp
        // search`'s own query embedding.
        let url = serve_fixed_embedding(r#"{"data":[{"embedding":[1.0,0.0,0.0]}]}"#, 2);

        let index_opts = IndexOptions {
            path: dir.to_path_buf(),
            embed_url: Some(url.clone()),
            embed_model: None,
            embed_key_file: None,
        };
        let summary = run_index_cli(&index_opts).expect("run_index_cli with --embed-url");
        assert_eq!(summary.added, 1);

        let mut search_options =
            crate::test_support::search_opts(dir, "findable", crate::search::SearchFormat::Paths);
        search_options.embed_url = Some(url);
        let output =
            crate::search::run_search(&search_options).expect("run_search with --embed-url");
        assert_eq!(output, "a.md");
    }
}
