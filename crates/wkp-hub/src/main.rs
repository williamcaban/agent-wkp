#![forbid(unsafe_code)]

//! `wkp-hub`: sshd + wkp-shell + git http-backend front end, per-tenant
//! bare repo and SQLite index, control plane (design 8). A separate binary
//! target so the laptop `wkp` binary never links the Postgres client or
//! control-plane code (design 3.3).
//! CODEOWNERS-gated: changes here need a human co-sign (design 9.1).
//! Implementation lands starting in M5; see `docs/plan/milestones.md`.

mod control_plane;

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
    let mut args = std::env::args().skip(1);
    let command = args.next();

    match command.as_deref() {
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
                match control_plane::connect()
                    .and_then(|mut client| control_plane::create_tenant(&mut client, &slug))
                {
                    Ok(tenant) => {
                        println!("wkp-hub: created tenant {} (id {})", tenant.slug, tenant.id)
                    }
                    Err(e) => {
                        eprintln!("wkp-hub: tenant create failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            _ => {
                eprintln!("wkp-hub: usage: wkp-hub tenant create <slug>");
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
            _ => {
                eprintln!("wkp-hub: usage: wkp-hub device register|revoke ...");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!(
                "wkp-hub: usage: wkp-hub migrate | tenant create <slug> | \
                 device register <tenant-slug> <public-key> | device revoke <public-key>"
            );
            std::process::exit(1);
        }
    }
}
