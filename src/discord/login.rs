use super::auth::valid_token;
use axum::{
    extract::{DefaultBodyLimit, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::oneshot};
use zeroize::Zeroizing;

struct Handoff {
    port: u16,
    sender: parking_lot::Mutex<Option<oneshot::Sender<HandoffResult>>>,
}

type HandoffResult = Result<Credential, &'static str>;

struct Credential {
    token: Zeroizing<String>,
    fingerprint: Option<String>,
}

#[derive(Deserialize)]
struct Rpc {
    cmd: String,
    args: Args,
    nonce: String,
}

#[derive(Deserialize)]
struct Args {
    #[serde(rename = "handoffToken")]
    token: Option<String>,
    fingerprint: Option<String>,
}

#[derive(Deserialize)]
struct Parameters {
    v: u8,
    payload: Option<String>,
    callback: Option<String>,
}

// Discord's browser uses POST for Chrome and a navigation GET for other browsers.
/// Reports the sign-in link, then `None` once the browser hands off.
pub async fn login(events: impl Fn(Option<String>)) -> Result<Zeroizing<String>, String> {
    let mut listener = None;
    for port in 6463..=6472 {
        match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(bound) => {
                listener = Some(bound);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(_) => return Err("Could not open the browser handoff.".into()),
        }
    }
    let listener = listener
        .ok_or("Discord's local RPC ports are occupied. Close other Discord clients and retry.")?;
    let port = listener
        .local_addr()
        .map_err(|_| "Could not open the browser handoff.")?
        .port();
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let key = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    let (sender, receiver) = oneshot::channel();
    let state = Arc::new(Handoff {
        port,
        sender: parking_lot::Mutex::new(Some(sender)),
    });
    let router = Router::new()
        .route("/rpc", get(navigation).post(submit).options(preflight))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state);
    let redirect = format!("/handoff?rpc={port}&key={key}");
    let mut url = reqwest::Url::parse("https://discord.com/login").unwrap();
    // Discord's RPC module reads location.search once, before the SPA redirect.
    url.query_pairs_mut()
        .append_pair("rpc", &port.to_string())
        .append_pair("redirect_to", &redirect);
    events(Some(url.into()));
    tokio::time::timeout(Duration::from_secs(600), async {
        tokio::select! {
            result = async {
                let handoff = receiver.await.map_err(|_| "The browser handoff closed.")??;
                events(None);
                let http = super::http_client().map_err(|_| "Could not initialize the handoff exchange.")?;
                let mut request = http.post("https://discord.com/api/v9/auth/handoff/exchange")
                    .header("Origin", "https://discord.com")
                    .header("Referer", "https://discord.com/login")
                    .header("X-Super-Properties", STANDARD.encode(super::client_properties().to_string()))
                    .json(&json!({"key": key, "handoff_token": handoff.token.as_str()}));
                if let Some(fingerprint) = handoff.fingerprint {
                    request = request.header("X-Fingerprint", fingerprint);
                }
                let response = request.send().await.map_err(|_| "Could not reach Discord for the handoff exchange.")?;
                if !response.status().is_success() {
                    return Err(format!("Discord rejected the handoff exchange (HTTP {}).", response.status().as_u16()));
                }
                let mut value: Value = response.json().await.map_err(|_| "Discord returned an unreadable handoff result.")?;
                match value.get_mut("token").map(Value::take) {
                    Some(Value::String(token)) if valid_token(&token) => Ok(Zeroizing::new(token)),
                    _ => Err("Discord did not return a valid session from the handoff.".into()),
                }
            } => result,
            _ = axum::serve(listener, router) => Err("The local browser handoff server stopped.".into()),
        }
    }).await.map_err(|_| "Browser sign-in timed out. No handoff completed.".to_string())?
}

fn allowed(headers: &HeaderMap, state: &Handoff, post: bool) -> bool {
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
        == Some(format!("127.0.0.1:{}", state.port).as_str())
        && match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            Some(origin) => origin == "https://discord.com",
            None => !post,
        }
}

fn accept(state: &Handoff, rpc: Rpc) -> Result<Value, StatusCode> {
    if rpc.cmd != "BROWSER_HANDOFF"
        || rpc.nonce.len() > 128
        || rpc
            .args
            .fingerprint
            .as_ref()
            .is_some_and(|v| v.len() > 256 || !v.bytes().all(|b| b.is_ascii_graphic()))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let credential = match rpc.args.token {
        Some(token) => {
            let token = Zeroizing::new(token);
            if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(StatusCode::BAD_REQUEST);
            }
            Ok(Credential { token, fingerprint: rpc.args.fingerprint })
        }
        None => Err("Discord's browser did not supply a handoff credential. Sign in on discord.com and retry."),
    };
    state
        .sender
        .lock()
        .take()
        .ok_or(StatusCode::GONE)?
        .send(credential)
        .map_err(|_| StatusCode::GONE)?;
    Ok(json!({"cmd": "BROWSER_HANDOFF", "evt": null, "nonce": rpc.nonce, "data": null}))
}

async fn submit(
    State(state): State<Arc<Handoff>>,
    headers: HeaderMap,
    Query(params): Query<Parameters>,
    Json(rpc): Json<Rpc>,
) -> Response {
    if !allowed(&headers, &state, true) || params.v != 1 {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = match accept(&state, rpc) {
        Ok(value) => Json(value).into_response(),
        Err(status) => status.into_response(),
    };
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        "https://discord.com".parse().unwrap(),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        "true".parse().unwrap(),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

async fn navigation(
    State(state): State<Arc<Handoff>>,
    headers: HeaderMap,
    Query(params): Query<Parameters>,
) -> Response {
    if !allowed(&headers, &state, false)
        || params.v != 1
        || params.callback.as_deref() != Some("https://discord.com/handoff?done=true")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let rpc = match params
        .payload
        .as_deref()
        .filter(|p| p.len() <= 16 * 1024)
        .and_then(|p| serde_json::from_str(p).ok())
    {
        Some(rpc) => rpc,
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    match accept(&state, rpc) {
        Ok(_) => Redirect::to("https://discord.com/handoff?done=true").into_response(),
        Err(status) => status.into_response(),
    }
}

async fn preflight(State(state): State<Arc<Handoff>>, headers: HeaderMap) -> Response {
    if !allowed(&headers, &state, true) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let out = response.headers_mut();
    out.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        "https://discord.com".parse().unwrap(),
    );
    out.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        "true".parse().unwrap(),
    );
    out.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        "POST".parse().unwrap(),
    );
    out.insert(
        "access-control-allow-private-network",
        "true".parse().unwrap(),
    );
    if let Some(value) = headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        out.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, value.clone());
    }
    response
}
