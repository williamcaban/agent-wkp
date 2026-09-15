//! Minimum git version enforcement (design: SSH commit signing needs
//! git >= 2.34; `docs/adr/0001-git-minimum-version.md`).

use std::process::Command;

/// Minimum git version `wkp` requires. Decided in `docs/adr/0001-git-minimum-version.md`:
/// SSH commit signing (`gpg.format = ssh`) needs git >= 2.34, and that is the
/// only hard floor. The builtin fsmonitor daemon is opportunistic (wired up
/// in M1), not part of this gate.
pub const MIN_GIT_VERSION: (u32, u32, u32) = (2, 34, 0);

#[derive(Debug, PartialEq, Eq)]
pub enum GitVersionCheck {
    Ok { found: (u32, u32, u32) },
    TooOld { found: (u32, u32, u32) },
    Unparseable { raw: String },
    NotFound,
}

/// Runs `git --version` and checks it against [`MIN_GIT_VERSION`].
pub fn check_git_version() -> GitVersionCheck {
    let output = match Command::new("git").arg("--version").output() {
        Ok(o) => o,
        Err(_) => return GitVersionCheck::NotFound,
    };
    if !output.status.success() {
        return GitVersionCheck::NotFound;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    match parse_git_version(&raw) {
        Some(found) if found >= MIN_GIT_VERSION => GitVersionCheck::Ok { found },
        Some(found) => GitVersionCheck::TooOld { found },
        None => GitVersionCheck::Unparseable { raw },
    }
}

/// Checks the installed git against [`MIN_GIT_VERSION`], returning a
/// human-readable error naming the found and required versions if it does
/// not meet the floor, is missing, or can't be parsed.
pub fn ensure_min_git_version() -> Result<(), String> {
    let (req_major, req_minor, req_patch) = MIN_GIT_VERSION;
    match check_git_version() {
        GitVersionCheck::Ok { .. } => Ok(()),
        GitVersionCheck::TooOld {
            found: (major, minor, patch),
        } => Err(format!(
            "wkp: git {major}.{minor}.{patch} found, but wkp requires git >= \
             {req_major}.{req_minor}.{req_patch} (needed for SSH commit signing; see \
             docs/adr/0001-git-minimum-version.md). Upgrade git and try again."
        )),
        GitVersionCheck::Unparseable { raw } => Err(format!(
            "wkp: could not parse a version from `git --version` output {raw:?}; wkp requires \
             git >= {req_major}.{req_minor}.{req_patch}."
        )),
        GitVersionCheck::NotFound => Err(format!(
            "wkp: git not found on PATH; wkp requires git >= {req_major}.{req_minor}.{req_patch}."
        )),
    }
}

/// Parses `git version X.Y[.Z][ (platform suffix)]` into a `(major, minor, patch)`
/// triple. Missing patch defaults to 0; a non-numeric trailing platform
/// suffix (e.g. Apple Git's `2.39.3 (Apple Git-146)`) is dropped.
fn parse_git_version(raw: &str) -> Option<(u32, u32, u32)> {
    let version_str = raw.strip_prefix("git version ")?;
    let core = version_str.split_whitespace().next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        None => 0,
        Some(p) => {
            let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                0
            } else {
                digits.parse().ok()?
            }
        }
    };
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_version() {
        assert_eq!(parse_git_version("git version 2.55.0"), Some((2, 55, 0)));
    }

    #[test]
    fn parses_platform_suffixed_version() {
        assert_eq!(
            parse_git_version("git version 2.39.3 (Apple Git-146)"),
            Some((2, 39, 3))
        );
    }

    #[test]
    fn parses_two_component_version() {
        assert_eq!(parse_git_version("git version 2.34"), Some((2, 34, 0)));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_git_version("not git at all"), None);
    }

    #[test]
    fn min_version_ordering() {
        assert!(MIN_GIT_VERSION == (2, 34, 0));
        assert!((2, 55, 0) >= MIN_GIT_VERSION);
        assert!((2, 33, 9) < MIN_GIT_VERSION);
    }

    #[test]
    fn ensure_min_git_version_passes_on_this_dev_machine() {
        // This crate's own CI and dev environments run a git new enough to
        // sign commits over SSH (see rust-toolchain.toml neighbors: CI baseline
        // in M0 task 2 runs on runners with git 2.55). If this ever fails in
        // CI, the runner's git dropped below our floor, which is itself
        // worth knowing about.
        assert!(ensure_min_git_version().is_ok());
    }
}
