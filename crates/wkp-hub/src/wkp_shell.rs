//! `wkp-shell`: key-to-tenant mapping for `sshd`'s `AuthorizedKeysCommand`
//! (design 8.1, M5-3), plus the `command=` wrapper it emits.
//!
//! **Human co-sign required**: design 9.1 names "the hub's key-to-tenant
//! mapping" explicitly as one of the changes that always needs one.
//!
//! Two halves, both driven by the control plane (M5-1), neither of
//! which trusts anything the connecting SSH client says about which
//! repo it wants:
//!
//! - [`resolve_authorized_key`] (`wkp-hub authorized-keys-command
//!   <public-key>`): given a presented public key, looks it up, and --
//!   only for an active, registered device -- emits one
//!   `authorized_keys` line naming that device's *own* tenant, nothing
//!   else. An unknown or revoked key gets no line at all (empty
//!   stdout, exit 0 -- `sshd`'s own `AuthorizedKeysCommand` contract
//!   for "no keys found"), never a default or fallback tenant.
//! - [`decide_git_shell_command`] (invoked as the emitted line's own
//!   `command=`, `wkp-hub git-shell <tenant-slug>`): reads
//!   `$SSH_ORIGINAL_COMMAND` (whatever the connecting client actually
//!   asked to run) and decides whether to run `git-receive-pack` or
//!   `git-upload-pack` against that tenant's *own fixed* repo path --
//!   deliberately never the path the client's own command string
//!   names, so there is no path-traversal or cross-tenant-repo
//!   surface to validate against at all, and anything that isn't
//!   exactly one of those two commands is refused outright, even
//!   though the key that got this far already authenticated
//!   successfully.
//!
//! Per this task's own scope (`docs/plan/milestones.md`'s M5-3 issue
//! text): real `sshd` `AuthorizedKeysCommand` wiring and the container
//! that runs it are M5-5's job; per-tenant bare repo *provisioning* is
//! M5-4's. This module only defines the *contract* both later tasks
//! build against -- the fixed repo-path convention in
//! [`tenant_repo_path`], and the fact that `git-shell` never listens
//! to the client for where that repo lives.

use crate::control_plane;
use postgres::Client;
use std::path::{Path, PathBuf};

/// One `authorized_keys` line to emit for a resolved, active device.
pub struct AuthorizedKeyLine {
    pub tenant_slug: String,
    pub line: String,
}

/// Given the exact public-key string a connecting device presented
/// (the same `<type> <base64>` format [`control_plane::Device`]
/// stores, per M5-2's own registration), resolves it to its device row
/// and tenant. `Ok(None)` for an unknown key *or* a revoked one
/// (`revoked_at` set) -- both mean "no line", not an error; refusing a
/// connection is `sshd`'s own job once this program's stdout comes back
/// empty, not something this function signals via `Err`.
pub fn resolve_authorized_key(
    client: &mut Client,
    public_key: &str,
) -> Result<Option<AuthorizedKeyLine>, control_plane::Error> {
    let Some(device) = control_plane::find_device_by_public_key(client, public_key)? else {
        return Ok(None);
    };
    if device.revoked_at.is_some() {
        return Ok(None);
    }
    let Some(tenant) = control_plane::find_tenant_by_id(client, device.tenant_id)? else {
        // A device whose tenant row is gone is exactly as unauthorized
        // as one that was never registered -- refuse the same way
        // (`Ok(None)`), not a control-plane error.
        return Ok(None);
    };

    // `restrict` (OpenSSH 7.2+): disables port/agent/X11 forwarding, a
    // pty, and ~/.ssh/rc all at once -- defense in depth alongside
    // sshd_config's own equivalent settings (design 8.1's hardening
    // list, wired by M5-5), not a substitute for them.
    let line = format!(
        "command=\"wkp-hub git-shell {}\",restrict {public_key}",
        tenant.slug
    );
    Ok(Some(AuthorizedKeyLine {
        tenant_slug: tenant.slug,
        line,
    }))
}

/// The one, fixed bare-repo path a tenant's `git-shell` invocation
/// ever operates on -- the contract M5-4's actual provisioning must
/// honor. Never derived from anything a connecting client says.
pub fn tenant_repo_path(repos_root: &Path, tenant_slug: &str) -> PathBuf {
    repos_root.join(format!("{tenant_slug}.git"))
}

/// What a `wkp-hub git-shell <tenant-slug>` invocation decides to do,
/// given `$SSH_ORIGINAL_COMMAND`.
#[derive(Debug, PartialEq, Eq)]
pub enum GitShellDecision {
    Exec {
        program: &'static str,
        repo_path: PathBuf,
    },
    Refuse {
        reason: String,
    },
}

/// `git-receive-pack`/`git-upload-pack` (design 8.1's own two named
/// commands) or refuse -- always against `tenant_slug`'s *own* fixed
/// path ([`tenant_repo_path`]), regardless of what path
/// `ssh_original_command` names. A real `git` client sends
/// `git-upload-pack '/some/path'` (single-quoted), but that path is
/// never read here at all: this key already only ever maps to one
/// tenant ([`resolve_authorized_key`]), so there is nothing to
/// disambiguate and no reason to trust a client-supplied path string
/// for it.
pub fn decide_git_shell_command(
    tenant_slug: &str,
    repos_root: &Path,
    ssh_original_command: Option<&str>,
) -> GitShellDecision {
    let Some(cmd) = ssh_original_command else {
        return GitShellDecision::Refuse {
            reason: "interactive shells are not allowed -- only git-receive-pack and \
                     git-upload-pack, run over `ssh`, e.g. `git push`/`git fetch`"
                .to_string(),
        };
    };

    let program = if cmd == "git-receive-pack" || cmd.starts_with("git-receive-pack ") {
        "git-receive-pack"
    } else if cmd == "git-upload-pack" || cmd.starts_with("git-upload-pack ") {
        "git-upload-pack"
    } else {
        return GitShellDecision::Refuse {
            reason: format!(
                "command not allowed: {cmd} -- only git-receive-pack and git-upload-pack"
            ),
        };
    };

    GitShellDecision::Exec {
        program,
        repo_path: tenant_repo_path(repos_root, tenant_slug),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::{connect, create_tenant, register_device, revoke_device};

    fn unique_slug(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}-{nanos}")
    }

    #[test]
    fn resolve_authorized_key_emits_a_restricted_command_line_for_an_active_device() {
        let mut client = connect().expect("connect");
        let tenant = create_tenant(&mut client, &unique_slug("wkp-shell-active")).expect("tenant");
        let public_key = unique_slug("ssh-ed25519 AAAA...active");
        register_device(&mut client, tenant.id, &public_key).expect("register_device");

        let resolved = resolve_authorized_key(&mut client, &public_key)
            .expect("resolve_authorized_key")
            .expect("an active, registered device must resolve to a line");
        assert_eq!(resolved.tenant_slug, tenant.slug);
        assert!(
            resolved
                .line
                .contains(&format!("command=\"wkp-hub git-shell {}\"", tenant.slug)),
            "line must name this device's own tenant: {}",
            resolved.line
        );
        assert!(resolved.line.contains("restrict"));
        assert!(
            resolved.line.ends_with(&public_key),
            "line must end with the exact presented public key: {}",
            resolved.line
        );
    }

    #[test]
    fn resolve_authorized_key_refuses_an_unknown_key() {
        let mut client = connect().expect("connect");
        let resolved =
            resolve_authorized_key(&mut client, "ssh-ed25519 this-key-was-never-registered")
                .expect("resolve_authorized_key");
        assert!(resolved.is_none(), "an unknown key must emit no line");
    }

    /// M5-3's own core acceptance criterion.
    #[test]
    fn resolve_authorized_key_refuses_a_revoked_key() {
        let mut client = connect().expect("connect");
        let tenant = create_tenant(&mut client, &unique_slug("wkp-shell-revoked")).expect("tenant");
        let public_key = unique_slug("ssh-ed25519 AAAA...revoked");
        let device = register_device(&mut client, tenant.id, &public_key).expect("register_device");

        // Still active: resolves.
        assert!(resolve_authorized_key(&mut client, &public_key)
            .expect("resolve_authorized_key")
            .is_some());

        revoke_device(&mut client, device.id).expect("revoke_device");

        let resolved =
            resolve_authorized_key(&mut client, &public_key).expect("resolve_authorized_key");
        assert!(resolved.is_none(), "a revoked device must emit no line");
    }

    #[test]
    fn decide_git_shell_command_allows_receive_and_upload_pack_against_the_tenants_own_path() {
        let repos_root = Path::new("/srv/wkp-hub/repos");

        let receive = decide_git_shell_command(
            "acme",
            repos_root,
            Some("git-receive-pack '/some/attacker-chosen/path.git'"),
        );
        assert_eq!(
            receive,
            GitShellDecision::Exec {
                program: "git-receive-pack",
                repo_path: PathBuf::from("/srv/wkp-hub/repos/acme.git"),
            },
            "the client-supplied path must be ignored entirely -- always this tenant's own repo"
        );

        let upload = decide_git_shell_command("acme", repos_root, Some("git-upload-pack '/x'"));
        assert_eq!(
            upload,
            GitShellDecision::Exec {
                program: "git-upload-pack",
                repo_path: PathBuf::from("/srv/wkp-hub/repos/acme.git"),
            }
        );
    }

    #[test]
    fn decide_git_shell_command_refuses_an_interactive_shell() {
        let decision = decide_git_shell_command("acme", Path::new("/srv/wkp-hub/repos"), None);
        assert!(matches!(decision, GitShellDecision::Refuse { .. }));
    }

    /// M5-3's own explicit acceptance criterion: verified by test that
    /// the restriction actually holds, not just by reading the string.
    #[test]
    fn decide_git_shell_command_refuses_anything_other_than_the_two_allowed_commands() {
        let repos_root = Path::new("/srv/wkp-hub/repos");
        for disallowed in [
            "/bin/bash",
            "rm -rf /",
            "git-receive-pack-evil",
            "sh -c 'git-upload-pack /x'",
            "",
        ] {
            let decision = decide_git_shell_command("acme", repos_root, Some(disallowed));
            assert!(
                matches!(decision, GitShellDecision::Refuse { .. }),
                "expected {disallowed:?} to be refused, got {decision:?}"
            );
        }
    }
}
