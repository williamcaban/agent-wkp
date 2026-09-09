//! RFC 8628 device-authorization grants (design 6.4, 8.1, M5-2): the
//! `device_grants` table M5-1's schema already created for this task
//! to populate and query.
//!
//! The client (`wkp hub register`) already knows which tenant it wants
//! to join (a `--tenant <slug>` the human configuring the device
//! provides) and generates its own keypair before ever contacting the
//! hub -- so `tenant_id` and `public_key` are both known and stored at
//! grant-creation time, matching `device_grants.tenant_id NOT NULL`
//! exactly as M5-1 already defined it. Approval only ever needs a
//! `user_code`, never a second round of "which tenant, which key" --
//! those were decided up front by the device, not by whoever clicks
//! approve.

use super::{Client, Device, Error};
use time::{Duration, OffsetDateTime};

/// How long an unapproved grant stays valid -- RFC 8628 calls this
/// `expires_in`; ten minutes matches the range most real device-flow
/// implementations (GitHub, Google) use.
pub const GRANT_TTL: Duration = Duration::minutes(10);

/// How often a well-behaved polling client should retry -- RFC 8628's
/// `interval`, returned to the client so it (not the server) paces
/// itself; not separately enforced server-side in this milestone's
/// minimal implementation (an M5-6-or-later hardening concern, not
/// this task's -- see the module doc comment's scope generally).
pub const POLL_INTERVAL_SECONDS: i64 = 5;

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceGrant {
    pub id: i64,
    pub tenant_id: i64,
    pub device_code: String,
    pub user_code: String,
    pub public_key: String,
    pub expires_at: OffsetDateTime,
    pub approved_device_id: Option<i64>,
}

impl DeviceGrant {
    pub fn is_expired(&self) -> bool {
        OffsetDateTime::now_utc() >= self.expires_at
    }
}

/// 26 unambiguous uppercase characters (no `0`/`O`, no `1`/`I`/`L`) --
/// GitHub's own device-flow user codes use the same restricted
/// alphabet for the same reason: a human is about to type this by
/// hand from a screen.
const USER_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// A cryptographically random hex string of `byte_len` bytes, read
/// straight off `/dev/urandom` -- the same pattern
/// `wkp_git::sync::generate_device_id` already established (CLAUDE.md's
/// slim-core rule: this is exactly the kind of thing a `rand` crate
/// would otherwise exist only to provide), reused here rather than
/// adding a second random-generation dependency to this workspace.
fn random_hex(byte_len: usize) -> Result<String, Error> {
    use std::io::Read;
    let mut bytes = vec![0u8; byte_len];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(Error::Io)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// An 8-character, `XXXX-XXXX`-formatted code from
/// [`USER_CODE_ALPHABET`] -- short and typeable, unlike `device_code`
/// (long and never shown to a human at all).
fn random_user_code() -> Result<String, Error> {
    use std::io::Read;
    let mut raw = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut raw))
        .map_err(Error::Io)?;
    let chars: String = raw
        .iter()
        .map(|b| USER_CODE_ALPHABET[(*b as usize) % USER_CODE_ALPHABET.len()] as char)
        .collect();
    Ok(format!("{}-{}", &chars[0..4], &chars[4..8]))
}

/// Creates a new, unapproved grant for `tenant_id` naming `public_key`
/// as the device key it wants registered once a human approves.
pub fn create_device_grant(
    client: &mut Client,
    tenant_id: i64,
    public_key: &str,
) -> Result<DeviceGrant, Error> {
    let device_code = random_hex(32)?;
    let user_code = random_user_code()?;
    let expires_at = OffsetDateTime::now_utc() + GRANT_TTL;

    let row = client.query_one(
        "INSERT INTO device_grants (tenant_id, device_code, user_code, public_key, expires_at) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, tenant_id, device_code, user_code, public_key, expires_at, approved_device_id",
        &[&tenant_id, &device_code, &user_code, &public_key, &expires_at],
    )?;
    Ok(grant_from_row(&row))
}

pub fn find_grant_by_device_code(
    client: &mut Client,
    device_code: &str,
) -> Result<Option<DeviceGrant>, Error> {
    let row = client.query_opt(
        "SELECT id, tenant_id, device_code, user_code, public_key, expires_at, \
         approved_device_id FROM device_grants WHERE device_code = $1",
        &[&device_code],
    )?;
    Ok(row.as_ref().map(grant_from_row))
}

pub fn find_grant_by_user_code(
    client: &mut Client,
    user_code: &str,
) -> Result<Option<DeviceGrant>, Error> {
    let row = client.query_opt(
        "SELECT id, tenant_id, device_code, user_code, public_key, expires_at, \
         approved_device_id FROM device_grants WHERE user_code = $1",
        &[&user_code],
    )?;
    Ok(row.as_ref().map(grant_from_row))
}

/// Approves `grant`: registers its `public_key` under its `tenant_id`
/// (via [`super::register_device`]) and records the result as the
/// grant's `approved_device_id`. Idempotent -- approving an
/// already-approved grant a second time returns the same device it
/// returned the first time, rather than trying (and failing, on
/// `devices.public_key`'s `UNIQUE` constraint) to register the same
/// key twice.
pub fn approve_grant(client: &mut Client, grant: &DeviceGrant) -> Result<Device, Error> {
    if let Some(device_id) = grant.approved_device_id {
        if let Some(device) = super::find_device_by_public_key(client, &grant.public_key)? {
            debug_assert_eq!(device.id, device_id);
            return Ok(device);
        }
    }

    let device = super::register_device(client, grant.tenant_id, &grant.public_key)?;
    client.execute(
        "UPDATE device_grants SET approved_device_id = $1 WHERE id = $2",
        &[&device.id, &grant.id],
    )?;
    Ok(device)
}

fn grant_from_row(row: &postgres::Row) -> DeviceGrant {
    DeviceGrant {
        id: row.get("id"),
        tenant_id: row.get("tenant_id"),
        device_code: row.get("device_code"),
        user_code: row.get("user_code"),
        public_key: row.get("public_key"),
        expires_at: row.get("expires_at"),
        approved_device_id: row.get("approved_device_id"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::{connect, create_tenant};

    fn unique_slug(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}-{nanos}")
    }

    #[test]
    fn create_find_and_approve_a_grant_round_trip() {
        let mut client = connect().expect("connect");
        let tenant = create_tenant(&mut client, &unique_slug("grant-round-trip")).expect("tenant");
        let public_key = unique_slug("ssh-ed25519 AAAA...grant");

        let grant =
            create_device_grant(&mut client, tenant.id, &public_key).expect("create_device_grant");
        assert!(!grant.is_expired());
        assert!(grant.approved_device_id.is_none());
        assert_eq!(grant.device_code.len(), 64, "32 bytes as hex");
        assert_eq!(grant.user_code.len(), 9, "XXXX-XXXX");

        let by_device_code = find_grant_by_device_code(&mut client, &grant.device_code)
            .expect("find_grant_by_device_code")
            .expect("grant must be found by device_code");
        assert_eq!(by_device_code, grant);

        let by_user_code = find_grant_by_user_code(&mut client, &grant.user_code)
            .expect("find_grant_by_user_code")
            .expect("grant must be found by user_code");
        assert_eq!(by_user_code, grant);

        let device = approve_grant(&mut client, &grant).expect("approve_grant");
        assert_eq!(device.tenant_id, tenant.id);
        assert_eq!(device.public_key, public_key);

        let refreshed = find_grant_by_device_code(&mut client, &grant.device_code)
            .expect("find after approve")
            .expect("grant still found");
        assert_eq!(refreshed.approved_device_id, Some(device.id));
    }

    #[test]
    fn approve_grant_is_idempotent() {
        let mut client = connect().expect("connect");
        let tenant = create_tenant(&mut client, &unique_slug("grant-idempotent")).expect("tenant");
        let public_key = unique_slug("ssh-ed25519 AAAA...idempotent");
        let grant =
            create_device_grant(&mut client, tenant.id, &public_key).expect("create_device_grant");

        let first = approve_grant(&mut client, &grant).expect("first approve");
        let refreshed = find_grant_by_device_code(&mut client, &grant.device_code)
            .expect("find after first approve")
            .expect("grant found");
        let second = approve_grant(&mut client, &refreshed).expect("second approve");

        assert_eq!(
            first, second,
            "approving twice must not create a second device"
        );
    }

    #[test]
    fn find_grant_returns_none_for_unknown_codes() {
        let mut client = connect().expect("connect");
        assert!(find_grant_by_device_code(&mut client, "never-issued")
            .expect("find_grant_by_device_code")
            .is_none());
        assert!(find_grant_by_user_code(&mut client, "ZZZZ-ZZZZ")
            .expect("find_grant_by_user_code")
            .is_none());
    }
}
