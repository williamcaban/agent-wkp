//! Shared test-only fixtures used across this module's submodule test
//! suites.

use crate::frontmatter::Frontmatter;

use super::schema::Item;

/// `human_signed: true` by default: every existing test using this
/// predates M2-6's signing gate and is testing something else entirely
/// (search ranking, traversal, materialize output shape, ...) --
/// defaulting to "properly reviewed" here means none of them needed
/// individual updates for M2-6, only the tests that specifically
/// exercise the gate itself override it.
pub(crate) fn item(path: &str, title: &str, body: &str) -> Item {
    let fm = Frontmatter {
        title: Some(title.to_string()),
        ..Default::default()
    };
    Item {
        path: path.to_string(),
        frontmatter: fm,
        body: body.to_string(),
        embedding: None,
        human_signed: true,
    }
}

/// A securely created, uniquely named temp directory for a test's own
/// `index.db` (`tempfile` rather than `std::env::temp_dir()` +
/// a predictable name: the latter is flagged by this repo's semgrep
/// gate as an insecure-temp-file pattern, since a shared temp
/// directory with a guessable name invites symlink/TOCTOU races).
pub(crate) fn temp_db_dir(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("wkp-core-test-{name}-"))
        .tempdir()
        .expect("create temp dir")
}
