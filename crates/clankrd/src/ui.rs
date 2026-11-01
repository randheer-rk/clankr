//! Serves the web UI (`ui/`, built with `npm run build`) from the binary. Static files need no
//! auth; every API call the app makes goes through the normal bearer-token checks.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

#[derive(rust_embed::Embed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct Assets;

pub(crate) async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path == "v1" || path.starts_with("v1/") {
        return crate::HttpError(clankr_types::ApiError::new(clankr_types::ErrorCode::NotFound, format!("no API route {}", uri.path()))).into_response();
    }
    // hashed asset names never change content; the app shell must always be revalidated
    let (file, cache) = match Assets::get(path) {
        Some(f) if !path.is_empty() => (f, "public, max-age=31536000, immutable"),
        _ => match Assets::get("index.html") {
            Some(f) => (f, "no-cache"),
            None => return (StatusCode::NOT_FOUND, "web UI not built: run `npm ci && npm run build` in ui/, then rebuild clankrd").into_response(),
        },
    };
    let mime = file.metadata.mimetype().to_owned();
    ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache.to_owned())], file.data).into_response()
}
