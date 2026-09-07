#![forbid(unsafe_code)]

//! Git plumbing wrapper, bundles, and sync.
//! CODEOWNERS-gated: changes here need a human co-sign (design 9.1).
//! No `Command::new("git")` is allowed outside this crate (CLAUDE.md hard rule).
//! Implementation lands starting in M2; see `docs/plan/milestones.md`.
