//! The three raw `Command::new("git")` wrappers every other module in
//! this crate is built on. Nothing outside `wkp-git` may call these
//! directly (CLAUDE.md's "no `Command::new(\"git\")` outside this crate"
//! rule is enforced by keeping even *these* wrappers un-exported past the
//! crate boundary).

use std::path::Path;
use std::process::Command;

pub(crate) fn run_git_stdout(repo_dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Like [`run_git_stdout`], but returns stdout's exact bytes rather than
/// a lossy-UTF8 `String` -- needed for any command whose output isn't
/// necessarily text, e.g. `cat-file -p` on a blob that holds arbitrary
/// binary content (an age-encrypted payload's ciphertext bytes, which a
/// lossy UTF-8 conversion would silently corrupt).
pub(crate) fn run_git_stdout_bytes(repo_dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Like [`run_git_stdout`], but feeds `stdin_data` to the child process's
/// stdin rather than assuming the command needs none -- `git
/// interpret-trailers` (design 5.4/7.4's provenance-trailer machinery,
/// M2-3) reads the message it formats or parses from stdin when given no
/// `<file>` argument, and this crate would rather use git's own trailer
/// logic than re-derive the subject/trailer-block formatting rules itself.
pub(crate) fn run_git_with_stdin(
    repo_dir: &Path,
    args: &[&str],
    stdin_data: &str,
) -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .expect("child spawned with Stdio::piped() stdin")
        .write_all(stdin_data.as_bytes())
        .map_err(|e| e.to_string())?;
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub(crate) fn run_git(repo_dir: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}
