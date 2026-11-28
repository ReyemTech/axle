//! `GET /install.sh`: the shell installer, with this server's address and version baked in.

use askama::Template;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::AppState;

/// The rendered installer. `server_url` is validated by `ServerConfig` (`AXL_PUBLIC_URL`) or
/// built from the bind address, so it is safe to splice inside single quotes.
#[derive(Template)]
#[template(path = "install.sh", escape = "none")]
pub struct InstallScript {
    pub server_url: String,
    pub version: &'static str,
}

/// `GET /install.sh`: 200 with the script, or a 503 naming `AXL_PUBLIC_URL` when the server
/// cannot say where clients should connect.
pub async fn install_sh(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let server_url = match state.config.advertised_url(host) {
        Ok(url) => url,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("{e}\n")).into_response(),
    };
    let script = InstallScript {
        server_url,
        version: env!("CARGO_PKG_VERSION"),
    };
    match script.render() {
        Ok(body) => (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/x-shellscript; charset=utf-8"),
            )],
            body,
        )
            .into_response(),
        Err(e) => super::ApiError::internal(e).into_response(),
    }
}
