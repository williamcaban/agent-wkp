#![no_main]

use libfuzzer_sys::fuzz_target;

// M1-1 acceptance criteria: the frontmatter parser must never panic on
// arbitrary byte input. `parse` only accepts `&str`, matching how it is
// actually called (on a UTF-8 markdown file read from the store), so the
// fuzz target does the same lossy conversion a real caller reading an
// unexpectedly non-UTF-8 file would do.
fuzz_target!(|data: &[u8]| {
    let input = String::from_utf8_lossy(data);
    let _ = wkp_core::frontmatter::parse(&input);
});
