// Router-native `GET /api/cross-repo/chunk` handler.
//
// Workers' query-time BFS expansion (`query::cross_repo::CrossRepoResolver`)
// calls this endpoint when a call-graph edge points into a repo whose chunk
// content is NOT local to the calling worker. The router resolves WHICH repo
// owns the requested file (prefix match over configured repos), then proxies
// to that repo's worker `GET /api/graph-chunk` through the standard
// spawn-on-miss machinery — so a cold callee repo pays one bounded worker
// spawn, exactly like any other action request.
//
// Failure semantics mirror the resolver contract: unknown owner → 404;
// spawn/worker failure → the proxy layer's warming/error JSON. The worker
// treats every non-2xx as "endpoint absent" and drops that expansion subtree.

use serde::Deserialize;
// NOTE: this file is include!()-d into `router::mod`, so it shares that
// module's imports: State/Json/StatusCode/Response/json/ensure_dir_and_load/
// normalize_repo_path/acquire_and_proxy. Query arrives under the module's
// `AxumQuery` alias.

#[derive(Debug, Deserialize)]
pub struct CrossRepoChunkQuery {
    pub fqn: String,
    pub file: String,
}


/// GET /api/cross-repo/chunk?fqn=&file=
pub async fn cross_repo_chunk(
    State(state): State<RouterState>,
    AxumQuery(q): AxumQuery<CrossRepoChunkQuery>,
) -> Response {
    let settings = match ensure_dir_and_load(&state.home_dir) {
        Ok(s) => s,
        Err(e) => {
            let body = json!({ "error": format!("failed to load settings: {e}") });
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response();
        }
    };
    // Ownership = prefix match over CONFIGURED repos (the same rule workers
    // apply on their side as a backstop). Longest-prefix wins implicitly: repos
    // are distinct roots, and `path_in_repo` requires a real path-component
    // boundary, so two repos cannot both own one file unless nested — in which
    // case the deeper root must be listed first is NOT guaranteed, so pick the
    // LONGEST matching root explicitly for determinism.
    let mut owner: Option<String> = None;
    for repo in &settings.repos {
        if crate::path_in_repo(&q.file, repo)
            && owner
                .as_ref()
                .is_none_or(|best| normalize_repo_path(repo).len() > best.len())
        {
            owner = Some(normalize_repo_path(repo));
        }
    }
    let Some(owner) = owner else {
        let body = json!({ "error": "no configured repository owns the requested file" });
        return (StatusCode::NOT_FOUND, Json(body)).into_response();
    };

    let uri = format!(
        "/api/graph-chunk?fqn={}&file={}",
        percent_encode(&q.fqn),
        percent_encode(&q.file),
    );
    let req = axum::http::Request::builder()
        .method(axum::http::Method::GET)
        .uri(uri)
        .body(axum::body::Body::empty())
        .unwrap_or_else(|_| unreachable!("static method+uri always build"));
    acquire_and_proxy(&state.proxy, &owner, req).await
}

/// Minimal percent-encoding for query values: everything outside the RFC 3986
/// unreserved set (plus '/') becomes `%XX`. Windows drive-colon paths and
/// spaces are the common cases; correctness beats brevity here.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            // Unreserved per RFC 3986, PLUS '/' — absolute paths are the
            // dominant payload and stay readable; axum's Query extractor
            // decodes either form identically.
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}
mod tests {
    use super::percent_encode;

    #[test]
    fn encodes_paths_and_keeps_unreserved() {
        assert_eq!(percent_encode("/repo/b/b.rs"), "/repo/b/b.rs");
        assert_eq!(percent_encode(r"C:\repo\b"), r"C%3A%5Crepo%5Cb");
        assert_eq!(percent_encode("a b~c-d.e_f"), "a%20b~c-d.e_f");
    }
}
