//! `git http-backend` as a CGI subprocess (design 8.1's HTTPS
//! transport, M5-6). The only plumbing in this module that talks to
//! git via environment variables and CGI framing instead of argv --
//! `git http-backend` *is* a CGI program (git's own smart-HTTP
//! implementation), and this is the one place anything in the
//! workspace needs to speak that protocol rather than run an ordinary
//! git subcommand.
//!
//! Buffers the whole request body and the whole CGI response in
//! memory rather than streaming -- correct, not yet optimized for a
//! very large push/clone; revisit under real load per this crate's own
//! established posture elsewhere (`control_plane`'s one-connection-
//! per-request doc comment makes the same call for the same reason).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// One CGI request `wkp-hub`'s own HTTP server (M5-6) hands off to
/// `git http-backend` after validating the caller's bearer token and
/// deciding it may act as `remote_user` against this tenant.
pub struct CgiRequest<'a> {
    pub method: &'a str,
    /// e.g. `/acme.git/info/refs` or `/acme.git/git-upload-pack` --
    /// git http-backend's own convention for naming which repo (under
    /// `project_root`) and which of its smart-HTTP endpoints this
    /// request targets.
    pub path_info: &'a str,
    pub query_string: &'a str,
    pub content_type: Option<&'a str>,
    /// Set as `REMOTE_USER` -- git http-backend does not itself
    /// enforce anything with this, but records it in its own traces;
    /// the actual authorization decision (which token maps to which
    /// tenant, whether it's revoked) happens entirely before this
    /// function is ever called, in `wkp-hub`'s own request handler.
    pub remote_user: &'a str,
    pub body: &'a [u8],
}

/// A parsed CGI response: git http-backend's own convention is a
/// `Status: <code> <reason>` header line for anything other than 200
/// (verified by hand: a request for a nonexistent repo produces
/// `Status: 404 Not Found` with exit code 0 regardless -- this
/// function's own caller must read `status`, never the subprocess's
/// exit code, to know what happened), CRLF-terminated headers, a
/// blank line, then the response body verbatim.
pub struct CgiResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Runs `git http-backend` against `project_root` (the same
/// `GIT_PROJECT_ROOT` convention `wkp-shell`'s fixed per-tenant repo
/// paths already use, M5-3/M5-4) for one CGI request.
pub fn run_http_backend(project_root: &Path, req: &CgiRequest) -> Result<CgiResponse, String> {
    let mut command = Command::new("git");
    command
        .arg("http-backend")
        .env_clear()
        // `PATH` must survive `env_clear()`: git http-backend execs
        // git-upload-pack/git-receive-pack as its own child process,
        // found via PATH like any other git subcommand invocation.
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("GIT_PROJECT_ROOT", project_root)
        // Every repo under `project_root` is one of this hub's own
        // tenant repos (never an arbitrary directory a client's own
        // path could otherwise export) -- see `wkp-shell`'s own doc
        // comment on why the path a client names is never trusted for
        // more than an index into that fixed set.
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", req.method)
        .env("PATH_INFO", req.path_info)
        .env("QUERY_STRING", req.query_string)
        .env("REMOTE_USER", req.remote_user)
        .env("CONTENT_LENGTH", req.body.len().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(content_type) = req.content_type {
        command.env("CONTENT_TYPE", content_type);
    }

    let mut child = command.spawn().map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .expect("child spawned with Stdio::piped() stdin")
        .write_all(req.body)
        .map_err(|e| e.to_string())?;
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    parse_cgi_response(&output.stdout)
}

fn parse_cgi_response(raw: &[u8]) -> Result<CgiResponse, String> {
    let separator = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, i + 4))
        .or_else(|| {
            raw.windows(2)
                .position(|w| w == b"\n\n")
                .map(|i| (i, i + 2))
        })
        .ok_or("git http-backend produced no CGI header/body separator")?;
    let (header_end, body_start) = separator;

    let mut status = 200u16;
    let mut headers = Vec::new();
    for line in String::from_utf8_lossy(&raw[..header_end]).lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if key.eq_ignore_ascii_case("Status") {
            status = value
                .split_whitespace()
                .next()
                .and_then(|code| code.parse().ok())
                .unwrap_or(200);
        } else {
            headers.push((key.to_string(), value.to_string()));
        }
    }

    Ok(CgiResponse {
        status,
        headers,
        body: raw[body_start..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::init_bare_repo;

    fn temp_project_root(label: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("wkp-git-http-backend-{label}-"))
            .tempdir()
            .expect("create temp project root")
    }

    #[test]
    fn info_refs_against_an_existing_repo_succeeds() {
        let temp = temp_project_root("info-refs");
        init_bare_repo(&temp.path().join("acme.git")).expect("init_bare_repo");

        let response = run_http_backend(
            temp.path(),
            &CgiRequest {
                method: "GET",
                path_info: "/acme.git/info/refs",
                query_string: "service=git-upload-pack",
                content_type: None,
                remote_user: "test-device",
                body: b"",
            },
        )
        .expect("run_http_backend");

        assert_eq!(response.status, 200);
        assert!(response
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("Content-Type")
                && v == "application/x-git-upload-pack-advertisement"));
        assert!(response
            .body
            .starts_with(b"001e# service=git-upload-pack\n"));
    }

    /// git http-backend's own convention (found by hand): a missing
    /// repo is a `Status: 404` header, not a nonzero exit code -- this
    /// test is what pins that down as a real contract, not an
    /// assumption.
    #[test]
    fn info_refs_against_a_missing_repo_returns_404_not_an_error() {
        let temp = temp_project_root("missing-repo");

        let response = run_http_backend(
            temp.path(),
            &CgiRequest {
                method: "GET",
                path_info: "/never-provisioned.git/info/refs",
                query_string: "service=git-upload-pack",
                content_type: None,
                remote_user: "test-device",
                body: b"",
            },
        )
        .expect("run_http_backend");

        assert_eq!(response.status, 404);
    }
}
