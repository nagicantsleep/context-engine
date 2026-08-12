//! Embedded UI assets (single-page HTML + Spec ADE self-hosted fonts).
//!
//! The admin UI is one `index.html` file plus a handful of woff2 files. Both the
//! standalone server and the router front-end serve them via the same handlers
//! so the binary stays self-contained (no runtime filesystem dependency on the
//! source tree) and the two entry points stay byte-identical.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;

/// Spec ADE sans + mono faces shipped under `assets/fonts/`. Filenames are the
/// public URL segment (`/assets/fonts/<name>`) — keep them stable.
const FONTS: &[(&str, &[u8], &str)] = &[
    (
        "IBMPlexSans-latin-400.woff2",
        include_bytes!("fonts/IBMPlexSans-latin-400.woff2"),
        "font/woff2",
    ),
    (
        "IBMPlexSans-latin-400-italic.woff2",
        include_bytes!("fonts/IBMPlexSans-latin-400-italic.woff2"),
        "font/woff2",
    ),
    (
        "IBMPlexSans-latin-500.woff2",
        include_bytes!("fonts/IBMPlexSans-latin-500.woff2"),
        "font/woff2",
    ),
    (
        "IBMPlexSans-latin-600.woff2",
        include_bytes!("fonts/IBMPlexSans-latin-600.woff2"),
        "font/woff2",
    ),
    (
        "IBMPlexSans-latin-700.woff2",
        include_bytes!("fonts/IBMPlexSans-latin-700.woff2"),
        "font/woff2",
    ),
    (
        "IBMPlexSans-vietnamese-400.woff2",
        include_bytes!("fonts/IBMPlexSans-vietnamese-400.woff2"),
        "font/woff2",
    ),
    (
        "IBMPlexSans-vietnamese-500.woff2",
        include_bytes!("fonts/IBMPlexSans-vietnamese-500.woff2"),
        "font/woff2",
    ),
    (
        "IBMPlexSans-vietnamese-600.woff2",
        include_bytes!("fonts/IBMPlexSans-vietnamese-600.woff2"),
        "font/woff2",
    ),
    (
        "IBMPlexSans-vietnamese-700.woff2",
        include_bytes!("fonts/IBMPlexSans-vietnamese-700.woff2"),
        "font/woff2",
    ),
    (
        "Lilex-latin-400.woff2",
        include_bytes!("fonts/Lilex-latin-400.woff2"),
        "font/woff2",
    ),
    (
        "Lilex-latin-500.woff2",
        include_bytes!("fonts/Lilex-latin-500.woff2"),
        "font/woff2",
    ),
    (
        "Lilex-latin-600.woff2",
        include_bytes!("fonts/Lilex-latin-600.woff2"),
        "font/woff2",
    ),
    (
        "Lilex-latin-700.woff2",
        include_bytes!("fonts/Lilex-latin-700.woff2"),
        "font/woff2",
    ),
];

/// Serve the single-page admin UI (compile-time embedded).
pub async fn serve_index() -> impl IntoResponse {
    let html = include_str!("index.html");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    (headers, html)
}

/// Serve a self-hosted ADE font by exact filename. Unknown names → 404 (no
/// path traversal: the match is against a fixed allow-list, never the FS).
pub async fn serve_font(
    axum::extract::Path(name): axum::extract::Path<String>,
) -> impl IntoResponse {
    match FONTS.iter().find(|(n, _, _)| *n == name.as_str()) {
        Some((_, bytes, mime)) => {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
            // Fonts are content-addressed by filename in the binary; long cache
            // is safe and avoids a round-trip on every reload of the SPA.
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            );
            (StatusCode::OK, headers, *bytes).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::FONTS;

    #[test]
    fn every_font_is_real_woff2() {
        // wOFF2 magic is ASCII "wOF2" (0x77 0x4f 0x46 0x32).
        for (name, bytes, mime) in FONTS {
            assert_eq!(*mime, "font/woff2", "{name}");
            assert!(bytes.len() > 100, "{name} too small");
            assert_eq!(&bytes[..4], b"wOF2", "{name} bad magic");
        }
    }

    #[test]
    fn font_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (name, _, _) in FONTS {
            assert!(seen.insert(*name), "duplicate font name: {name}");
        }
    }
}
