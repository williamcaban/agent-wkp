#![forbid(unsafe_code)]

//! The `wkp` binary. Subcommands (`init`, `index`, `search`, `context`,
//! `materialize`, `hooks`, `remember`, `wkpd`, ...) land starting in M1;
//! see `docs/plan/milestones.md`.

use std::path::{Path, PathBuf};

mod bundle;
mod context;
mod filter;
mod forget;
mod hooks;
mod hub_register;
mod import;
mod index_cmd;
mod init;
mod materialize;
mod merge_driver;
mod promote;
mod purge;
mod remember;
mod resolve_conflicts;
mod search;
mod sync_cmd;
mod wkpd;

#[cfg(test)]
mod test_support;

fn main() {
    // Dispatching on a CLI flag, not a security-sensitive use of argv.
    let mut args = std::env::args().skip(1); // nosemgrep: rust.lang.security.args.args
    let command = args.next();

    match command.as_deref() {
        Some("--version" | "-V") => {
            println!("wkp {}", env!("CARGO_PKG_VERSION"));
        }
        Some("init") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            let path = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().expect("wkp: cannot read cwd"));
            match init::run_init(&path) {
                Ok(()) => println!("wkp: initialized store at {}", path.display()),
                Err(msg) => {
                    eprintln!("wkp: init failed: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("import") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            let path = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().expect("wkp: cannot read cwd"));
            let claude_home = import::claude_home_from_env();
            match import::run_import(&path, claude_home.as_deref()) {
                Ok(summary) => println!("{summary}"),
                Err(msg) => {
                    eprintln!("wkp: import failed: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("search") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match search::parse_search_args(args) {
                Ok(opts) => match search::run_search(&opts) {
                    Ok(output) => println!("{output}"),
                    Err(msg) => {
                        eprintln!("wkp: search failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("context") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match search::parse_search_args(args) {
                Ok(opts) => match context::run_context(&opts) {
                    Ok(output) => println!("{output}"),
                    Err(msg) => {
                        eprintln!("wkp: context failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("traverse") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match context::parse_traverse_args(args) {
                Ok(opts) => match context::run_traverse(&opts) {
                    Ok(output) => println!("{output}"),
                    Err(msg) => {
                        eprintln!("wkp: traverse failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("index") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match index_cmd::parse_index_args(args) {
                Ok(opts) => match index_cmd::run_index_cli(&opts) {
                    Ok(summary) => println!("{summary}"),
                    Err(msg) => {
                        eprintln!("wkp: index failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("materialize") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match materialize::parse_materialize_args(args) {
                Ok(opts) => match materialize::run_materialize(&opts) {
                    Ok(()) => {}
                    Err(msg) => {
                        eprintln!("wkp: materialize failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("remember") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match remember::parse_remember_args(args) {
                Ok(opts) => match remember::run_remember(&opts) {
                    Ok(summary) => println!("{summary}"),
                    Err(msg) => {
                        eprintln!("wkp: remember failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("promote") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match promote::parse_promote_args(args) {
                Ok(opts) => match promote::run_promote(&opts) {
                    Ok(summary) => println!("{summary}"),
                    Err(msg) => {
                        eprintln!("wkp: promote failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("hub") => {
            match args.next().as_deref() {
                Some("register") => match hub_register::parse_hub_register_args(args) {
                    Ok(opts) => match hub_register::run_hub_register(&opts) {
                        Ok(summary) => println!("{summary}"),
                        Err(msg) => {
                            eprintln!("wkp: hub register failed: {msg}");
                            std::process::exit(1);
                        }
                    },
                    Err(msg) => {
                        eprintln!("wkp: {msg}");
                        std::process::exit(1);
                    }
                },
                _ => {
                    eprintln!("wkp: usage: wkp hub register --hub-url <url> --tenant <slug> [--path <dir>]");
                    std::process::exit(1);
                }
            }
        }
        Some("forget") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match forget::parse_forget_args(args) {
                Ok(opts) => {
                    let result = match &opts.target {
                        forget::ForgetTarget::Item(item_path) => {
                            let item_path = item_path.clone();
                            forget::run_forget_item(&opts, &item_path).map(|s| s.to_string())
                        }
                        forget::ForgetTarget::Device(device_id) => {
                            let device_id = device_id.clone();
                            forget::run_forget_device(&opts, &device_id).map(|s| s.to_string())
                        }
                    };
                    match result {
                        Ok(summary) => println!("{summary}"),
                        Err(msg) => {
                            eprintln!("wkp: forget failed: {msg}");
                            std::process::exit(1);
                        }
                    }
                }
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("purge") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match purge::parse_purge_args(args) {
                Ok(opts) => match purge::run_purge(&opts) {
                    Ok(summary) => println!("{summary}"),
                    Err(msg) => {
                        eprintln!("wkp: purge failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        // Deliberately no `ensure_min_git_version` check: `wkp hooks`
        // never touches git or the store, only prints static text (design
        // 3.3: "the binary never writes outside its own store" -- this
        // command doesn't write anywhere at all).
        Some("hooks") => match hooks::parse_hooks_args(args) {
            Ok(framework) => match hooks::render_hooks(&framework) {
                Ok(text) => println!("{text}"),
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            },
            Err(msg) => {
                eprintln!("wkp: {msg}");
                std::process::exit(1);
            }
        },
        // Deliberately no `ensure_min_git_version` check: git itself
        // invokes this (per its own merge-driver protocol), not a human
        // typing `wkp`, and it only reads/writes the three plain temp
        // files git hands it -- no repo access, no `wkp-git` call at all.
        Some("merge-driver") => {
            let mut positional = args;
            let (Some(ancestor), Some(ours), Some(theirs)) =
                (positional.next(), positional.next(), positional.next())
            else {
                eprintln!(
                    "wkp: merge-driver requires three paths: <ancestor> <ours> <theirs> \
                     (git supplies these itself per its merge-driver protocol)"
                );
                std::process::exit(1);
            };
            if let Err(msg) = merge_driver::run_merge_driver(
                Path::new(&ancestor),
                Path::new(&ours),
                Path::new(&theirs),
            ) {
                eprintln!("wkp: merge-driver failed: {msg}");
                std::process::exit(1);
            }
        }
        // Deliberately no `ensure_min_git_version` check, same reasoning
        // as merge-driver above: git itself invokes this per its own
        // clean/smudge filter protocol (gitattributes(5)), with cwd
        // already set to the top of the working tree -- confirmed by
        // hand, not assumed, since that's exactly what
        // `filter::run_filter_clean`/`run_filter_smudge` rely on to find
        // the store's `recipients` file and `.wkp/device-identity`
        // fallback without a separate `--store` flag.
        Some("filter") => {
            use std::io::{Read, Write};
            let direction = args.next();
            let _file_path = args.next(); // %f -- accepted per git's protocol, not needed by content-based detection
            let store_root = std::env::current_dir().expect("wkp: cannot read cwd");

            let mut content = Vec::new();
            std::io::stdin()
                .read_to_end(&mut content)
                .expect("wkp: failed to read filter input from stdin");

            match direction.as_deref() {
                Some("clean") => match filter::run_filter_clean(&store_root, &content) {
                    Ok(output) => {
                        std::io::stdout()
                            .write_all(&output)
                            .expect("wkp: failed to write filter output");
                    }
                    Err(msg) => {
                        eprintln!("wkp: filter clean failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Some("smudge") => {
                    let output = filter::run_filter_smudge(&store_root, &content);
                    std::io::stdout()
                        .write_all(&output)
                        .expect("wkp: failed to write filter output");
                }
                _ => {
                    eprintln!("wkp: filter requires a direction: clean|smudge");
                    std::process::exit(1);
                }
            }
        }
        Some("resolve-conflicts") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match resolve_conflicts::parse_resolve_conflicts_args(args) {
                Ok(path) => match resolve_conflicts::resolve_modify_delete_conflicts(&path) {
                    Ok(inbox_paths) if inbox_paths.is_empty() => {
                        println!("wkp: no modify/delete conflicts found");
                    }
                    Ok(inbox_paths) => {
                        for inbox_path in inbox_paths {
                            println!("wkp: kept modification, proposed deletion at {inbox_path}");
                        }
                    }
                    Err(msg) => {
                        eprintln!("wkp: resolve-conflicts failed: {msg}");
                        std::process::exit(1);
                    }
                },
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Some("sync") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            // `wkp sync status` is a sub-subcommand; anything else (or
            // nothing at all) falls through to the ordinary `wkp sync
            // [--remote <name>] [--path <dir>]` flow, so the peeked token
            // has to be put back for `parse_sync_args` when it isn't
            // "status".
            let mut args = args;
            let first = args.next();
            if first.as_deref() == Some("status") {
                match sync_cmd::parse_sync_status_args(args) {
                    Ok(path) => match sync_cmd::run_sync_status(&path) {
                        Ok(conflicts) => {
                            println!("{}", sync_cmd::SyncStatusSummary { conflicts })
                        }
                        Err(msg) => {
                            eprintln!("wkp: sync status failed: {msg}");
                            std::process::exit(1);
                        }
                    },
                    Err(msg) => {
                        eprintln!("wkp: {msg}");
                        std::process::exit(1);
                    }
                }
            } else {
                let rebuilt = first.into_iter().chain(args);
                match sync_cmd::parse_sync_args(rebuilt) {
                    Ok(opts) => match sync_cmd::run_sync(&opts) {
                        Ok(summary) => println!("{summary}"),
                        Err(msg) => {
                            eprintln!("wkp: sync failed: {msg}");
                            std::process::exit(1);
                        }
                    },
                    Err(msg) => {
                        eprintln!("wkp: {msg}");
                        std::process::exit(1);
                    }
                }
            }
        }
        Some("bundle") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match args.next().as_deref() {
                Some("export") => match bundle::parse_bundle_export_args(args) {
                    Ok(opts) => match bundle::run_bundle_export(&opts) {
                        Ok(summary) => println!("{summary}"),
                        Err(msg) => {
                            eprintln!("wkp: bundle export failed: {msg}");
                            std::process::exit(1);
                        }
                    },
                    Err(msg) => {
                        eprintln!("wkp: {msg}");
                        std::process::exit(1);
                    }
                },
                Some("import") => match bundle::parse_bundle_import_args(args) {
                    Ok(opts) => match bundle::run_bundle_import(&opts) {
                        Ok(summary) => println!("{summary}"),
                        Err(msg) => {
                            eprintln!("wkp: bundle import failed: {msg}");
                            std::process::exit(1);
                        }
                    },
                    Err(msg) => {
                        eprintln!("wkp: {msg}");
                        std::process::exit(1);
                    }
                },
                other => {
                    eprintln!(
                        "wkp: bundle requires a subcommand: export or import (got {other:?})"
                    );
                    std::process::exit(1);
                }
            }
        }
        Some("wkpd") => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            match wkpd::parse_wkpd_args(args) {
                Ok(opts) => {
                    if let Err(msg) = wkpd::run_wkpd(&opts) {
                        eprintln!("wkp: wkpd failed: {msg}");
                        std::process::exit(1);
                    }
                }
                Err(msg) => {
                    eprintln!("wkp: {msg}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            if let Err(msg) = wkp_git::ensure_min_git_version() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            eprintln!("wkp: no subcommands implemented yet (see docs/plan/milestones.md)");
            std::process::exit(1);
        }
    }
}

/// Writes `content` to `dest` via a temp file in the same directory,
/// renamed into place -- never in place (CLAUDE.md hard rule). Shared by
/// `wkp remember`/`wkp promote`/`wkp materialize`, the three write paths
/// that produce a file a harness or a human reads back.
fn atomic_write(dest: &Path, content: &str) -> Result<(), String> {
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "materialized.md".to_string());
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dest.with_file_name(format!(".{file_name}.tmp-{pid}-{nanos}"));
    let result = std::fs::write(&tmp, content).map_err(|e| e.to_string());
    match result {
        Ok(()) => std::fs::rename(&tmp, dest).map_err(|e| e.to_string()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}
