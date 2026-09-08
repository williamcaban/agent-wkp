#![forbid(unsafe_code)]

//! Store model, frontmatter, tiers, index, FTS5 search, and merge driver.
//! Implementation lands starting in M1; see `docs/plan/milestones.md`.

pub mod embed;
pub mod frontmatter;
pub mod graph;
pub mod index;
pub mod secrets;
