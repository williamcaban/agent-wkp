//! The control plane's Postgres schema (design 8.2, M5-1).
//!
//! `IF NOT EXISTS` everywhere, run on every [`super::connect`] rather
//! than via a separate migration step -- appropriate for a schema
//! nothing is deployed against yet (see [`super::connect`]'s own doc
//! comment). Revisit with a real migration tool once this control
//! plane holds data across a schema change in production.
//!
//! **Real bug found running this crate's own tests concurrently, not
//! assumed**: `CREATE TABLE IF NOT EXISTS` is not actually safe under
//! concurrent execution in Postgres -- two connections racing to
//! create the same table can both pass the existence check before
//! either commits, and the *implicit sequence* a `BIGSERIAL` column
//! creates has no `IF NOT EXISTS` protection at all, so the loser gets
//! a real `duplicate key value violates unique constraint
//! "pg_class_relname_nsp_index"` error, not a harmless no-op. Multiple
//! `wkp-hub` processes (or, as here, multiple test threads) calling
//! [`super::connect`] at once hits this for real, not just in theory.
//! Fixed with a session-scoped `pg_advisory_lock` around the whole
//! schema batch, so concurrent callers serialize on schema creation
//! instead of racing.

use postgres::Client;

/// An arbitrary, fixed lock key naming "this crate's schema-creation
/// critical section" -- any constant works for `pg_advisory_lock`, as
/// long as it's the same one every caller uses and doesn't collide
/// with a lock key some other part of this control plane might take
/// for an unrelated reason (nothing else in this crate calls
/// `pg_advisory_lock` yet).
const SCHEMA_LOCK_KEY: i64 = 0x776b705f68756221; // "wkp_hub!" as bytes, just a fixed, recognizable constant

pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tenants (
    id BIGSERIAL PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- M5-7 (ADR-0010): per-tenant pod lifecycle state. `always_warm`
    -- opts a tenant out of on-demand start/idle-teardown entirely, for
    -- one that can't tolerate a cold start. `pod_running`/
    -- `pod_started_at` reflect this control plane's own last-known
    -- state, not a live poll of the container runtime -- the
    -- provisioning/reaper code (a later M5-7 task) is what keeps them
    -- honest, the same way `devices.revoked_at` is a fact this table
    -- records, not a live check against anything external.
    -- `last_active_at` is what the reaper's idle-timeout decision reads;
    -- it starts equal to `created_at` (a tenant is not "idle" before it
    -- has ever had a chance to be active) rather than NULL, so a reaper
    -- query never needs a NULL-handling special case.
    always_warm BOOLEAN NOT NULL DEFAULT false,
    pod_running BOOLEAN NOT NULL DEFAULT false,
    pod_started_at TIMESTAMPTZ,
    last_active_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS devices (
    id BIGSERIAL PRIMARY KEY,
    tenant_id BIGINT NOT NULL REFERENCES tenants (id),
    public_key TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at TIMESTAMPTZ,
    -- M5-6 (design 8.1's HTTPS transport): a SHA-256 hex digest, never
    -- the plaintext token itself -- same reasoning as a password hash,
    -- even though this secret is high-entropy and machine-generated
    -- rather than user-chosen (so a fast hash is fine here; there is
    -- no offline dictionary attack to slow down, unlike a real
    -- password). Nullable: an SSH-only device never gets one. UNIQUE
    -- so a lookup by presented token is a single indexed equality
    -- check, matching `public_key`'s own lookup shape exactly.
    bearer_token_hash TEXT UNIQUE
);

CREATE INDEX IF NOT EXISTS devices_tenant_id_idx ON devices (tenant_id);

-- RFC 8628 device-authorization grants (M5-2's own table to populate
-- and query; created here so M5-2 needs no schema change of its own).
-- `user_code` is what a human types into the verification page;
-- `device_code` is what the polling CLI holds and never displays.
-- `approved_device_id` is set once a human approves the grant and a
-- device row (this module's `register_device`) exists for it.
CREATE TABLE IF NOT EXISTS device_grants (
    id BIGSERIAL PRIMARY KEY,
    tenant_id BIGINT NOT NULL REFERENCES tenants (id),
    device_code TEXT NOT NULL UNIQUE,
    user_code TEXT NOT NULL UNIQUE,
    public_key TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    approved_device_id BIGINT REFERENCES devices (id)
);
"#;

pub(super) fn create_schema(client: &mut Client) -> Result<(), postgres::Error> {
    client.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK_KEY])?;
    let result = client.batch_execute(SCHEMA);
    // Best-effort unlock: report the batch's own result either way, not
    // an unlock failure that would mask it -- an advisory lock this
    // session fails to explicitly release still releases automatically
    // when the session/connection ends, so a failed unlock here is not
    // a stuck lock, just a slightly later release than usual.
    let _ = client.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK_KEY]);
    result
}
