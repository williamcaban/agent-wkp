//! The only crate in this workspace permitted `unsafe` code, per design 3.3
//! and the CLAUDE.md hard rule (`#![forbid(unsafe_code)]` everywhere else).
//! No `unsafe` yet: bundled SQLite (rusqlite `bundled` feature, design 5.3)
//! is wired up in M1; see `docs/plan/milestones.md`.
