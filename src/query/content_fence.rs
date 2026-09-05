//! Content fence — the query-side guarantee that a returned result always has
//! real code behind it.
//!
//! ## Why this exists
//! A resident vector shard and the `chunk` rows it points at are two separate
//! stores that are mutated at different instants during an index run:
//!
//! * full rebuild: `delete_all_data` clears `chunk` (store::ops), and the new
//!   shard is only published later, after the whole repo has been re-embedded.
//! * incremental: `delete_files_data_incremental` clears the affected files'
//!   `chunk` rows, and the shard delta is only applied after streaming finishes.
//!
//! In both windows a query can hit a shard whose vectors reference `chunk` rows
//! that no longer exist. `EmbeddingIdentity` does NOT catch this: the identity is
//! unchanged (same model), so the shard validates as a `Hit`. The DB lookup then
//! returns nothing and the pipeline happily emits a result block with an empty
//! body — a confident answer containing no code.
//!
//! This module is the fail-closed backstop: a candidate whose stored content did
//! not resolve is DROPPED, and a query whose candidates were *all* dropped is
//! reported as `warming` (retry) rather than as a genuine "no results". Dropping
//! is preferred over erroring so a partially re-indexed repo still returns the
//! chunks that ARE durable (the chosen "partial results, never empty" semantics).
//!
//! The logic is a pure function over already-fetched chunks so it is unit-tested
//! without a live DB, an embedding client, or a running index.

use regex::Regex;
use std::sync::LazyLock;

use crate::query::merger::MergeChunk;

/// Minimum number of non-whitespace characters a chunk's stored content must have
/// to count as resolved. A chunk row that is absent (or present but blank) yields
/// an empty string from `fetch_chunk_content`, which is indistinguishable from —
/// and just as useless as — a missing row, so both are fenced by the same rule.
pub const MIN_RESOLVED_CONTENT_CHARS: usize = 1;

/// Outcome of fencing a candidate set.
#[derive(Debug, Default)]
pub struct ContentFence {
    /// Candidates whose stored content resolved. Original order is preserved.
    pub kept: Vec<MergeChunk>,
    /// How many candidates were dropped because their content did not resolve.
    pub dropped: usize,
}

impl ContentFence {
    /// True when there were candidates but every one of them was dropped.
    ///
    /// This is the signal that the vector shard is out of sync with the `chunk`
    /// table (an index run is mid-flight), NOT that the repo has no match. The
    /// caller must translate it into `warming = true`.
    pub fn stale_shard_detected(&self) -> bool {
        self.kept.is_empty() && self.dropped > 0
    }
}

/// True if `content` failed to resolve to real stored code.
pub fn is_unresolved_content(content: &str) -> bool {
    content.trim().len() < MIN_RESOLVED_CONTENT_CHARS
}

/// Drop every candidate whose stored content did not resolve.
pub fn apply(chunks: Vec<MergeChunk>) -> ContentFence {
    let total = chunks.len();
    let kept: Vec<MergeChunk> = chunks
        .into_iter()
        .filter(|c| !is_unresolved_content(&c.content))
        .collect();
    ContentFence {
        dropped: total - kept.len(),
        kept,
    }
}

// ─── Secret redaction ───────────────────────────────────────────────────────
//
// The second fence: a returned chunk (or a reranker/agentic tool payload) must
// not carry credential material out of the engine — into the rerank LLM
// payload, MCP transcripts, or REST responses. Chunks are code, so the net is
// DELIBERATELY precision-first: vendor-shaped tokens are a hard guarantee,
// while the generic assignment rule only fires on a secret-shaped value
// (≥8 chars of `[A-Za-z0-9_/+=-]`, no spaces, quotes, or dots) so legitimate
// code passes through untouched (`password: String`, `api_key = config.api_key`,
// `process.env.API_KEY`). A missed secret is accepted over a mangled snippet;
// every hit is replaced with a visible, greppable `[REDACTED:<kind>]` marker
// that never re-matches (redaction is idempotent).

/// One redaction rule: every match of `re` is replaced by `replacement`, which
/// may reference capture groups (`$1`) to keep a trusted prefix.
struct SecretPattern {
    re: Regex,
    replacement: &'static str,
}

static SECRET_PATTERNS: LazyLock<Vec<SecretPattern>> = LazyLock::new(|| {
    vec![
        // Whole key blocks first so a multi-line key collapses to one marker.
        SecretPattern {
            re: Regex::new(
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY( BLOCK)?-----.*?-----END [A-Z0-9 ]*PRIVATE KEY( BLOCK)?-----",
            )
            .expect("valid private-key regex"),
            replacement: "[REDACTED:private-key]",
        },
        SecretPattern {
            re: Regex::new(r"\bsk-ant-[A-Za-z0-9_-]{16,}\b").expect("valid anthropic regex"),
            replacement: "[REDACTED:anthropic-key]",
        },
        SecretPattern {
            re: Regex::new(r"\bsk-(proj-)?[A-Za-z0-9_-]{20,}\b").expect("valid openai regex"),
            replacement: "[REDACTED:openai-key]",
        },
        SecretPattern {
            re: Regex::new(r"\b(gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{22,})\b")
                .expect("valid github regex"),
            replacement: "[REDACTED:github-token]",
        },
        SecretPattern {
            re: Regex::new(r"\bAIza[0-9A-Za-z_-]{35}\b").expect("valid google regex"),
            replacement: "[REDACTED:google-api-key]",
        },
        SecretPattern {
            re: Regex::new(r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b").expect("valid slack regex"),
            replacement: "[REDACTED:slack-token]",
        },
        SecretPattern {
            re: Regex::new(r"\bnpm_[A-Za-z0-9]{36}\b").expect("valid npm regex"),
            replacement: "[REDACTED:npm-token]",
        },
        SecretPattern {
            re: Regex::new(r"\b[sr]k_(live|test)_[A-Za-z0-9]{10,}\b").expect("valid stripe regex"),
            replacement: "[REDACTED:stripe-key]",
        },
        SecretPattern {
            re: Regex::new(r"\bAKIA[0-9A-Z]{16}\b").expect("valid aws regex"),
            replacement: "[REDACTED:aws-access-key]",
        },
        SecretPattern {
            re: Regex::new(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b")
                .expect("valid jwt regex"),
            replacement: "[REDACTED:jwt]",
        },
        // Keep the "Authorization: Bearer" prefix; drop the credential.
        SecretPattern {
            re: Regex::new(r#"(?i)\b(authorization\s*[:=]\s*(?:bearer|basic|token)\s+)[^\s'"]+"#)
                .expect("valid auth-header regex"),
            replacement: "$1[REDACTED:auth-header]",
        },
        // Keep scheme + username of an userinfo URL; drop the password. The
        // trailing `@` anchors the match — without it a `host:port` would be
        // treated as credentials — and the scheme is any protocol, so
        // postgres/mysql/redis DSNs are covered too.
        SecretPattern {
            re: Regex::new(r#"(?i)\b([a-z][a-z0-9+.-]*://[^\s'"/:@]+:)[^\s'"/:@]+@"#)
                .expect("valid url regex"),
            replacement: "$1[REDACTED:url-credentials]@",
        },
        // Generic secret assignment — LAST, and only secret-shaped values.
        // The identifier prefix before the key is preserved (`db_password`
        // keeps its name), and the value must contain a digit (or be ≥16
        // chars of the token charset) — dots and colons are excluded, so
        // `x = process.env.API_KEY` and `token = serde_json::Value` never
        // match.
        SecretPattern {
            re: Regex::new(
                r#"(?i)\b([A-Za-z0-9_]*)(client_?secret|access_?token|refresh_?token|auth_?token|api_?key|private_?key|pass(word|wd)?|pwd|secret|token)\b(\s*[:=]\s*)("(?:[A-Za-z0-9_\-/+=]{0,60}[0-9][A-Za-z0-9_\-/+=]{0,60}|[A-Za-z0-9_\-/+=]{16,})"|'(?:[A-Za-z0-9_\-/+=]{0,60}[0-9][A-Za-z0-9_\-/+=]{0,60}|[A-Za-z0-9_\-/+=]{16,})'|[A-Za-z0-9_\-/+=]{0,60}[0-9][A-Za-z0-9_\-/+=]{0,60}|[A-Za-z0-9_\-/+=]{16,})"#,
            )
            .expect("valid secret-assignment regex"),
            replacement: "$1$2$3$4[REDACTED:secret-assignment]",
        },
    ]
});

/// Redact credential-shaped material from `content` (see the section above for
/// the precision-first policy and the marker format).
pub fn redact_secrets(content: &str) -> String {
    let mut out = content.to_owned();
    for pattern in SECRET_PATTERNS.iter() {
        out = pattern
            .re
            .replace_all(&out, pattern.replacement)
            .into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(file: &str, content: &str) -> MergeChunk {
        MergeChunk {
            file: file.to_owned(),
            line_start: 1,
            line_end: 2,
            score: 1.0,
            content: content.to_owned(),
            symbol: None,
            symbol_fqn: None,
            symbol_kind: None,
        }
    }

    #[test]
    fn drops_unresolved_and_keeps_resolved_in_order() {
        let fence = apply(vec![
            chunk("a.rs", "fn a() {}"),
            chunk("gone.rs", ""),
            chunk("b.rs", "fn b() {}"),
        ]);
        assert_eq!(fence.dropped, 1, "the missing chunk row must be dropped");
        assert_eq!(
            fence
                .kept
                .iter()
                .map(|c| c.file.as_str())
                .collect::<Vec<_>>(),
            vec!["a.rs", "b.rs"],
            "surviving candidates keep their original order"
        );
        assert!(
            !fence.stale_shard_detected(),
            "a partial drop is partial results, not a stale shard"
        );
    }

    /// Whitespace-only content is as useless as an absent row — same fence.
    #[test]
    fn whitespace_only_content_is_unresolved() {
        assert!(is_unresolved_content(""));
        assert!(is_unresolved_content("   \n\t "));
        assert!(!is_unresolved_content("x"));
    }

    /// Every candidate dropped => the shard is ahead of the chunk table.
    #[test]
    fn all_dropped_is_reported_as_stale_shard() {
        let fence = apply(vec![chunk("gone1.rs", ""), chunk("gone2.rs", "")]);
        assert_eq!(fence.dropped, 2);
        assert!(fence.kept.is_empty());
        assert!(
            fence.stale_shard_detected(),
            "all-dropped must be a retryable warming signal, never 'no results'"
        );
    }

    /// A genuinely empty candidate set is NOT a stale shard: the search simply
    /// matched nothing, which must keep reporting a real empty.
    #[test]
    fn no_candidates_is_not_stale_shard() {
        let fence = apply(vec![]);
        assert_eq!(fence.dropped, 0);
        assert!(
            !fence.stale_shard_detected(),
            "empty input must stay a genuine empty, not a warming retry"
        );
    }

    // ── Secret redaction ────────────────────────────────────────────────

    #[test]
    fn vendor_tokens_redacted_with_kind_marker() {
        let cases = vec![
            (concat!("ghp_", "0123456789abcdefghij0123456789abcd"), "github-token"),
            (
                concat!("github_pat_", "11AAAAAAA0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
                "github-token",
            ),
            (concat!("sk-ant-api03-", "0123456789abcdefghij"), "anthropic-key"),
            (concat!("sk-proj-", "0123456789abcdefghij"), "openai-key"),
            (concat!("sk-", "0123456789abcdefghijklmnopqrst"), "openai-key"),
            (concat!("AIzaSy", "A1234567890abcdefghijklmnopqrstuv"), "google-api-key"),
            (concat!("xoxb-123456789012-", "abcdefghijklmn"), "slack-token"),
            (concat!("npm_", "0123456789abcdefghijklmnopqrstuvwxyz"), "npm-token"),
            (concat!("sk_live_", "0123456789abcd"), "stripe-key"),
            (concat!("AKIA", "IOSFODNN7EXAMPLE"), "aws-access-key"),
        ];
        for (secret, kind) in cases {
            let out = redact_secrets(&format!("token = {secret};"));
            let want = format!("[REDACTED:{kind}]");
            assert!(out.contains(&want), "{secret} must redact as {kind}: {out}");
            assert!(!out.contains(&secret[..12]), "{out}");
        }
    }

    #[test]
    fn jwt_redacted() {
        let jwt = concat!(
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            ".",
            "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
            ".",
            "dozjgNryP4J3jVmNHl0w5N0XgL0n3I9PlFUP0THsR8U"
        );
        let out = redact_secrets(&format!("Authorization: Bearer {jwt}"));
        // Both the JWT rule and the auth-header rule apply; no raw segment remains.
        assert!(!out.contains("eyJhbGciOiJIUzI1NiIs"), "{out}");
        assert!(out.contains("[REDACTED:"), "{out}");
    }

    #[test]
    fn private_key_block_redacted_across_lines() {
        let pem = concat!(
            "-----BEGIN ",
            "RSA PRIVATE KEY-----\n",
            "MIIEowIBAAKCAQEA0Z3VS5JJcds3xfn/yG\n",
            "WybXADF [\nmore base64]\n",
            "-----END ",
            "RSA PRIVATE KEY-----\n",
            "fn real_code() {}"
        );
        let out = redact_secrets(pem);
        assert!(out.contains("[REDACTED:private-key]"), "{out}");
        assert!(!out.contains("MIIEowIBAAKCAQEA"), "{out}");
        assert!(out.contains("fn real_code() {}"), "trailing code survives");
    }

    #[test]
    fn auth_header_and_url_credentials_keep_trusted_prefix() {
        let out = redact_secrets("Authorization: Bearer abc123def456");
        assert_eq!(out, "Authorization: Bearer [REDACTED:auth-header]");

        let out = redact_secrets("postgres://admin:s3cr3t_Pass@db.internal:5432/app");
        assert_eq!(
            out,
            "postgres://admin:[REDACTED:url-credentials]@db.internal:5432/app"
        );
    }

    #[test]
    fn jdbc_and_port_urls_are_not_url_credentials() {
        // host:port has no `@` — must not be treated as userinfo credentials.
        let src = "https://example.com:8080/path and redis://cache:6379";
        assert_eq!(redact_secrets(src), src);
    }

    #[test]
    fn secret_assignment_redacts_only_the_value() {
        let cases = [
            r#"API_KEY = "aBcDeFgH123456""#,
            "db_password=hunter2hunter2",
            "client_secret: '9f8e7d6c5b4a'",
            "ACCESS_TOKEN=abcdef1234567890",
        ];
        for src in cases {
            let out = redact_secrets(src);
            assert!(
                out.ends_with("[REDACTED:secret-assignment]"),
                "{src} -> {out}"
            );
        }
    }

    #[test]
    fn legitimate_code_is_not_mangled() {
        let untouched = [
            "pub struct Config { pub password: String }",
            "let api_key = config.api_key;",
            "API_KEY = process.env.API_KEY",
            "fn generate_api_key() -> String {",
            "token_count = 42",
            "user_password.get_or_insert_default()",
            "https://example.com:8080/path",
            "// commit abcdef0123456789 fixed the parser",
        ];
        for src in untouched {
            assert_eq!(redact_secrets(src), src, "must not touch: {src}");
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        let token = concat!("ghp_", "0123456789abcdefghij0123456789abcd");
        let src = format!(r#"password="hunter2hunter2" and {token}"#);
        let once = redact_secrets(&src);
        assert_eq!(redact_secrets(&once), once, "markers must not re-match");
    }
}
