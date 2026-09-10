//! The hub's control plane (design 8.2, M5-1): accounts/tenants,
//! registered device keys, and RFC 8628 device-authorization grants,
//! in Postgres -- the small transactional store design 8.2 reserves
//! Postgres for, distinct from every tenant's own per-tenant SQLite
//! index (`wkp-core`'s existing index code, reused unchanged per
//! design 3.3, not duplicated here).
//!
//! **CI, not a hosted instance**: per `docs/plan/milestones.md`'s M5
//! scope notes, this milestone's exit criterion only needs something
//! CI can stand up and tear down itself (a Postgres service
//! container) to prove the schema and queries work -- nothing in this
//! module talks to, or assumes, a real hosted Supabase/RDS instance.
//! `connect` reads its connection string from `DATABASE_URL` (an env
//! var, never argv, per CLAUDE.md's secrets rule), which for local
//! development or CI names that service container, never a
//! credential this crate itself picks or stores.

pub mod grants;
mod schema;

use postgres::{Client, NoTls, Row};
use time::OffsetDateTime;

/// A tenant: one bare repo, one per-tenant index, one Unix UID at the
/// deployment layer (design 8.2) -- this table only carries the
/// control-plane's own bookkeeping row, not the repo/index themselves.
#[derive(Debug, Clone, PartialEq)]
pub struct Tenant {
    pub id: i64,
    pub slug: String,
}

/// One registered device key (design 6.4, 8.1): the public half of a
/// device's signing identity, scoped to one tenant. `revoked_at` being
/// `Some` is what [`crate::control_plane`]'s callers (`wkp-shell`,
/// M5-3) check to refuse a connection -- there is no separate "delete
/// the row" revocation path, so a revoked device's own history (which
/// tenant it belonged to, when it was revoked) is never lost.
#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub id: i64,
    pub tenant_id: i64,
    pub public_key: String,
    pub created_at: OffsetDateTime,
    pub revoked_at: Option<OffsetDateTime>,
    /// SHA-256 hex digest of this device's HTTPS bearer token (M5-6,
    /// design 8.1), if it has one -- never the plaintext, which exists
    /// only for the instant [`issue_bearer_token`] returns it.
    pub bearer_token_hash: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    MissingDatabaseUrl,
    Postgres(postgres::Error),
    /// Not a query failure -- a caller (e.g. `wkp-hub device register`)
    /// asked for something by name (a tenant slug, a device's public
    /// key) that this control plane has no row for. Distinct from
    /// `find_tenant_by_slug`/`find_device_by_public_key`'s own
    /// `Ok(None)` return, which is the right shape for a caller that
    /// treats "not found" as an ordinary branch to handle -- this
    /// variant exists for callers (the admin CLI) that instead want to
    /// `?`-propagate "not found" as a single, uniform failure.
    NotFound(String),
    /// A `/dev/urandom` read failed while generating a grant's
    /// `device_code`/`user_code` ([`grants`]).
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::MissingDatabaseUrl => write!(
                f,
                "DATABASE_URL is not set -- the control plane needs a Postgres connection \
                 string (e.g. postgres://postgres:postgres@localhost:5432/wkp_hub_test for a \
                 local/CI Postgres; see .github/workflows/rust-ci.yml's service container for \
                 the exact convention this workspace's CI uses)"
            ),
            Error::Postgres(e) => write!(f, "postgres error: {e}"),
            Error::NotFound(what) => write!(f, "not found: {what}"),
            Error::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<postgres::Error> for Error {
    fn from(e: postgres::Error) -> Self {
        Error::Postgres(e)
    }
}

/// Connects to the control plane's Postgres using `DATABASE_URL` and
/// ensures the schema exists (idempotent `CREATE TABLE IF NOT EXISTS`,
/// not a migration framework -- there is nothing deployed yet for a
/// schema to migrate *from*; revisit once this control plane is
/// actually running somewhere with data that must survive a schema
/// change).
///
/// `NoTls`: the CI/local-dev Postgres this connects to today is a
/// same-host service container with no network hop to protect (see
/// the module doc comment) -- real TLS to a real hosted instance is
/// deployment work explicitly out of scope for this milestone's task
/// list (`docs/plan/milestones.md`).
pub fn connect() -> Result<Client, Error> {
    let database_url = std::env::var("DATABASE_URL").map_err(|_| Error::MissingDatabaseUrl)?;
    let mut client = Client::connect(&database_url, NoTls)?;
    schema::create_schema(&mut client)?;
    Ok(client)
}

/// Creates a tenant with the given `slug` (the bare repo's own
/// directory name at the deployment layer, M5-4 -- unique, never
/// reused). Returns the new tenant's row.
pub fn create_tenant(client: &mut Client, slug: &str) -> Result<Tenant, Error> {
    let row = client.query_one(
        "INSERT INTO tenants (slug) VALUES ($1) RETURNING id, slug",
        &[&slug],
    )?;
    Ok(tenant_from_row(&row))
}

/// Looks up a tenant by its `slug`. `Ok(None)` for an unknown slug --
/// an ordinary, expected outcome for a caller like `wkp-hub device
/// register`, not a control-plane failure.
pub fn find_tenant_by_slug(client: &mut Client, slug: &str) -> Result<Option<Tenant>, Error> {
    let row = client.query_opt("SELECT id, slug FROM tenants WHERE slug = $1", &[&slug])?;
    Ok(row.as_ref().map(tenant_from_row))
}

/// Looks up a tenant by its `id` -- the reverse direction of
/// [`find_tenant_by_slug`], needed by anything that starts from a
/// [`Device`] row (which only carries `tenant_id`), e.g. `wkp-shell`'s
/// key-to-tenant resolution (M5-3).
pub fn find_tenant_by_id(client: &mut Client, tenant_id: i64) -> Result<Option<Tenant>, Error> {
    let row = client.query_opt("SELECT id, slug FROM tenants WHERE id = $1", &[&tenant_id])?;
    Ok(row.as_ref().map(tenant_from_row))
}

/// Registers a device's public key under `tenant_id` -- the write side
/// of what M5-2's RFC 8628 flow calls once a grant is approved, and
/// what M5-3's `wkp-shell` reads back to resolve a presented key.
pub fn register_device(
    client: &mut Client,
    tenant_id: i64,
    public_key: &str,
) -> Result<Device, Error> {
    let row = client.query_one(
        "INSERT INTO devices (tenant_id, public_key) VALUES ($1, $2) \
         RETURNING id, tenant_id, public_key, created_at, revoked_at, bearer_token_hash",
        &[&tenant_id, &public_key],
    )?;
    Ok(device_from_row(&row))
}

/// Looks up a device by its exact public key. `Ok(None)` for an
/// unknown key -- not an error, since `wkp-shell` (M5-3) needs to
/// treat "no such device" as an ordinary, expected outcome (refuse
/// the connection), not a control-plane failure.
pub fn find_device_by_public_key(
    client: &mut Client,
    public_key: &str,
) -> Result<Option<Device>, Error> {
    let row = client.query_opt(
        "SELECT id, tenant_id, public_key, created_at, revoked_at, bearer_token_hash \
         FROM devices WHERE public_key = $1",
        &[&public_key],
    )?;
    Ok(row.as_ref().map(device_from_row))
}

/// A SHA-256 hex digest of `token` -- the only form of a bearer token
/// this control plane ever stores or compares against (see
/// [`Device::bearer_token_hash`]'s own doc comment).
fn hash_bearer_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Issues a fresh HTTPS bearer token for an existing device (M5-6,
/// design 8.1) -- overwrites any previous token that device had,
/// which invalidates it, the same way rotating an API key does.
/// Returns the plaintext token: this is the only moment it exists
/// outside the caller's own hands, since only [`hash_bearer_token`]'s
/// digest is ever persisted.
pub fn issue_bearer_token(client: &mut Client, device_id: i64) -> Result<String, Error> {
    let token = grants::random_hex(32)?;
    let hash = hash_bearer_token(&token);
    let updated = client.execute(
        "UPDATE devices SET bearer_token_hash = $1 WHERE id = $2",
        &[&hash, &device_id],
    )?;
    if updated == 0 {
        return Err(Error::NotFound(format!("device {device_id}")));
    }
    Ok(token)
}

/// Looks up a device by presenting the bearer token a client claims
/// (the reverse-proxy/CGI bridge's own job, M5-6) -- hashes `token`
/// and compares against [`Device::bearer_token_hash`], never the
/// plaintext. `Ok(None)` for an unknown token, the same "ordinary,
/// expected outcome" shape [`find_device_by_public_key`] uses; callers
/// still need their own `revoked_at` check, exactly as `wkp-shell`
/// does for the SSH path -- this function does not filter revoked
/// devices out itself.
pub fn find_device_by_bearer_token(
    client: &mut Client,
    token: &str,
) -> Result<Option<Device>, Error> {
    let hash = hash_bearer_token(token);
    let row = client.query_opt(
        "SELECT id, tenant_id, public_key, created_at, revoked_at, bearer_token_hash \
         FROM devices WHERE bearer_token_hash = $1",
        &[&hash],
    )?;
    Ok(row.as_ref().map(device_from_row))
}

/// Revokes a device (sets `revoked_at` to now, if not already set).
/// Idempotent: revoking an already-revoked device leaves its original
/// `revoked_at` untouched rather than overwriting it with a later
/// timestamp.
pub fn revoke_device(client: &mut Client, device_id: i64) -> Result<(), Error> {
    client.execute(
        "UPDATE devices SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
        &[&device_id],
    )?;
    Ok(())
}

fn tenant_from_row(row: &Row) -> Tenant {
    Tenant {
        id: row.get("id"),
        slug: row.get("slug"),
    }
}

fn device_from_row(row: &Row) -> Device {
    Device {
        id: row.get("id"),
        tenant_id: row.get("tenant_id"),
        public_key: row.get("public_key"),
        created_at: row.get("created_at"),
        revoked_at: row.get("revoked_at"),
        bearer_token_hash: row.get("bearer_token_hash"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test gets its own tenant `slug` (a nanosecond-suffixed
    /// name) rather than a shared fixture -- these tests all share one
    /// real Postgres database (the CI/local service container), run
    /// concurrently, and never truncate tables between runs.
    fn unique_slug(prefix: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}-{nanos}")
    }

    #[test]
    fn create_tenant_and_register_device_round_trip() {
        let mut client = connect().expect("connect (is DATABASE_URL set? see Error::Display)");
        let tenant = create_tenant(&mut client, &unique_slug("round-trip")).expect("create_tenant");
        assert!(!tenant.slug.is_empty());

        // A unique key per run, not a fixed literal -- `public_key` is
        // UNIQUE, and this database persists across test runs (this
        // module's own doc comment on `unique_slug`), so a fixed
        // literal here would collide with a previous run's leftover
        // row the same way a fixed tenant slug would.
        let public_key = unique_slug("ssh-ed25519 AAAA...round-trip");
        let device = register_device(&mut client, tenant.id, &public_key).expect("register_device");
        assert_eq!(device.tenant_id, tenant.id);
        assert!(device.revoked_at.is_none());

        let found = find_device_by_public_key(&mut client, &public_key)
            .expect("find_device_by_public_key")
            .expect("device must be found");
        assert_eq!(found, device);
    }

    #[test]
    fn find_tenant_by_slug_round_trips_and_returns_none_for_unknown() {
        let mut client = connect().expect("connect");
        let slug = unique_slug("find-by-slug");
        let created = create_tenant(&mut client, &slug).expect("create_tenant");

        let found = find_tenant_by_slug(&mut client, &slug)
            .expect("find_tenant_by_slug")
            .expect("tenant must be found");
        assert_eq!(found, created);

        assert!(
            find_tenant_by_slug(&mut client, &unique_slug("never-created"))
                .expect("find_tenant_by_slug")
                .is_none()
        );
    }

    #[test]
    fn find_tenant_by_id_round_trips_and_returns_none_for_unknown() {
        let mut client = connect().expect("connect");
        let created =
            create_tenant(&mut client, &unique_slug("find-by-id")).expect("create_tenant");

        let found = find_tenant_by_id(&mut client, created.id)
            .expect("find_tenant_by_id")
            .expect("tenant must be found");
        assert_eq!(found, created);

        assert!(find_tenant_by_id(&mut client, -1)
            .expect("find_tenant_by_id")
            .is_none());
    }

    #[test]
    fn find_device_by_public_key_returns_none_for_an_unknown_key() {
        let mut client = connect().expect("connect");
        let found =
            find_device_by_public_key(&mut client, "ssh-ed25519 this-key-was-never-registered")
                .expect("find_device_by_public_key");
        assert!(found.is_none());
    }

    /// M5-1's own acceptance criterion: a revoked device is
    /// distinguishable from an active one.
    #[test]
    fn revoke_device_sets_revoked_at_and_is_idempotent() {
        let mut client = connect().expect("connect");
        let tenant = create_tenant(&mut client, &unique_slug("revoke")).expect("create_tenant");
        let device = register_device(&mut client, tenant.id, &unique_slug("device-key"))
            .expect("register_device");
        assert!(device.revoked_at.is_none());

        revoke_device(&mut client, device.id).expect("revoke_device");
        let after_first = find_device_by_public_key(&mut client, &device.public_key)
            .expect("find after first revoke")
            .expect("device must still be found (row kept, not deleted)");
        assert!(
            after_first.revoked_at.is_some(),
            "a revoked device must have revoked_at set"
        );

        revoke_device(&mut client, device.id).expect("second revoke_device must not error");
        let after_second = find_device_by_public_key(&mut client, &device.public_key)
            .expect("find after second revoke")
            .expect("device must still be found");
        assert_eq!(
            after_second.revoked_at, after_first.revoked_at,
            "revoking an already-revoked device must not move its revoked_at forward"
        );
    }

    #[test]
    fn devices_in_different_tenants_are_independent() {
        let mut client = connect().expect("connect");
        let tenant_a =
            create_tenant(&mut client, &unique_slug("tenant-a")).expect("create tenant a");
        let tenant_b =
            create_tenant(&mut client, &unique_slug("tenant-b")).expect("create tenant b");
        assert_ne!(tenant_a.id, tenant_b.id);

        let device_a = register_device(&mut client, tenant_a.id, &unique_slug("key-a"))
            .expect("register device a");
        let device_b = register_device(&mut client, tenant_b.id, &unique_slug("key-b"))
            .expect("register device b");

        revoke_device(&mut client, device_a.id).expect("revoke device a");

        let refreshed_a = find_device_by_public_key(&mut client, &device_a.public_key)
            .expect("find a")
            .expect("device a found");
        let refreshed_b = find_device_by_public_key(&mut client, &device_b.public_key)
            .expect("find b")
            .expect("device b found");
        assert!(refreshed_a.revoked_at.is_some());
        assert!(
            refreshed_b.revoked_at.is_none(),
            "revoking a device in tenant A must not affect tenant B's device"
        );
    }

    #[test]
    fn issue_bearer_token_round_trips_and_never_stores_the_plaintext() {
        let mut client = connect().expect("connect");
        let tenant = create_tenant(&mut client, &unique_slug("bearer-token")).expect("tenant");
        let device = register_device(&mut client, tenant.id, &unique_slug("device-key"))
            .expect("register_device");
        assert!(
            device.bearer_token_hash.is_none(),
            "a freshly registered device has no bearer token yet"
        );

        let token = issue_bearer_token(&mut client, device.id).expect("issue_bearer_token");
        assert!(!token.is_empty());

        let found = find_device_by_bearer_token(&mut client, &token)
            .expect("find_device_by_bearer_token")
            .expect("device must be found by its own token");
        assert_eq!(found.id, device.id);
        assert_ne!(
            found.bearer_token_hash.as_deref(),
            Some(token.as_str()),
            "the stored hash must never equal the plaintext token"
        );
    }

    #[test]
    fn find_device_by_bearer_token_returns_none_for_an_unknown_token() {
        let mut client = connect().expect("connect");
        assert!(
            find_device_by_bearer_token(&mut client, "this-token-was-never-issued")
                .expect("find_device_by_bearer_token")
                .is_none()
        );
    }

    /// Rotation: issuing a new token for the same device invalidates
    /// the old one, the same way rotating an API key does.
    #[test]
    fn issuing_a_new_bearer_token_invalidates_the_previous_one() {
        let mut client = connect().expect("connect");
        let tenant = create_tenant(&mut client, &unique_slug("bearer-rotate")).expect("tenant");
        let device = register_device(&mut client, tenant.id, &unique_slug("device-key"))
            .expect("register_device");

        let first = issue_bearer_token(&mut client, device.id).expect("first issue");
        let second = issue_bearer_token(&mut client, device.id).expect("second issue");
        assert_ne!(first, second);

        assert!(
            find_device_by_bearer_token(&mut client, &first)
                .expect("find first")
                .is_none(),
            "the first token must no longer resolve to any device"
        );
        let found_second = find_device_by_bearer_token(&mut client, &second)
            .expect("find second")
            .expect("second token must resolve");
        assert_eq!(found_second.id, device.id);
    }

    #[test]
    fn issue_bearer_token_fails_for_an_unknown_device() {
        let mut client = connect().expect("connect");
        let result = issue_bearer_token(&mut client, -1);
        assert!(matches!(result, Err(Error::NotFound(_))));
    }
}
