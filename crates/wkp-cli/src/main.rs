#![forbid(unsafe_code)]

//! The `wkp` binary. Subcommands (`init`, `index`, `search`, `context`,
//! `materialize`, `hooks`, `remember`, `wkpd`, ...) land starting in M1;
//! see `docs/plan/milestones.md`. For now only `--version` is wired up.

fn main() {
    // Dispatching on a CLI flag, not a security-sensitive use of argv.
    let arg = std::env::args().nth(1); // nosemgrep: rust.lang.security.args.args
    match arg.as_deref() {
        Some("--version" | "-V") => {
            println!("wkp {}", env!("CARGO_PKG_VERSION"));
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
