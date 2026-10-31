//! The local HTTP server: request guard, page session, assets and API routes.
use crate::{
    discord::{
        cache::{Access, CacheStore, VoiceUser},
        models::Snapshot,
    },
    files::{self, Uploads},
    render::Message,
    requests,
    session::Session,
    voice,
    worker::{self, Request},
};
use axum::{
    extract::{self, DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event as ServerEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::mpsc;

pub struct App {
    pub data: PathBuf,
    pub cache: CacheStore,
    pub session: Arc<Session>,
    /// Account and media commands for the worker.
    pub commands: mpsc::Sender<Request>,
    pub hosts: [String; 2],
    pub secret: String,
    pub cookie: String,
    pub uploads: Arc<Uploads>,
}

/// The account the page acts for. Requests from a page showing another account are rejected.
pub fn page_account(headers: &HeaderMap) -> &str {
    headers
        .get("x-suncord-account")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
}

pub fn signed_in(app: &App, headers: &HeaderMap) -> bool {
    app.cache
        .user_id()
        .is_some_and(|user| user == page_account(headers))
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/session", post(session))
        .route("/api/state", get(client_state))
        .route("/api/events", get(client_events))
        .route("/api/ui", post(ui))
        .route("/api/action", post(action))
        .route("/api/media", get(voice::media))
        .route("/api/snapshot", get(snapshot))
        .route("/api/messages/{channel}", get(messages))
        .route("/api/files", post(files::upload))
        .route("/api/files/{id}", delete(files::remove_upload))
        .fallback_service(get(asset))
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

fn hash(content: impl std::hash::Hash) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    content.hash(&mut hasher);
    std::hash::Hasher::finish(&hasher)
}

fn content_tag(content: impl std::hash::Hash) -> String {
    format!("\"{:016x}\"", hash(content))
}

const JS: &str = "text/javascript; charset=utf-8";
const SVG: &str = "image/svg+xml";
const EMOJI_CSS: &[u8] = include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/emoji.css"));
const EMOJI_ASSETS: &[(&str, &str, &[u8])] =
    include!(concat!(env!("SUNCORD_ASSETS"), "/emoji-assets.rs"));
/// Embedded files by path, with their content type.
#[rustfmt::skip]
const ASSETS: &[(&str, &str, &[u8])] = &[
    ("/sounds/user_join.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/user_join.mp3"))),
    ("/sounds/user_leave.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/user_leave.mp3"))),
    ("/sounds/disconnect.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/disconnect.mp3"))),
    ("/sounds/mute.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/mute.mp3"))),
    ("/sounds/unmute.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/unmute.mp3"))),
    ("/sounds/deafen.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/deafen.mp3"))),
    ("/sounds/undeafen.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/undeafen.mp3"))),
    ("/sounds/stream_started.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/stream_started.mp3"))),
    ("/sounds/stream_ended.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/stream_ended.mp3"))),
    ("/sounds/stream_user_joined.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/stream_user_joined.mp3"))),
    ("/sounds/stream_user_left.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/stream_user_left.mp3"))),
    ("/sounds/call_ringing.mp3", "audio/mpeg", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/sounds/call_ringing.mp3"))),
    ("/", "text/html; charset=utf-8", include_bytes!("../ui/index.html")),
    ("/icons.svg", SVG, include_bytes!("../ui/icons.svg")),
    ("/app.css", "text/css; charset=utf-8", include_bytes!("../ui/app.css")),
    ("/playback.js", JS, include_bytes!("../ui/playback.js")),
    ("/core.js", JS, include_bytes!("../ui/core.js")),
    ("/composer.js", JS, include_bytes!("../ui/composer.js")),
    ("/timeline.js", JS, include_bytes!("../ui/timeline.js")),
    ("/message.js", JS, include_bytes!("../ui/message.js")),
    ("/sidebar.js", JS, include_bytes!("../ui/sidebar.js")),
    ("/panels.js", JS, include_bytes!("../ui/panels.js")),
    ("/profile.js", JS, include_bytes!("../ui/profile.js")),
    ("/picker.js", JS, include_bytes!("../ui/picker.js")),
    ("/accounts.js", JS, include_bytes!("../ui/accounts.js")),
    ("/voice.js", JS, include_bytes!("../ui/voice.js")),
    ("/app.js", JS, include_bytes!("../ui/app.js")),
    ("/emojis.tsv", "text/tab-separated-values; charset=utf-8", include_bytes!("../assets/emoji/emojis.tsv")),
    ("/fonts/Inter.woff2", "font/woff2", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/fonts/Inter.woff2"))),
    ("/fonts/JetBrainsMono.woff2", "font/woff2", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/fonts/JetBrainsMono.woff2"))),
];

/// The generated CSS includes font hashes, so checking the build tag never reads bitmap data.
async fn asset(uri: axum::http::Uri, headers: HeaderMap) -> Response {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let Some((_, mime, bytes)) = ASSETS
        .iter()
        .chain(EMOJI_ASSETS)
        .find(|(path, _, _)| *path == uri.path())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let tag = TAG.get_or_init(|| content_tag((ASSETS, EMOJI_CSS)));
    let cache = [
        (header::CACHE_CONTROL, "no-cache"),
        (header::ETAG, tag.as_str()),
    ];
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == tag.as_bytes())
    {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    (cache, [(header::CONTENT_TYPE, *mime)], *bytes).into_response()
}

async fn guard(State(app): State<Arc<App>>, request: extract::Request, next: Next) -> Response {
    let headers = request.headers();
    // Checking the host blocks DNS rebinding. The origin must match it.
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|host| app.hosts.iter().any(|known| known == host))
    else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let origin = format!("http://{host}");
    if headers
        .get(header::ORIGIN)
        .is_some_and(|h| h.as_bytes() != origin.as_bytes())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let api = request.uri().path().starts_with("/api/");
    // Other localhost ports share the cookie's site, but must not own playback.
    if api
        && headers
            .get("sec-fetch-site")
            .is_some_and(|site| site != "same-origin")
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let cookie = format!("{}={}", app.cookie, app.secret);
    if api
        && request.uri().path() != "/api/session"
        && !headers
            .get(header::COOKIE)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|cookies| cookies.split(';').any(|value| value.trim() == cookie))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if request.method() != Method::GET
        && headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()) != Some(origin.as_str())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; font-src 'self'; img-src 'self' blob: data: https:; media-src 'self' blob: https:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"));
    response
}

/// Keeps browser sessions valid across restarts. The file is private to the user.
pub fn saved_secret(data: &std::path::Path) -> Result<String, String> {
    let path = data.join("browser-secret");
    if let Ok(secret) = std::fs::read_to_string(&path) {
        if secret.len() == 64 && secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(secret);
        }
    }
    let mut secret = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut secret);
    let secret: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
    crate::write_file(&path, secret.as_bytes())
        .map_err(|error| format!("Could not save the browser session secret: {error}"))?;
    Ok(secret)
}

#[derive(Deserialize)]
struct Token {
    token: String,
}
async fn session(State(app): State<Arc<App>>, Json(body): Json<Token>) -> Response {
    if body.token != app.secret {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        [(
            header::SET_COOKIE,
            format!(
                "{}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=34560000",
                app.cookie, app.secret
            ),
        )],
        StatusCode::NO_CONTENT,
    )
        .into_response()
}

/// The tag covers everything but the revision, so pages skip snapshots that did not change.
async fn snapshot(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Reply<'a> {
        #[serde(flatten)]
        saved: &'a Snapshot,
        readable: Vec<&'a str>,
        sendable: Vec<&'a str>,
        voice: BTreeMap<String, Vec<VoiceUser>>,
        calls: HashMap<String, Vec<String>>,
        account: &'a Option<String>,
        session: u64,
    }
    let _gate = app.session.gate.read().await;
    let state = app.session.state.lock().unwrap();
    let voice = app.cache.voice();
    let calls = app.cache.calls();
    let body = app.cache.read(|saved| {
        serde_json::to_vec(&Reply {
            saved,
            readable: saved.allowed(Access::View),
            sendable: saved.allowed(Access::Send),
            voice,
            calls,
            account: &state.account,
            session: app.session.generation(),
        })
    });
    let Ok(mut body) = body else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let tag = content_tag(&body);
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == tag.as_bytes())
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, tag)]).into_response();
    }
    // The revision becomes the last field, replacing the closing brace.
    body.pop();
    body.extend(format!(",\"snapshotRevision\":{}}}", state.snapshot_revision).bytes());
    (
        [
            (header::CONTENT_TYPE, "application/json".to_owned()),
            (header::ETAG, tag),
        ],
        body,
    )
        .into_response()
}

async fn messages(
    State(app): State<Arc<App>>,
    Path(channel): Path<String>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let _render = app
        .session
        .remote
        .try_acquire()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let account = page_account(&headers);
    let (revision, rows) = app
        .cache
        .readable_messages(&channel, account)
        .ok_or(StatusCode::NOT_FOUND)?;
    let tag = format!("\"{account}:{revision}\"");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(tag.as_str())
    {
        return Ok(([(header::ETAG, tag)], StatusCode::NOT_MODIFIED).into_response());
    }
    let messages =
        tokio::task::block_in_place(|| rows.into_iter().map(Message::from).collect::<Vec<_>>());
    Ok(([(header::ETAG, tag)], Json(messages)).into_response())
}

async fn client_events(State(app): State<Arc<App>>) -> Response {
    let receiver = app.session.changes.subscribe();
    if !*receiver.borrow() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.changed().await.ok()?;
        if !*receiver.borrow_and_update() {
            return None;
        }
        Some((
            Ok::<_, std::convert::Infallible>(ServerEvent::default().data("update")),
            receiver,
        ))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn client_state(State(app): State<Arc<App>>) -> Response {
    #[derive(Serialize)]
    struct Reply<'a> {
        #[serde(flatten)]
        state: &'a crate::session::State,
        session: u64,
        notices: Vec<crate::discord::cache::Notice>,
    }
    let session = &app.session;
    let _gate = session.gate.read().await;
    let state = session.state.lock().unwrap();
    Json(Reply {
        state: &state,
        session: session.generation(),
        notices: app.cache.notices(),
    })
    .into_response()
}

fn reply(result: Result<serde_json::Value, String>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::CONFLICT, error).into_response(),
    }
}

fn busy() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "The client is busy").into_response()
}

fn account_changed() -> Response {
    (StatusCode::CONFLICT, "The active account changed").into_response()
}

/// Page requests that run against the shared session and cache.
async fn ui(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(request): Json<requests::Action>,
) -> Response {
    let session = &app.session;
    let Ok(_remote) = (!request.cache_only())
        .then(|| session.remote.try_acquire())
        .transpose()
    else {
        return busy();
    };
    let _gate = session.gate.read().await;
    if session.closed() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !matches!(request, requests::Action::Preferences { .. }) && !signed_in(&app, &headers) {
        return account_changed();
    }
    reply(requests::apply(&app, request).await)
}

/// Account and media commands, which the worker runs one at a time.
async fn action(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(command): Json<worker::Command>,
) -> Response {
    let guard = if command.action.changes_account() {
        Some(app.session.gate.clone().write_owned().await)
    } else {
        None
    };
    if app.session.closed() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let (sender, result) = tokio::sync::oneshot::channel();
    if app
        .commands
        .try_send(Request::Action(
            command,
            page_account(&headers).to_owned(),
            sender,
            guard,
        ))
        .is_err()
    {
        return busy();
    }
    match result.await {
        Ok(result) => reply(result),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
