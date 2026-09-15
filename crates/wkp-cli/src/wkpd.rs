//! `wkpd`: watch-triggered automation (M3-8, design 4.2) -- a thin wrapper
//! over M3-1 through M3-4's already-standalone logic (per-device
//! auto-commit, incremental re-index, `wkp sync`), run on a
//! watch-triggered cycle instead of requiring a human to invoke each of
//! those by hand. No new sync logic of its own.
//!
//! Binds only a Unix domain socket (never a TCP port), mode `0600`, peer
//! UID verified on every connection -- design 7.1's threat model names
//! this exact check as the control against a compromised local process
//! talking to `wkpd`. See
//! `docs/adr/0004-wkpd-peer-credential-verification.md` for why that
//! check is Linux-only today: `wkpd` deliberately refuses to bind at all
//! on any other target OS rather than running with no peer check, or an
//! unverified one.
//!
//! Scope note: a debounced watch cycle auto-commits added/modified paths
//! onto this device's branch (the same [`wkp_git::sync::commit_to_device_branch`]
//! `wkp remember` uses); a cycle that only saw deletions skips
//! auto-commit for that cycle rather than half-implementing delete
//! handling here -- the file is still gone from the working tree, it
//! simply is not yet reflected in a new commit until a future change
//! (or a manual `wkp` write) captures it too. Not silently lossy, just a
//! deliberately narrow first version.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use notify::Watcher;

use crate::{index_cmd, sync_cmd};

pub(crate) struct WkpdOptions {
    pub(crate) path: PathBuf,
    pub(crate) principal: String,
    pub(crate) signing_key_file: PathBuf,
    pub(crate) remote: String,
    pub(crate) socket_path: Option<PathBuf>,
    pub(crate) debounce_ms: u64,
}

/// `wkpd --principal <p> --signing-key-file <path> [--path <dir>]
/// [--remote <name>] [--socket-path <path>] [--debounce-ms <n>]`.
/// `--principal`/`--signing-key-file` are required, matching every other
/// write command's convention (`wkp remember`, `wkp promote`) rather than
/// inventing a separate device-identity key-management system here.
pub(crate) fn parse_wkpd_args(
    mut args: impl Iterator<Item = String>,
) -> Result<WkpdOptions, String> {
    let mut path = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut principal: Option<String> = None;
    let mut signing_key_file: Option<PathBuf> = None;
    let mut remote = "origin".to_string();
    let mut socket_path: Option<PathBuf> = None;
    let mut debounce_ms: u64 = 2000;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--path" => path = PathBuf::from(args.next().ok_or("--path requires a value")?),
            "--principal" => {
                principal = Some(args.next().ok_or("--principal requires a value")?);
            }
            "--signing-key-file" => {
                signing_key_file = Some(PathBuf::from(
                    args.next().ok_or("--signing-key-file requires a value")?,
                ));
            }
            "--remote" => remote = args.next().ok_or("--remote requires a value")?,
            "--socket-path" => {
                socket_path = Some(PathBuf::from(
                    args.next().ok_or("--socket-path requires a value")?,
                ));
            }
            "--debounce-ms" => {
                let v = args.next().ok_or("--debounce-ms requires a value")?;
                debounce_ms = v
                    .parse()
                    .map_err(|_| format!("invalid --debounce-ms value: {v}"))?;
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
    }

    let principal = principal.ok_or("wkpd requires --principal <principal>")?;
    let signing_key_file = signing_key_file.ok_or("wkpd requires --signing-key-file <path>")?;

    Ok(WkpdOptions {
        path,
        principal,
        signing_key_file,
        remote,
        socket_path,
        debounce_ms,
    })
}

fn socket_path_for(opts: &WkpdOptions) -> PathBuf {
    opts.socket_path
        .clone()
        .unwrap_or_else(|| opts.path.join(".wkp/wkpd.sock"))
}

/// Binds `path` as a Unix domain socket, mode `0600` regardless of the
/// process's umask (set explicitly after bind, since a socket file's
/// permissions at creation follow the umask like any other file).
/// Linux-only -- see the module doc comment and ADR-0004.
#[cfg(target_os = "linux")]
fn bind_socket(path: &Path) -> Result<UnixListener, String> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // A stale socket file from a previous, no-longer-running wkpd must not
    // block a fresh bind.
    let _ = std::fs::remove_file(path);
    let listener =
        UnixListener::bind(path).map_err(|e| format!("binding {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    Ok(listener)
}

#[cfg(not(target_os = "linux"))]
fn bind_socket(_path: &Path) -> Result<UnixListener, String> {
    Err(
        "wkpd's UDS peer-credential check is only implemented on Linux today \
         (see docs/adr/0004-wkpd-peer-credential-verification.md); refusing to \
         start rather than running without it."
            .to_string(),
    )
}

/// Whether `stream`'s connecting peer is this same process's own user --
/// the entire security-relevant check `wkpd`'s socket exists to enforce
/// (design 7.1). `false` on any error (an unresolvable peer is treated as
/// untrusted, not as trusted-by-default).
#[cfg(target_os = "linux")]
fn peer_is_self(stream: &UnixStream) -> bool {
    use std::os::fd::AsFd;
    let my_uid = rustix::process::getuid();
    matches!(
        rustix::net::sockopt::socket_peercred(stream.as_fd()),
        Ok(cred) if cred.uid == my_uid
    )
}

/// Unreachable in practice on any platform other than Linux: [`bind_socket`]
/// already refuses to bind the control socket at all there, so
/// [`serve_control_socket`] is never actually entered with a connection to
/// check. Still needs to exist so the crate compiles for every target this
/// workspace cross-compiles for -- `false` is the fail-closed answer if it
/// were ever somehow reached.
#[cfg(not(target_os = "linux"))]
fn peer_is_self(_stream: &UnixStream) -> bool {
    false
}

/// Accepts connections on `listener` forever, verifying each peer before
/// replying -- a non-matching peer is dropped without a response (refused,
/// per M3-8's acceptance criteria), not merely logged and allowed through.
fn serve_control_socket(listener: UnixListener) {
    for incoming in listener.incoming() {
        let Ok(stream) = incoming else { continue };
        if peer_is_self(&stream) {
            respond_ok(stream);
        } else {
            eprintln!("wkpd: refused a connection from an unrecognized peer");
        }
    }
}

fn respond_ok(mut stream: UnixStream) {
    use std::io::Write;
    let _ = stream.write_all(b"ok\n");
}

/// One watch-triggered cycle (design 4.2): auto-commit local changes onto
/// this device's branch, re-index, then opportunistically sync. Each step
/// is independent -- a failure at any point is reported to the caller as
/// an `Err` (never a panic), and [`run_wkpd`]'s loop logs it and waits for
/// the next change rather than exiting.
fn run_cycle(opts: &WkpdOptions) -> Result<(), String> {
    let changes = wkp_git::detect_changes(&opts.path)?;
    let mut added_or_modified: Vec<PathBuf> = changes.added;
    added_or_modified.extend(changes.modified);
    for renamed in &changes.renamed {
        added_or_modified.push(renamed.to.clone());
    }

    if !added_or_modified.is_empty() {
        let device_id = wkp_git::sync::device_id(&opts.path)?;
        let provenance = wkp_git::provenance::Provenance {
            actor: Some(opts.principal.clone()),
            session: None,
            source: Some("wkpd".to_string()),
            confidence: None,
        };
        let subject = format!(
            "wkpd: auto-commit {} changed file(s)",
            added_or_modified.len()
        );
        wkp_git::sync::commit_to_device_branch(
            &opts.path,
            &device_id,
            &added_or_modified,
            &subject,
            &opts.principal,
            &opts.signing_key_file,
            &provenance,
        )?;
    }

    index_cmd::run_index_cli(&index_cmd::IndexOptions {
        path: opts.path.clone(),
        embed_url: None,
        embed_model: None,
        embed_key_file: None,
    })?;

    if let Err(msg) = sync_cmd::run_sync(&sync_cmd::SyncOptions {
        path: opts.path.clone(),
        remote: opts.remote.clone(),
    }) {
        eprintln!("wkpd: opportunistic sync failed (will retry on the next change): {msg}");
    }

    Ok(())
}

/// `wkpd`'s main loop: binds the control socket (its own thread), then
/// watches `opts.path` for changes, debouncing (`opts.debounce_ms`,
/// default 2s per design 4.2) before running one [`run_cycle`]. Runs
/// forever; returns only on a setup failure (bad socket path, watcher
/// couldn't attach) -- once the loop starts, per-cycle failures are
/// logged and retried on the next change, never propagated as a reason to
/// stop.
pub(crate) fn run_wkpd(opts: &WkpdOptions) -> Result<(), String> {
    let socket_path = socket_path_for(opts);
    let listener = bind_socket(&socket_path)?;
    std::thread::spawn(move || serve_control_socket(listener));

    let (tx, rx) = mpsc::channel::<()>();
    let mut watcher = notify::recommended_watcher(move |_res: notify::Result<notify::Event>| {
        let _ = tx.send(());
    })
    .map_err(|e| e.to_string())?;
    watcher
        .watch(&opts.path, notify::RecursiveMode::Recursive)
        .map_err(|e| e.to_string())?;

    println!(
        "wkpd: watching {} (debounce {}ms, socket {})",
        opts.path.display(),
        opts.debounce_ms,
        socket_path.display()
    );

    loop {
        if rx.recv().is_err() {
            break;
        }
        // Coalesce a burst of events (e.g. an editor's save-as-rename
        // dance) into a single cycle rather than one per raw fs event.
        while rx
            .recv_timeout(Duration::from_millis(opts.debounce_ms))
            .is_ok()
        {}

        if let Err(msg) = run_cycle(opts) {
            eprintln!("wkpd: cycle failed (will retry on the next change): {msg}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;

    #[test]
    fn parse_wkpd_args_reads_all_flags() {
        let opts = parse_wkpd_args(args(&[
            "--principal",
            "agent:claude-code@host",
            "--signing-key-file",
            "/tmp/key",
            "--path",
            "/tmp/store",
            "--remote",
            "upstream",
            "--socket-path",
            "/tmp/custom.sock",
            "--debounce-ms",
            "500",
        ]))
        .expect("parse_wkpd_args");
        assert_eq!(opts.principal, "agent:claude-code@host");
        assert_eq!(opts.signing_key_file, PathBuf::from("/tmp/key"));
        assert_eq!(opts.path, PathBuf::from("/tmp/store"));
        assert_eq!(opts.remote, "upstream");
        assert_eq!(opts.socket_path, Some(PathBuf::from("/tmp/custom.sock")));
        assert_eq!(opts.debounce_ms, 500);
    }

    #[test]
    fn parse_wkpd_args_requires_principal_and_signing_key_file() {
        assert!(parse_wkpd_args(args(&[])).is_err());
        assert!(parse_wkpd_args(args(&["--principal", "agent:x"])).is_err());
    }

    #[test]
    fn parse_wkpd_args_defaults_remote_and_debounce() {
        let opts = parse_wkpd_args(args(&[
            "--principal",
            "agent:x",
            "--signing-key-file",
            "/tmp/key",
        ]))
        .expect("parse_wkpd_args");
        assert_eq!(opts.remote, "origin");
        assert_eq!(opts.debounce_ms, 2000);
        assert_eq!(opts.socket_path, None);
    }

    #[test]
    fn bind_socket_creates_a_mode_0600_socket_file() {
        use std::os::unix::fs::PermissionsExt;

        let temp = temp_dir("wkpd-bind-socket");
        let socket_path = temp.path().join("wkpd.sock");
        let _listener = bind_socket(&socket_path).expect("bind_socket");

        let mode = std::fs::metadata(&socket_path)
            .expect("stat socket file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn bind_socket_is_idempotent_over_a_stale_socket_file() {
        let temp = temp_dir("wkpd-bind-socket-stale");
        let socket_path = temp.path().join("wkpd.sock");
        {
            let _first = bind_socket(&socket_path).expect("first bind_socket");
        }
        // The first listener is dropped (closed) but the socket file may
        // still exist on disk -- a second bind must not fail because of it.
        let _second = bind_socket(&socket_path).expect("second bind_socket");
    }

    #[test]
    fn peer_is_self_accepts_a_same_user_connection() {
        let temp = temp_dir("wkpd-peer-is-self");
        let socket_path = temp.path().join("wkpd.sock");
        let listener = bind_socket(&socket_path).expect("bind_socket");

        let _client = UnixStream::connect(&socket_path).expect("connect");
        let (server_side, _addr) = listener.accept().expect("accept");
        assert!(peer_is_self(&server_side));
    }

    /// M3-8's own acceptance criterion, end to end: starting `wkpd`,
    /// writing a file, confirming a sync cycle runs without a human
    /// invoking `wkp sync` directly -- the changed file lands as a real
    /// commit on the device's own branch.
    #[test]
    fn wkpd_auto_commits_a_changed_file_without_a_manual_sync_call() {
        let temp = temp_dir("wkpd-end-to-end");
        let dir = temp.path();
        test_init(dir).expect("run_init");
        let key = generate_test_key_and_register(dir, "agent:claude-code@host");

        let opts = WkpdOptions {
            path: dir.to_path_buf(),
            principal: "agent:claude-code@host".to_string(),
            signing_key_file: key.private_path,
            remote: "origin".to_string(),
            socket_path: None,
            debounce_ms: 50,
        };

        let watch_path = opts.path.clone();
        std::thread::spawn(move || {
            let mut wkpd_opts = opts;
            wkpd_opts.path = watch_path;
            let _ = run_wkpd(&wkpd_opts);
        });

        // Give the watcher a moment to attach before the write it needs
        // to observe.
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(
            dir.join("watched.md"),
            "written by a plain editor, not wkp remember\n",
        )
        .expect("write watched.md");

        let device_id = wkp_git::sync::device_id(dir).expect("device_id");
        let branch = wkp_git::sync::device_branch_name(&device_id);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if wkp_git::local_branch_exists(dir, &branch) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "wkpd never committed the watched change within the timeout"
            );
            std::thread::sleep(Duration::from_millis(100));
        }

        // This store had zero commits before wkpd's cycle ran, so the
        // auto-commit takes commit_to_device_branch's bootstrap path
        // (see sync.rs) -- HEAD is retargeted at the device branch
        // directly, with no intervening checkout, so the working tree
        // file wkpd observed being written is still right there on disk.
        assert_eq!(
            wkp_git::current_branch(dir).expect("current_branch"),
            branch
        );
        let content = std::fs::read_to_string(dir.join("watched.md")).expect("read watched.md");
        assert!(content.contains("written by a plain editor"));
    }
}
