use axum::{
    extract::Path,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../web/"]
struct WebAssets;

pub(crate) const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

pub(crate) async fn index() -> Response {
    asset("index.html")
}

pub(crate) async fn named(Path(name): Path<String>) -> Response {
    if name.contains(['/', '\\']) || name.starts_with('.') {
        return StatusCode::NOT_FOUND.into_response();
    }
    asset(&name)
}

fn asset(name: &str) -> Response {
    let Some(file) = WebAssets::get(name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = file.data.into_owned().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(file.metadata.mimetype())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response
}

pub(crate) async fn security_headers(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let csp = crate::auth::request_authority(req.headers(), req.uri())
        .and_then(|host| {
            HeaderValue::from_str(&CSP.replace(
                "connect-src 'self'",
                &format!("connect-src 'self' ws://{host} wss://{host}"),
            ))
            .ok()
        })
        .unwrap_or_else(|| HeaderValue::from_static(CSP));
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(csp);
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    response
}
