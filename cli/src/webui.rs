use anyhow::Result;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use std::net::SocketAddr;
use std::path::Path;
use tower_http::services::{ServeDir, ServeFile};

/// Runs a small public-facing server that serves the built webui
/// (`webui_dir`, e.g. `<workspace>/webui/dist`) as static files and
/// reverse-proxies everything under `/api/` to the kernel gateway, which is
/// bound separately (see `main.rs::run` — the gateway itself never binds
/// the public port directly, same "kernel doesn't know webui exists"
/// separation `kernel::gateway::GatewayConfig`'s own doc comment describes;
/// this is the "separate wrapper" it says isn't built yet).
///
/// Lets the webui keep using plain same-origin `fetch('/api/...')` calls
/// (see `webui/src/api.js`) without any CORS setup — from the browser's
/// perspective there's only ever the one public port.
pub async fn serve(webui_dir: std::path::PathBuf, api_addr: SocketAddr, port: u16) -> Result<()> {
    let index = webui_dir.join("index.html");
    let static_service = ServeDir::new(&webui_dir).not_found_service(ServeFile::new(index));

    let app = Router::new()
        .route("/api/{*path}", any(proxy_api))
        .fallback_service(static_service)
        .with_state(ProxyState {
            api_addr,
            // no `.timeout(...)` call — reqwest's async client has none by
            // default. That matters here specifically: `/api/message`
            // blocks for the agent's entire run (kernel/src/gateway.rs
            // `run_trigger`), which for a multi-turn `ssh_exec` chain can
            // genuinely take minutes — same reasoning as
            // webui/vite.config.js's dev-proxy `timeout: 0` for the same
            // endpoint, just on the production-serving side of it instead
            client: reqwest::Client::new(),
        });

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    println!("[ebinactl] webui: http://0.0.0.0:{port} (serving {})", webui_dir.display());
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Clone)]
struct ProxyState {
    api_addr: SocketAddr,
    client: reqwest::Client,
}

/// Forwards method/path/query/headers/body to the real gateway on
/// `api_addr` and streams the response straight back — streaming (not
/// buffering the response) matters here specifically because `/api/thinking`
/// and `/api/logs` are long-lived SSE streams, not one-shot responses.
async fn proxy_api(State(state): State<ProxyState>, req: Request) -> Response {
    let path_and_query = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/api");
    let url = format!("http://{}{}", state.api_addr, path_and_query);
    let method = req.method().clone();
    let mut headers = req.headers().clone();
    // reqwest sets its own Host for the upstream request — forwarding the
    // original one would tell the gateway it's being addressed as the
    // public host/port, which it isn't
    headers.remove(axum::http::header::HOST);

    let body = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return (axum::http::StatusCode::BAD_REQUEST, format!("reading request body: {e}")).into_response(),
    };

    let upstream = state.client.request(method, &url).headers(headers).body(body).send().await;
    let upstream = match upstream {
        Ok(r) => r,
        Err(e) => return (axum::http::StatusCode::BAD_GATEWAY, format!("gateway unreachable: {e}")).into_response(),
    };

    let status = upstream.status();
    let mut response_headers = upstream.headers().clone();
    let stream = upstream.bytes_stream();
    let mut response = Response::builder().status(status).body(axum::body::Body::from_stream(stream)).unwrap();
    std::mem::swap(response.headers_mut(), &mut response_headers);
    response
}

pub fn looks_like_a_build(dir: &Path) -> bool {
    dir.join("index.html").is_file()
}
