//! Embedded Web Console (`web/dist` baked into the binary at compile time).

use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../web/dist"]
struct WebAssets;

/// Whether the Console was embedded (at least `index.html`).
pub fn has_console() -> bool {
    WebAssets::get("index.html").is_some()
}

/// Serve embedded SPA assets. Unknown paths fall back to `index.html` (client router).
pub async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(file) = WebAssets::get(path) {
        return asset_response(path, file.data.as_ref());
    }

    // SPA deep links: /agents/:id → index.html
    if !path.contains('.') {
        if let Some(file) = WebAssets::get("index.html") {
            return asset_response("index.html", file.data.as_ref());
        }
    }

    (
        StatusCode::NOT_FOUND,
        "Web Console asset not found. Rebuild with `cd web && npm run build` before cargo build.",
    )
        .into_response()
}

fn asset_response(path: &str, data: &[u8]) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CACHE_CONTROL, cache_control(path))
        .body(Body::from(data.to_vec()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn cache_control(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}
