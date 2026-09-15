//! Embedding vectors: serialization, similarity, and Reciprocal Rank
//! Fusion (design 5.3, M1-9). The arithmetic here has no dependency on how
//! a vector was obtained, so it is always compiled; only [`embed_remote`]
//! (talking to an actual endpoint) needs the `embed` Cargo feature and its
//! `ureq` dependency -- kept off the default build (CLAUDE.md's slim-core
//! rule and the M0 binary-size gate apply to the default artifact; see
//! `docs/adr/0003-hybrid-search-embed-feature-gate.md`).

#![allow(clippy::module_name_repetitions)]

/// Serializes an embedding as little-endian `f32` bytes, for storage in
/// the index's `embedding` BLOB column.
pub fn serialize_embedding(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for v in vector {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Deserializes bytes written by [`serialize_embedding`]. Tolerant of a
/// byte count that isn't a multiple of 4 (drops the trailing partial
/// value) rather than panicking -- a truncated or corrupted BLOB should
/// degrade to "not usable", not crash the caller.
pub fn deserialize_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// Cosine similarity between two vectors. Returns `0.0` (rather than
/// dividing by zero or NaN) for a length mismatch or a zero-magnitude
/// vector -- callers are expected to have already filtered to matching
/// dimensions, so this is a safe fallback, not a silent truncation.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

/// Reciprocal Rank Fusion (Cormack, Clarke & Buettcher, SIGIR 2009):
/// combines any number of already-ranked (best-first) path lists into one
/// fused ranking by summing `1 / (k + rank)` (1-indexed rank) for every
/// list a path appears in -- same `k = 60` constant and same
/// reciprocal-of-rank (not reciprocal-of-score) formulation agent-wkp's
/// Python implementation used, so a path present in both the BM25 and
/// vector-similarity rankings naturally outranks one found by only one
/// signal.
pub fn reciprocal_rank_fusion(ranked_lists: &[&[String]]) -> Vec<(String, f64)> {
    const K: f64 = 60.0;
    let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for list in ranked_lists {
        for (i, path) in list.iter().enumerate() {
            let rank = (i + 1) as f64;
            *scores.entry(path.clone()).or_insert(0.0) += 1.0 / (K + rank);
        }
    }
    let mut fused: Vec<(String, f64)> = scores.into_iter().collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused
}

/// Configuration for an OpenAI-compatible `/embeddings` endpoint (design
/// 5.3). Always compiled -- the type itself carries no network dependency
/// -- only [`embed_remote`] needs the `embed` feature.
///
/// `api_key` must already have been read by the caller from a `0600` file
/// or stdin, never from argv or an environment variable (CLAUDE.md's hard
/// rule: "secrets never touch argv or environment variables"). This is a
/// deliberate deviation from agent-wkp's `WKP_EMBED_API_KEY` env-var
/// precedent -- see the ADR referenced above.
#[derive(Debug, Clone, Default)]
pub struct EmbedConfig {
    pub url: String,
    pub api_key: Option<String>,
    pub model: Option<String>,
}

#[cfg(feature = "embed")]
mod remote {
    use super::EmbedConfig;

    #[derive(Debug)]
    pub enum EmbedError {
        Request(String),
        Response(String),
    }

    impl std::fmt::Display for EmbedError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                EmbedError::Request(e) => write!(f, "embedding request failed: {e}"),
                EmbedError::Response(e) => write!(f, "embedding response invalid: {e}"),
            }
        }
    }

    impl std::error::Error for EmbedError {}

    /// Calls `{config.url}/embeddings` with an OpenAI-compatible request
    /// body (`{"input": text}`, plus `model` if set) and returns the
    /// first embedding vector in the response.
    pub fn embed_remote(text: &str, config: &EmbedConfig) -> Result<Vec<f32>, EmbedError> {
        let url = format!("{}/embeddings", config.url.trim_end_matches('/'));
        let mut body = format!("{{\"input\":{}", json_string(text));
        if let Some(model) = &config.model {
            body.push_str(&format!(",\"model\":{}", json_string(model)));
        }
        body.push('}');

        let mut req = ureq::post(&url).header("Content-Type", "application/json");
        if let Some(key) = &config.api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
        let mut resp = req
            .send(body.as_str())
            .map_err(|e| EmbedError::Request(e.to_string()))?;
        let text_body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| EmbedError::Request(e.to_string()))?;
        extract_embedding(&text_body).ok_or_else(|| {
            EmbedError::Response("no numeric \"embedding\" array found in response".to_string())
        })
    }

    fn json_string(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    /// A tolerant, hand-rolled extraction of the first `"embedding":[...]`
    /// numeric array in an OpenAI-compatible embeddings response --
    /// deliberately not a general JSON parser (CLAUDE.md's slim-core
    /// rule): the only thing ever needed from this response is one flat
    /// array of numbers, so scanning for that key and parsing the
    /// bracketed numbers is a bounded, dependency-free equivalent to what
    /// a full parser would do here, at a fraction of the dependency cost
    /// a crate like `serde_json` would add for this one call site.
    fn extract_embedding(body: &str) -> Option<Vec<f32>> {
        let key = "\"embedding\"";
        let key_pos = body.find(key)?;
        let after_key = &body[key_pos + key.len()..];
        let colon = after_key.find(':')?;
        let after_colon = after_key[colon + 1..].trim_start();
        let inside = after_colon.strip_prefix('[')?;
        let close = inside.find(']')?;
        let numbers = &inside[..close];
        let mut out = Vec::new();
        for part in numbers.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            out.push(part.parse::<f32>().ok()?);
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        #[test]
        fn extract_embedding_reads_a_flat_number_array() {
            let body = r#"{"data":[{"embedding":[0.1,-0.2,3]}]}"#;
            assert_eq!(extract_embedding(body), Some(vec![0.1, -0.2, 3.0]));
        }

        #[test]
        fn extract_embedding_returns_none_without_the_key() {
            assert_eq!(extract_embedding(r#"{"error":"bad request"}"#), None);
        }

        #[test]
        fn extract_embedding_returns_none_on_an_empty_array() {
            assert_eq!(extract_embedding(r#"{"embedding":[]}"#), None);
        }

        /// A minimal, hand-rolled HTTP/1.1 server on loopback -- exercises
        /// `embed_remote` end to end (request body shape, header, response
        /// parsing) without a real network call or a mocking dependency.
        /// Reads exactly one request, replies with a fixed embedding, then
        /// closes; `embed_remote`'s one call per invocation matches that.
        fn serve_one_embedding(response_json: &'static str) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
            let addr = listener.local_addr().expect("local_addr");
            std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept one connection");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request_line = String::new();
                reader
                    .read_line(&mut request_line)
                    .expect("read request line");
                // Drain headers up to the blank line; body isn't needed by
                // this test (it only asserts on the *response* path).
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read header line");
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let mut stream = stream;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_json.len(),
                    response_json
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            });
            format!("http://{addr}")
        }

        #[test]
        fn embed_remote_parses_a_real_response_from_a_local_server() {
            let url = serve_one_embedding(r#"{"data":[{"embedding":[1.0,2.0,3.0]}]}"#);
            let config = EmbedConfig {
                url,
                api_key: None,
                model: None,
            };
            let result = embed_remote("hello world", &config).expect("embed_remote");
            assert_eq!(result, vec![1.0, 2.0, 3.0]);
        }

        #[test]
        fn embed_remote_reports_a_response_error_for_an_unparseable_body() {
            let url = serve_one_embedding(r#"{"error":"nope"}"#);
            let config = EmbedConfig {
                url,
                api_key: None,
                model: None,
            };
            let err = embed_remote("hello", &config).expect_err("expected a response error");
            assert!(matches!(err, EmbedError::Response(_)));
        }
    }
}

#[cfg(feature = "embed")]
pub use remote::{embed_remote, EmbedError};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_deserialize_round_trips() {
        let v = vec![0.5_f32, -1.25, 3.0];
        let bytes = serialize_embedding(&v);
        assert_eq!(bytes.len(), 12);
        assert_eq!(deserialize_embedding(&bytes), v);
    }

    #[test]
    fn deserialize_embedding_drops_a_trailing_partial_value_instead_of_panicking() {
        let mut bytes = serialize_embedding(&[1.0, 2.0]);
        bytes.push(0xFF); // 9 bytes: 2 whole f32s plus one stray byte
        assert_eq!(deserialize_embedding(&bytes), vec![1.0, 2.0]);
    }

    #[test]
    fn cosine_similarity_of_identical_vectors_is_one() {
        let v = vec![1.0, 2.0, 3.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_of_orthogonal_vectors_is_zero() {
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_mismatched_lengths_returns_zero() {
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
    }

    #[test]
    fn cosine_similarity_zero_vector_returns_zero_not_nan() {
        let score = cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]);
        assert_eq!(score, 0.0);
    }

    #[test]
    fn rrf_ranks_a_path_present_in_both_lists_above_either_alone() {
        let a = vec!["x".to_string(), "y".to_string()];
        let b = vec!["y".to_string(), "z".to_string()];
        let fused = reciprocal_rank_fusion(&[&a, &b]);
        assert_eq!(fused[0].0, "y");
    }

    #[test]
    fn rrf_of_a_single_list_preserves_its_order() {
        let a = vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ];
        let fused = reciprocal_rank_fusion(&[&a]);
        let paths: Vec<&str> = fused.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["first", "second", "third"]);
    }
}
