#![forbid(unsafe_code)]

//! `wkp-hub`: sshd + wkp-shell + git http-backend front end, per-tenant
//! bare repo and SQLite index, control plane (design 8). A separate binary
//! target so the laptop `wkp` binary never links the Postgres client or
//! control-plane code (design 3.3).
//! CODEOWNERS-gated: changes here need a human co-sign (design 9.1).
//! Implementation lands starting in M5; see `docs/plan/milestones.md`.

mod control_plane;
mod http;
mod tenant_pod;
mod tenant_repo;
mod wkp_shell;

/// Where per-tenant bare repos live (M5-4's own job to actually
/// provision) -- read once, here, so `wkp-hub git-shell` doesn't need
/// its own separate configuration story beyond this one env var.
/// Never argv (CLAUDE.md's secrets rule doesn't strictly apply to a
/// plain filesystem path, but there is no reason for it to be
/// per-invocation configurable via the `authorized_keys` `command=`
/// string either -- one hub process, one repos root).
const REPOS_ROOT_ENV: &str = "WKP_HUB_REPOS_ROOT";
const DEFAULT_REPOS_ROOT: &str = "/srv/wkp-hub/repos";

fn repos_root() -> std::path::PathBuf {
    std::env::var(REPOS_ROOT_ENV)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(DEFAULT_REPOS_ROOT))
}

/// The image a tenant's own pod runs (M5-7, ADR-0010) -- the same
/// `wkp-hub` image M5-5's `deploy/hub/Containerfile` already builds,
/// invoked in `serve-tenant` mode; not a second image to build.
const TENANT_IMAGE_ENV: &str = "WKP_HUB_TENANT_IMAGE";
const DEFAULT_TENANT_IMAGE: &str = "localhost/wkp-hub";

fn tenant_image() -> String {
    std::env::var(TENANT_IMAGE_ENV).unwrap_or_else(|_| DEFAULT_TENANT_IMAGE.to_string())
}

/// A minimal admin CLI over M5-1's control plane -- `migrate` (ensure
/// the schema exists), `tenant create`, `device register`/`revoke`.
/// Genuinely useful operator tooling on its own (provisioning a tenant
/// or revoking a device by hand needs exactly this, independent of
/// whether `wkp-shell`/RFC 8628/the container exist yet), not a stub
/// written only to give this task's control-plane functions a real
/// caller. `sshd`'s `AuthorizedKeysCommand` (M5-3), the RFC 8628 HTTP
/// endpoints (M5-2) and the container entrypoint (M5-5) are later,
/// separate ways into the same control plane, not replacements for
/// this one.
fn main() {
    // Dispatching on a CLI flag, not a security-sensitive use of argv.
    let mut args = std::env::args().skip(1); // nosemgrep: rust.lang.security.args.args
    let command = args.next();

    match command.as_deref() {
        Some("serve") => {
            let port = args
                .next()
                .as_deref()
                .filter(|a| *a == "--port")
                .and_then(|_| args.next())
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(8080);
            if let Err(e) = http::serve(port, repos_root(), tenant_image()) {
                eprintln!("wkp-hub: serve failed: {e}");
                std::process::exit(1);
            }
        }
        Some("serve-tenant") => {
            // M5-7 (ADR-0010): the mode a tenant's own pod runs -- one
            // fixed repo, no bearer-token check (the front door already
            // made that decision before ever proxying here), no
            // RFC 8628/`/verify` routes at all.
            let mut tenant_slug = None;
            let mut port = 8080u16;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--tenant" => tenant_slug = args.next(),
                    "--port" => {
                        port = args
                            .next()
                            .and_then(|p| p.parse::<u16>().ok())
                            .unwrap_or(port)
                    }
                    _ => {}
                }
            }
            let Some(tenant_slug) = tenant_slug else {
                eprintln!("wkp-hub: usage: wkp-hub serve-tenant --tenant <slug> [--port <port>]");
                std::process::exit(1);
            };
            if let Err(e) = http::serve_single_tenant(port, tenant_slug, repos_root()) {
                eprintln!("wkp-hub: serve-tenant failed: {e}");
                std::process::exit(1);
            }
        }
        Some("authorized-keys-command") => {
            // sshd's AuthorizedKeysCommand (M5-5's own wiring) passes
            // the key as one or more tokens depending on how the
            // directive names them (`%t %k`, or a single pre-quoted
            // `"%t %k"`) -- join whatever's left so both shapes
            // reconstruct the same `<type> <base64>` string
            // control_plane::Device::public_key stores.
            let public_key = args.collect::<Vec<_>>().join(" ");
            if public_key.trim().is_empty() {
                eprintln!("wkp-hub: usage: wkp-hub authorized-keys-command <public-key>");
                std::process::exit(1);
            }
            let result = control_plane::connect()
                .and_then(|mut client| wkp_shell::resolve_authorized_key(&mut client, &public_key));
            match result {
                // Deliberately no output at all for an unknown/revoked
                // key, exit 0 -- sshd's own AuthorizedKeysCommand
                // contract for "no keys found", not an error.
                Ok(Some(resolved)) => {
                    // stderr, not stdout: sshd parses AuthorizedKeysCommand's
                    // stdout as literal authorized_keys lines, nothing else
                    // may appear there. This is just an operator-facing
                    // diagnostic (which tenant a connection resolved to,
                    // useful in sshd's own logs), not part of the protocol.
                    eprintln!("wkp-hub: resolved to tenant {}", resolved.tenant_slug);
                    println!("{}", resolved.line);
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("wkp-hub: authorized-keys-command failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("git-shell") => {
            let Some(tenant_slug) = args.next() else {
                eprintln!("wkp-hub: usage: wkp-hub git-shell <tenant-slug>");
                std::process::exit(1);
            };
            let ssh_original_command = std::env::var("SSH_ORIGINAL_COMMAND").ok();
            match wkp_shell::decide_git_shell_command(
                &tenant_slug,
                &repos_root(),
                ssh_original_command.as_deref(),
            ) {
                wkp_shell::GitShellDecision::Exec { program, repo_path } => {
                    let status = std::process::Command::new(program).arg(&repo_path).status();
                    match status {
                        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
                        Err(e) => {
                            eprintln!("wkp-hub: git-shell: failed to run {program}: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                wkp_shell::GitShellDecision::Refuse { reason } => {
                    eprintln!("wkp-hub: git-shell: {reason}");
                    std::process::exit(1);
                }
            }
        }
        Some("start-pod") => {
            let Some(tenant_slug) = args.next() else {
                eprintln!("wkp-hub: usage: wkp-hub start-pod <tenant-slug>");
                std::process::exit(1);
            };
            let result = tenant_pod::start_pod(&tenant_image(), &repos_root(), &tenant_slug)
                .map_err(|e| e.to_string())
                .and_then(|()| {
                    let mut client = control_plane::connect()
                        .map_err(|e: control_plane::Error| e.to_string())?;
                    let tenant = control_plane::find_tenant_by_slug(&mut client, &tenant_slug)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("tenant {tenant_slug} not found"))?;
                    control_plane::record_pod_started(&mut client, tenant.id)
                        .map_err(|e| e.to_string())
                });
            match result {
                Ok(()) => println!("wkp-hub: pod started for tenant {tenant_slug}"),
                Err(e) => {
                    eprintln!("wkp-hub: start-pod failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("stop-pod") => {
            let Some(tenant_slug) = args.next() else {
                eprintln!("wkp-hub: usage: wkp-hub stop-pod <tenant-slug>");
                std::process::exit(1);
            };
            let result = tenant_pod::stop_pod(&tenant_slug)
                .map_err(|e| e.to_string())
                .and_then(|()| {
                    let mut client = control_plane::connect()
                        .map_err(|e: control_plane::Error| e.to_string())?;
                    let tenant = control_plane::find_tenant_by_slug(&mut client, &tenant_slug)
                        .map_err(|e| e.to_string())?
                        .ok_or_else(|| format!("tenant {tenant_slug} not found"))?;
                    control_plane::record_pod_stopped(&mut client, tenant.id)
                        .map_err(|e| e.to_string())
                });
            match result {
                Ok(()) => println!("wkp-hub: pod stopped for tenant {tenant_slug}"),
                Err(e) => {
                    eprintln!("wkp-hub: stop-pod failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("reap-idle-pods") => {
            let idle_minutes = args
                .next()
                .as_deref()
                .filter(|a| *a == "--idle-minutes")
                .and_then(|_| args.next())
                .and_then(|m| m.parse::<i64>().ok())
                .unwrap_or(30);
            let result = control_plane::connect()
                .map_err(|e| e.to_string())
                .and_then(|mut client| {
                    tenant_pod::reap_idle(&mut client, time::Duration::minutes(idle_minutes))
                });
            match result {
                Ok(reaped) => {
                    if reaped.is_empty() {
                        println!("wkp-hub: no pods idle for {idle_minutes}+ minutes");
                    }
                    for slug in reaped {
                        println!("wkp-hub: reaped pod for tenant {slug}");
                    }
                }
                Err(e) => {
                    eprintln!("wkp-hub: reap-idle-pods failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("index-tenant") => {
            let Some(tenant_slug) = args.next() else {
                eprintln!("wkp-hub: usage: wkp-hub index-tenant <tenant-slug>");
                std::process::exit(1);
            };
            match tenant_repo::index_tenant(&repos_root(), &tenant_slug) {
                Ok(summary) => {
                    println!(
                        "wkp-hub: indexed {} item(s) for tenant {tenant_slug}, skipped {} \
                         private item(s)",
                        summary.indexed.len(),
                        summary.skipped_private.len()
                    );
                }
                Err(e) => {
                    eprintln!("wkp-hub: index-tenant failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("provision-repo") => {
            // The git/filesystem half of `tenant create`, exposed on its
            // own: genuinely useful on its own (re-provisioning a repo
            // whose control-plane row already exists, or fixing a
            // missing/corrupted hook, without touching Postgres at all)
            // and, not incidentally, lets this crate's own tests and
            // this task's acceptance-criteria integration test exercise
            // the repo+hook half without needing a database -- the same
            // DB-free property `index-tenant` above already has.
            let Some(tenant_slug) = args.next() else {
                eprintln!("wkp-hub: usage: wkp-hub provision-repo <tenant-slug>");
                std::process::exit(1);
            };
            match tenant_repo::provision_tenant_repo(&repos_root(), &tenant_slug) {
                Ok(repo_path) => println!(
                    "wkp-hub: provisioned repo for tenant {tenant_slug} at {}",
                    repo_path.display()
                ),
                Err(e) => {
                    eprintln!("wkp-hub: provision-repo failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("migrate") => {
            match control_plane::connect() {
                Ok(_client) => println!("wkp-hub: schema is up to date"),
                Err(e) => {
                    eprintln!("wkp-hub: migrate failed: {e}");
                    std::process::exit(1);
                }
            };
        }
        Some("tenant") => match args.next().as_deref() {
            Some("create") => {
                let Some(slug) = args.next() else {
                    eprintln!("wkp-hub: tenant create requires a slug");
                    std::process::exit(1);
                };
                let tenant = match control_plane::connect()
                    .and_then(|mut client| control_plane::create_tenant(&mut client, &slug))
                {
                    Ok(tenant) => tenant,
                    Err(e) => {
                        eprintln!("wkp-hub: tenant create failed: {e}");
                        std::process::exit(1);
                    }
                };
                // M5-4: a tenant isn't actually usable (nothing to push
                // or pull) until its bare repo exists too -- one command
                // leaves both the control-plane row and the repo in
                // place, rather than requiring a second manual step.
                match tenant_repo::provision_tenant_repo(&repos_root(), &tenant.slug) {
                    Ok(repo_path) => println!(
                        "wkp-hub: created tenant {} (id {}), repo at {}",
                        tenant.slug,
                        tenant.id,
                        repo_path.display()
                    ),
                    Err(e) => {
                        eprintln!(
                            "wkp-hub: tenant create: control-plane row created, but \
                                   repo provisioning failed: {e}"
                        );
                        std::process::exit(1);
                    }
                }
            }
            // M5-7 (ADR-0010): admin/debug entry points into the pod-
            // lifecycle bookkeeping the control plane now tracks --
            // genuinely useful on their own (manually pinning a tenant
            // warm, correcting a stuck pod-state row, or previewing
            // what a reaper sweep would stop right now), independent
            // of whether the actual provisioning/reaper code that
            // would normally call these exists yet.
            Some("set-always-warm") => {
                let (slug, value) = (args.next(), args.next());
                let (Some(slug), Some(value)) = (slug, value) else {
                    eprintln!("wkp-hub: usage: wkp-hub tenant set-always-warm <slug> <true|false>");
                    std::process::exit(1);
                };
                let Ok(always_warm) = value.parse::<bool>() else {
                    eprintln!(
                        "wkp-hub: set-always-warm: value must be 'true' or 'false', got {value}"
                    );
                    std::process::exit(1);
                };
                let result = (|| {
                    let mut client = control_plane::connect()?;
                    let tenant = control_plane::find_tenant_by_slug(&mut client, &slug)?
                        .ok_or_else(|| control_plane::Error::NotFound(format!("tenant {slug}")))?;
                    control_plane::set_always_warm(&mut client, tenant.id, always_warm)
                })();
                match result {
                    Ok(()) => println!("wkp-hub: tenant {slug} always_warm = {always_warm}"),
                    Err(e) => {
                        eprintln!("wkp-hub: set-always-warm failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some("pod-started") => {
                let Some(slug) = args.next() else {
                    eprintln!("wkp-hub: usage: wkp-hub tenant pod-started <slug>");
                    std::process::exit(1);
                };
                let result = (|| {
                    let mut client = control_plane::connect()?;
                    let tenant = control_plane::find_tenant_by_slug(&mut client, &slug)?
                        .ok_or_else(|| control_plane::Error::NotFound(format!("tenant {slug}")))?;
                    control_plane::record_pod_started(&mut client, tenant.id)
                })();
                match result {
                    Ok(()) => println!("wkp-hub: tenant {slug} pod marked started"),
                    Err(e) => {
                        eprintln!("wkp-hub: pod-started failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some("pod-stopped") => {
                let Some(slug) = args.next() else {
                    eprintln!("wkp-hub: usage: wkp-hub tenant pod-stopped <slug>");
                    std::process::exit(1);
                };
                let result = (|| {
                    let mut client = control_plane::connect()?;
                    let tenant = control_plane::find_tenant_by_slug(&mut client, &slug)?
                        .ok_or_else(|| control_plane::Error::NotFound(format!("tenant {slug}")))?;
                    control_plane::record_pod_stopped(&mut client, tenant.id)
                })();
                match result {
                    Ok(()) => println!("wkp-hub: tenant {slug} pod marked stopped"),
                    Err(e) => {
                        eprintln!("wkp-hub: pod-stopped failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some("touch-active") => {
                let Some(slug) = args.next() else {
                    eprintln!("wkp-hub: usage: wkp-hub tenant touch-active <slug>");
                    std::process::exit(1);
                };
                let result = (|| {
                    let mut client = control_plane::connect()?;
                    let tenant = control_plane::find_tenant_by_slug(&mut client, &slug)?
                        .ok_or_else(|| control_plane::Error::NotFound(format!("tenant {slug}")))?;
                    control_plane::touch_last_active(&mut client, tenant.id)
                })();
                match result {
                    Ok(()) => println!("wkp-hub: tenant {slug} marked active"),
                    Err(e) => {
                        eprintln!("wkp-hub: touch-active failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some("list-idle") => {
                let idle_minutes = args
                    .next()
                    .as_deref()
                    .filter(|a| *a == "--idle-minutes")
                    .and_then(|_| args.next())
                    .and_then(|m| m.parse::<i64>().ok())
                    .unwrap_or(30);
                let result = control_plane::connect().and_then(|mut client| {
                    control_plane::find_idle_running_tenants(
                        &mut client,
                        time::Duration::minutes(idle_minutes),
                    )
                });
                match result {
                    Ok(tenants) => {
                        if tenants.is_empty() {
                            println!("wkp-hub: no tenants idle for {idle_minutes}+ minutes");
                        }
                        for tenant in tenants {
                            println!("wkp-hub: {} (id {}) idle", tenant.slug, tenant.id);
                        }
                    }
                    Err(e) => {
                        eprintln!("wkp-hub: list-idle failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            _ => {
                eprintln!(
                    "wkp-hub: usage: wkp-hub tenant create <slug> | \
                     set-always-warm <slug> <true|false> | \
                     pod-started <slug> | pod-stopped <slug> | \
                     touch-active <slug> | list-idle [--idle-minutes <n>]"
                );
                std::process::exit(1);
            }
        },
        Some("device") => match args.next().as_deref() {
            Some("register") => {
                let (tenant_slug, public_key) = (args.next(), args.next());
                let (Some(tenant_slug), Some(public_key)) = (tenant_slug, public_key) else {
                    eprintln!("wkp-hub: usage: wkp-hub device register <tenant-slug> <public-key>");
                    std::process::exit(1);
                };
                let result = (|| {
                    let mut client = control_plane::connect()?;
                    let tenant = control_plane::find_tenant_by_slug(&mut client, &tenant_slug)?
                        .ok_or_else(|| {
                            control_plane::Error::NotFound(format!("tenant {tenant_slug}"))
                        })?;
                    control_plane::register_device(&mut client, tenant.id, &public_key)
                })();
                match result {
                    Ok(device) => println!(
                        "wkp-hub: registered device {} for tenant {}",
                        device.id, tenant_slug
                    ),
                    Err(e) => {
                        eprintln!("wkp-hub: device register failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some("revoke") => {
                let Some(public_key) = args.next() else {
                    eprintln!("wkp-hub: usage: wkp-hub device revoke <public-key>");
                    std::process::exit(1);
                };
                let result = (|| {
                    let mut client = control_plane::connect()?;
                    let device =
                        control_plane::find_device_by_public_key(&mut client, &public_key)?
                            .ok_or_else(|| {
                                control_plane::Error::NotFound(format!("device {public_key}"))
                            })?;
                    control_plane::revoke_device(&mut client, device.id)?;
                    Ok::<_, control_plane::Error>(device)
                })();
                match result {
                    Ok(device) => println!("wkp-hub: revoked device {}", device.id),
                    Err(e) => {
                        eprintln!("wkp-hub: device revoke failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Some("issue-token") => {
                let Some(public_key) = args.next() else {
                    eprintln!("wkp-hub: usage: wkp-hub device issue-token <public-key>");
                    std::process::exit(1);
                };
                let result = (|| {
                    let mut client = control_plane::connect()?;
                    let device =
                        control_plane::find_device_by_public_key(&mut client, &public_key)?
                            .ok_or_else(|| {
                                control_plane::Error::NotFound(format!("device {public_key}"))
                            })?;
                    control_plane::issue_bearer_token(&mut client, device.id)
                })();
                match result {
                    // stdout carries only the token itself, no
                    // decoration: `wkp hub register`-style tooling can
                    // pipe/capture this directly rather than parsing a
                    // sentence out of it. The one time this plaintext
                    // is ever visible is right here -- the control
                    // plane never stores or returns it again.
                    Ok(token) => println!("{token}"),
                    Err(e) => {
                        eprintln!("wkp-hub: device issue-token failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            _ => {
                eprintln!("wkp-hub: usage: wkp-hub device register|revoke|issue-token ...");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!(
                "wkp-hub: usage: wkp-hub serve [--port <port>] | \
                 serve-tenant --tenant <slug> [--port <port>] | migrate | \
                 tenant create <slug> | device register <tenant-slug> <public-key> | \
                 device revoke <public-key> | device issue-token <public-key> | \
                 authorized-keys-command <public-key> | \
                 git-shell <tenant-slug> | index-tenant <tenant-slug> | \
                 provision-repo <tenant-slug> | start-pod <tenant-slug> | \
                 stop-pod <tenant-slug> | reap-idle-pods [--idle-minutes <n>]"
            );
            std::process::exit(1);
        }
    }
}
