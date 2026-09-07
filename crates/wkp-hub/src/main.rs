#![forbid(unsafe_code)]

//! `wkp-hub`: sshd + wkp-shell + git http-backend front end, per-tenant
//! bare repo and SQLite index, control plane (design 8). A separate binary
//! target so the laptop `wkp` binary never links the Postgres client or
//! control-plane code (design 3.3).
//! CODEOWNERS-gated: changes here need a human co-sign (design 9.1).
//! Implementation lands starting in M5; see `docs/plan/milestones.md`.

fn main() {
    eprintln!("wkp-hub: not implemented yet (see docs/plan/milestones.md, M5)");
    std::process::exit(1);
}
