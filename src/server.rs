//! Axum HTTP, WebSocket HID (`/ws/hid`), and SSE telemetry (`/api/telemetry/stream`) server.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::header::HeaderValue;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{stream, SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::wrappers::BroadcastStream;
use zeroize::Zeroizing;

use crate::aoa::HidBackend;
use crate::error::{AppError, Result};
use crate::mirror::{MirrorStreamMode, ScreenChunk, ScreenMirrorBackend};
use crate::telemetry::{TelemetryEvent, TelemetryStore};

const INDEX_HTML: &str = include_str!("../static/index.html");
const SESSION_COOKIE_NAME: &str = "otg_session";
const MAX_SESSIONS: usize = 64;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_WS_MSG_BYTES: usize = 16 * 1024;

/// Shared server state across HTTP, WebSocket, and SSE handlers.
pub struct AppState {
    pub bridge: Arc<dyn HidBackend>,
    pub mirror: Arc<dyn ScreenMirrorBackend>,
    pub telemetry: Arc<TelemetryStore>,
    pub token: Option<Zeroizing<String>>,
    pub sessions: Mutex<VecDeque<String>>,
    pub allowed_hosts: Vec<String>,
    pub shutdown_rx: watch::Receiver<bool>,
}

/// Return `true` when `host` resolves strictly to loopback (`localhost`, `127.0.0.0/8`, or `::1`).
#[must_use]
pub fn is_loopback_host(host: &str) -> bool {
    let lower = host.trim().to_ascii_lowercase();
    let stripped =
        lower.strip_prefix('[').and_then(|s| s.strip_suffix(']')).unwrap_or(lower.as_str());
    stripped == "localhost" || stripped.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Refuse unauthenticated non-loopback binds so physical USB HID injection is never exposed open.
pub fn validate_bind_auth(host: &str, has_token: bool) -> std::result::Result<(), String> {
    if has_token || is_loopback_host(host) {
        Ok(())
    } else {
        Err(format!(
            "Refusing to bind non-loopback address '{host}' without --token / ANDROID_WEB_OTG_TOKEN. Set a strong token or bind to 127.0.0.1."
        ))
    }
}

/// Build strict `Host` whitelist when bound to loopback; non-loopback binds rely on
/// same-origin `Origin` + `Sec-Fetch-Site` checks so private MagicDNS hostnames work cleanly.
#[must_use]
pub fn allowed_hosts_for_bind(host: &str, port: u16) -> Vec<String> {
    let lower = host.trim().to_ascii_lowercase();
    if is_loopback_host(&lower) {
        let mut hosts = vec![
            "127.0.0.1".to_string(),
            format!("127.0.0.1:{port}"),
            "localhost".to_string(),
            format!("localhost:{port}"),
            "[::1]".to_string(),
            format!("[::1]:{port}"),
        ];
        let bracketed = if lower.contains(':') && !lower.starts_with('[') {
            format!("[{lower}]")
        } else {
            lower
        };
        let with_port = format!("{bracketed}:{port}");
        if !hosts.contains(&bracketed) {
            hosts.push(bracketed);
        }
        if !hosts.contains(&with_port) {
            hosts.push(with_port);
        }
        hosts
    } else {
        Vec::new()
    }
}

#[derive(Deserialize)]
pub struct TelemetryQuery {
    pub since_id: Option<u64>,
    pub category: Option<String>,
    pub level: Option<String>,
    pub limit: Option<usize>,
    pub download: Option<u8>,
}

#[derive(Deserialize)]
pub struct AuthPayload {
    pub token: String,
}

#[derive(Deserialize)]
pub struct MousePayload {
    #[serde(default)]
    pub dx: i32,
    #[serde(default)]
    pub dy: i32,
    pub buttons: Option<u8>,
    #[serde(default)]
    pub vscroll: Option<i32>,
    #[serde(default)]
    pub wheel: Option<i32>,
    #[serde(default)]
    pub hscroll: i32,
    pub click: Option<String>,
}

#[derive(Deserialize)]
pub struct KeyPayload {
    #[serde(rename = "mod", default)]
    pub mod_mask: u8,
    #[serde(default)]
    pub code: u8,
    pub shortcut: Option<String>,
}

#[derive(Deserialize)]
pub struct TextPayload {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub enter: bool,
}

#[derive(Deserialize)]
pub struct UnlockPayload {
    pub secret: Option<String>,
    #[serde(default)]
    pub use_preset: bool,
    #[serde(default = "default_wake_method")]
    pub wake_method: String,
    #[serde(default = "default_true")]
    pub enter: bool,
    #[serde(default = "default_delay_ms")]
    pub delay_ms: u64,
}

fn default_wake_method() -> String {
    "swipe".to_string()
}

fn default_true() -> bool {
    true
}

fn default_delay_ms() -> u64 {
    600
}

#[derive(Deserialize)]
pub struct MirrorTapPayload {
    pub x: u32,
    pub y: u32,
}

#[derive(Deserialize)]
pub struct MirrorSwipePayload {
    pub x1: u32,
    pub y1: u32,
    pub x2: u32,
    pub y2: u32,
    #[serde(default = "default_swipe_duration_ms")]
    pub duration_ms: u32,
}

fn default_swipe_duration_ms() -> u32 {
    280
}

#[derive(Deserialize)]
pub struct ScreenWsQuery {
    pub mode: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WsHidCmd {
    Mouse {
        #[serde(default)]
        dx: i32,
        #[serde(default)]
        dy: i32,
        buttons: Option<u8>,
        #[serde(default)]
        vscroll: i32,
        #[serde(default)]
        hscroll: i32,
        click: Option<String>,
    },
    Key {
        #[serde(rename = "mod", default)]
        mod_mask: u8,
        #[serde(default)]
        code: u8,
        shortcut: Option<String>,
    },
    SwipeUp,
    Text {
        #[serde(default)]
        text: String,
        #[serde(default)]
        enter: bool,
    },
    Ping,
}

fn parse_origin_authority(origin: &str) -> Option<(String, &str)> {
    let lower = origin.trim();
    let (scheme, rest) = if let Some(r) = lower.strip_prefix("http://") {
        ("http", r)
    } else {
        ("https", lower.strip_prefix("https://")?)
    };
    if rest.is_empty()
        || rest.contains('/')
        || rest.contains('?')
        || rest.contains('#')
        || rest.contains('@')
    {
        return None;
    }
    Some((scheme.to_string(), rest))
}

fn hosts_match(origin_scheme: &str, origin_auth: &str, host_hdr: &str) -> bool {
    if origin_auth.eq_ignore_ascii_case(host_hdr) {
        return true;
    }
    let default_port = if origin_scheme == "https" { ":443" } else { ":80" };
    let norm_origin = origin_auth.strip_suffix(default_port).unwrap_or(origin_auth);
    let norm_host = host_hdr.strip_suffix(default_port).unwrap_or(host_hdr);
    norm_origin.eq_ignore_ascii_case(norm_host)
}

fn extract_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    for hdr_val in headers.get_all(header::COOKIE) {
        let Ok(raw) = hdr_val.to_str() else { continue };
        for part in raw.split(';') {
            let trimmed = part.trim();
            if let Some((k, v)) = trimmed.split_once('=') {
                if k.trim() == name {
                    return Some(v.trim());
                }
            }
        }
    }
    None
}

fn ct_eq_str(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

fn check_origin_and_host(state: &AppState, headers: &HeaderMap, path: &str) -> Result<()> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .ok_or_else(|| AppError::Forbidden("Missing or invalid Host header".into()))?;

    if !state.allowed_hosts.is_empty() {
        let lower_host = host.to_ascii_lowercase();
        if !state.allowed_hosts.iter().any(|allowed| allowed == &lower_host) {
            return Err(AppError::Forbidden("Untrusted Host header".into()));
        }
    }

    if let Some(sfs) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        let val = sfs.trim().to_ascii_lowercase();
        if val != "same-origin" && val != "none" {
            return Err(AppError::Forbidden("Cross-origin request rejected".into()));
        }
    }

    let origin_hdr = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if let Some(origin) = origin_hdr {
        let Some((scheme, auth)) = parse_origin_authority(origin) else {
            return Err(AppError::Forbidden("Malformed Origin header".into()));
        };
        if !hosts_match(&scheme, auth, host) {
            return Err(AppError::Forbidden("Cross-origin Origin mismatch".into()));
        }
    } else if path.starts_with("/ws/") {
        return Err(AppError::Forbidden("WebSocket requires matching Origin header".into()));
    }

    Ok(())
}

fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<()> {
    let Some(ref expected) = state.token else {
        return Ok(());
    };
    if let Some(hdr) = headers.get("x-auth-token").and_then(|v| v.to_str().ok()) {
        if ct_eq_str(hdr, expected) {
            return Ok(());
        }
    }
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(bearer) = auth.strip_prefix("Bearer ") {
            if ct_eq_str(bearer.trim(), expected) {
                return Ok(());
            }
        }
    }
    if let Some(cookie_val) = extract_cookie(headers, SESSION_COOKIE_NAME) {
        let sessions = state.sessions.lock().expect("sessions mutex poisoned");
        let valid = sessions.iter().any(|s| ct_eq_str(cookie_val, s));
        if valid {
            return Ok(());
        }
    }
    Err(AppError::Unauthorized("Invalid or missing token".into()))
}

async fn security_middleware(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let is_public =
        (path == "/" && method == Method::GET) || (path == "/api/auth" && method == Method::POST);

    let gate = check_origin_and_host(&state, req.headers(), &path).and_then(|()| {
        if is_public {
            Ok(())
        } else {
            check_auth(&state, req.headers())
        }
    });

    let mut resp = match gate {
        Ok(()) => next.run(req).await,
        Err(err) => err.into_response(),
    };

    let headers = resp.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

fn generate_session_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| AppError::Internal(format!("CSPRNG failed: {e}")))?;
    let mut out = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{b:02x}");
    }
    Ok(out)
}

/// Build the full Axum [`Router`] for HTTP, WebSocket HID, and SSE telemetry.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/auth", post(post_auth))
        .route("/api/status", get(get_status))
        .route("/api/specs", get(get_specs))
        .route("/api/telemetry", get(get_telemetry))
        .route("/api/telemetry/clear", post(post_telemetry_clear))
        .route("/api/telemetry/stream", get(stream_telemetry))
        .route("/api/reconnect", post(post_reconnect))
        .route("/api/mouse", post(post_mouse))
        .route("/api/swipe_up", post(post_swipe_up))
        .route("/api/key", post(post_key))
        .route("/api/text", post(post_text))
        .route("/api/unlock", post(post_unlock))
        .route("/api/mirror/status", get(get_mirror_status))
        .route("/api/screen/frame", get(get_screen_frame))
        .route("/api/mirror/tap", post(post_mirror_tap))
        .route("/api/mirror/swipe", post(post_mirror_swipe))
        .route("/ws/hid", get(ws_hid))
        .route("/ws/screen", get(ws_screen))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(state.clone(), security_middleware))
        .with_state(state)
}

async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(INDEX_HTML))
}

async fn post_auth(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AuthPayload>,
) -> Result<Response> {
    let Some(ref expected) = state.token else {
        return Ok(Json(json!({ "ok": true, "auth_required": false })).into_response());
    };
    if !ct_eq_str(payload.token.trim(), expected) {
        return Err(AppError::Unauthorized("Invalid API token".into()));
    }
    let session = generate_session_token()?;
    {
        let mut sessions = state.sessions.lock().expect("sessions mutex poisoned");
        if sessions.len() >= MAX_SESSIONS {
            sessions.pop_front();
        }
        sessions.push_back(session.clone());
    }
    let cookie = format!("{SESSION_COOKIE_NAME}={session}; HttpOnly; SameSite=Strict; Path=/");
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie), (header::CACHE_CONTROL, "no-store".to_string())],
        Json(json!({ "ok": true })),
    )
        .into_response())
}

fn preferred_usb_serial(state: &AppState) -> Option<String> {
    state.bridge.status_snapshot().device.and_then(|d| (!d.serial.is_empty()).then_some(d.serial))
}

async fn get_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let usb_status = state.bridge.status_snapshot();
    let mirror_status = state
        .mirror
        .status(
            usb_status
                .device
                .as_ref()
                .and_then(|d| (!d.serial.is_empty()).then(|| d.serial.clone())),
        )
        .await;
    Json(json!({
        "ok": true,
        "auth_required": state.token.is_some(),
        "unlock_preset_configured": state.bridge.unlock_preset_configured(),
        "auto_unlock": state.bridge.auto_unlock_enabled(),
        "connected": usb_status.connected,
        "device": usb_status.device,
        "error": usb_status.error,
        "mirror": mirror_status,
        "telemetry": state.telemetry.metrics(),
    }))
}

async fn get_mirror_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mir = state.mirror.status(preferred_usb_serial(&state)).await;
    Json(json!({
        "ok": true,
        "adb_installed": mir.adb_installed,
        "state": mir.state,
        "serial": mir.serial,
        "screen_width": mir.screen_width,
        "screen_height": mir.screen_height,
        "hint": mir.hint,
    }))
}

async fn get_screen_frame(State(state): State<Arc<AppState>>) -> Result<Response> {
    let png = state.mirror.capture_png(preferred_usb_serial(&state)).await?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")],
        png,
    )
        .into_response())
}

async fn post_mirror_tap(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<MirrorTapPayload>,
) -> Result<impl IntoResponse> {
    state.mirror.tap(preferred_usb_serial(&state), payload.x, payload.y).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn post_mirror_swipe(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<MirrorSwipePayload>,
) -> Result<impl IntoResponse> {
    state
        .mirror
        .swipe(
            preferred_usb_serial(&state),
            payload.x1,
            payload.y1,
            payload.x2,
            payload.y2,
            payload.duration_ms,
        )
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn get_specs(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let st = state.bridge.status_snapshot();
    Json(json!({
        "ok": true,
        "connected": st.connected,
        "usb": st.device,
        "error": st.error,
    }))
}

async fn get_telemetry(
    State(state): State<Arc<AppState>>,
    Query(q): Query<TelemetryQuery>,
) -> Result<Response> {
    if q.download == Some(1) {
        let body = state.telemetry.export_jsonl();
        return Ok((
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"android-web-otg-telemetry.jsonl\"",
                ),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response());
    }
    let snap = state.telemetry.snapshot(
        q.since_id.unwrap_or(0),
        q.category.as_deref(),
        q.level.as_deref(),
        q.limit.unwrap_or(200),
    );
    Ok(Json(json!({
        "ok": true,
        "metrics": snap.metrics,
        "events": snap.events,
        "next_id": snap.next_id,
    }))
    .into_response())
}

async fn post_telemetry_clear(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let _ = state.telemetry.clear();
    let ev = state.telemetry.log("info", "system", "Telemetry log buffer cleared", None);
    Json(json!({ "ok": true, "event_id": ev.id }))
}

struct SseState {
    backlog: VecDeque<TelemetryEvent>,
    last_sent_id: u64,
    rx: BroadcastStream<TelemetryEvent>,
    shutdown_rx: watch::Receiver<bool>,
}

fn format_sse_event(ev: &TelemetryEvent) -> Option<Event> {
    let data = serde_json::to_string(ev).ok()?;
    Some(Event::default().id(ev.id.to_string()).event("telemetry").data(data))
}

async fn stream_telemetry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>> {
    let last_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let rx = BroadcastStream::new(state.telemetry.subscribe());
    let backlog = if last_id > 0 {
        VecDeque::from(state.telemetry.events_since(last_id))
    } else {
        VecDeque::new()
    };

    let init =
        SseState { backlog, last_sent_id: last_id, rx, shutdown_rx: state.shutdown_rx.clone() };

    let stream = stream::unfold(init, |mut st| async move {
        if *st.shutdown_rx.borrow() {
            return None;
        }
        while let Some(ev) = st.backlog.pop_front() {
            if ev.id > st.last_sent_id {
                st.last_sent_id = ev.id;
                if let Some(formatted) = format_sse_event(&ev) {
                    return Some((Ok(formatted), st));
                }
            }
        }
        loop {
            tokio::select! {
                biased;
                _ = st.shutdown_rx.changed() => {
                    return None;
                }
                maybe_msg = st.rx.next() => {
                    match maybe_msg {
                        Some(Ok(ev)) => {
                            if ev.id <= st.last_sent_id {
                                continue;
                            }
                            st.last_sent_id = ev.id;
                            if let Some(formatted) = format_sse_event(&ev) {
                                return Some((Ok(formatted), st));
                            }
                        }
                        Some(Err(BroadcastStreamRecvError::Lagged(skipped))) => {
                            let notice = Event::default().comment(format!("lagged:{skipped}"));
                            return Some((Ok(notice), st));
                        }
                        None => return None,
                    }
                }
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn post_reconnect(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let st = state.bridge.reconnect().await;
    Json(json!({
        "ok": st.connected,
        "connected": st.connected,
        "device": st.device,
        "error": st.error,
    }))
}

async fn post_mouse(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<MousePayload>,
) -> Result<impl IntoResponse> {
    let vscroll = payload.vscroll.or(payload.wheel).unwrap_or(0);
    state
        .bridge
        .send_mouse(
            payload.dx,
            payload.dy,
            payload.buttons,
            vscroll,
            payload.hscroll,
            payload.click,
        )
        .await?;
    Ok(Json(json!({ "ok": true })))
}

async fn post_swipe_up(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse> {
    state.bridge.swipe_up().await?;
    Ok(Json(json!({ "ok": true })))
}

async fn post_key(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<KeyPayload>,
) -> Result<impl IntoResponse> {
    state.bridge.send_key(payload.mod_mask, payload.code, payload.shortcut).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn post_text(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<TextPayload>,
) -> Result<impl IntoResponse> {
    let sent = state.bridge.send_text(payload.text, payload.enter).await?;
    Ok(Json(json!({ "ok": true, "sent": sent })))
}

async fn post_unlock(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<UnlockPayload>,
) -> Result<impl IntoResponse> {
    if !matches!(payload.wake_method.as_str(), "swipe" | "space" | "none") {
        return Err(AppError::BadRequest("Invalid wake_method".into()));
    }
    let secret = payload.secret.map(Zeroizing::new);
    if payload.use_preset {
        if !state.bridge.unlock_preset_configured() {
            return Err(AppError::BadRequest("No unlock preset configured".into()));
        }
    } else {
        let len = secret.as_ref().map_or(0, |s| s.chars().count());
        if len == 0 || len > 128 {
            return Err(AppError::BadRequest("secret must be 1..128 characters".into()));
        }
    }
    let sent = state
        .bridge
        .unlock_with_secret(
            secret,
            payload.wake_method,
            payload.enter,
            payload.delay_ms,
            payload.use_preset,
        )
        .await?;
    Ok(Json(json!({ "ok": true, "sent": sent })))
}

struct WsGuard(Arc<TelemetryStore>);
impl Drop for WsGuard {
    fn drop(&mut self) {
        let remaining = self.0.ws_disconnected();
        self.0.log(
            "info",
            "http",
            format!("WebSocket HID client disconnected (active={remaining})"),
            None,
        );
    }
}

async fn ws_hid(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.max_message_size(MAX_WS_MSG_BYTES).on_upgrade(move |socket| handle_ws_hid(socket, state))
}

async fn send_ws_err(socket: &mut WebSocket, err: &AppError) {
    let _ = socket
        .send(Message::Text(json!({ "ok": false, "error": err.to_string() }).to_string().into()))
        .await;
}

async fn handle_ws_hid(mut socket: WebSocket, state: Arc<AppState>) {
    let active = state.telemetry.ws_connected();
    state.telemetry.log(
        "ok",
        "http",
        format!("WebSocket HID channel live (active={active})"),
        None,
    );
    let _guard = WsGuard(state.telemetry.clone());
    let mut shutdown_rx = state.shutdown_rx.clone();

    loop {
        if *shutdown_rx.borrow() {
            let _ = socket.close().await;
            break;
        }
        let next_msg = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                let _ = socket.close().await;
                break;
            }
            incoming = socket.recv() => incoming,
        };
        let Some(Ok(msg)) = next_msg else {
            break;
        };
        match msg {
            Message::Text(txt) => match serde_json::from_str::<WsHidCmd>(&txt) {
                Ok(WsHidCmd::Ping) => {
                    let _ = socket.send(Message::Text(r#"{"ok":true,"type":"pong"}"#.into())).await;
                }
                Ok(WsHidCmd::Mouse { dx, dy, buttons, vscroll, hscroll, click }) => {
                    if let Err(err) =
                        state.bridge.send_mouse(dx, dy, buttons, vscroll, hscroll, click).await
                    {
                        send_ws_err(&mut socket, &err).await;
                    }
                }
                Ok(WsHidCmd::Key { mod_mask, code, shortcut }) => {
                    if let Err(err) = state.bridge.send_key(mod_mask, code, shortcut).await {
                        send_ws_err(&mut socket, &err).await;
                    }
                }
                Ok(WsHidCmd::SwipeUp) => {
                    if let Err(err) = state.bridge.swipe_up().await {
                        send_ws_err(&mut socket, &err).await;
                    }
                }
                Ok(WsHidCmd::Text { text, enter }) => {
                    if let Err(err) = state.bridge.send_text(text, enter).await {
                        send_ws_err(&mut socket, &err).await;
                    }
                }
                Err(err) => {
                    let _ = socket
                        .send(Message::Text(
                            json!({ "ok": false, "error": err.to_string() }).to_string().into(),
                        ))
                        .await;
                }
            },
            Message::Close(_) => break,
            _ => {}
        }
    }
}

async fn ws_screen(
    ws: WebSocketUpgrade,
    Query(q): Query<ScreenWsQuery>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let mode = MirrorStreamMode::from_query(q.mode.as_deref());
    ws.max_message_size(MAX_WS_MSG_BYTES)
        .on_upgrade(move |socket| handle_ws_screen(socket, state, mode))
}

async fn handle_ws_screen(mut socket: WebSocket, state: Arc<AppState>, mode: MirrorStreamMode) {
    let mut rx = match state
        .mirror
        .spawn_stream(preferred_usb_serial(&state), mode, state.shutdown_rx.clone())
        .await
    {
        Ok(rx) => rx,
        Err(err) => {
            let _ = socket
                .send(Message::Text(
                    json!({ "type": "error", "error": err.to_string() }).to_string().into(),
                ))
                .await;
            let _ = socket.close().await;
            return;
        }
    };

    let mut shutdown_rx = state.shutdown_rx.clone();
    loop {
        if *shutdown_rx.borrow() {
            let _ = socket.close().await;
            break;
        }
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                let _ = socket.close().await;
                break;
            }
            maybe_chunk = rx.recv() => {
                let Some(chunk) = maybe_chunk else {
                    let _ = socket.close().await;
                    break;
                };
                let msg = match chunk {
                    ScreenChunk::Meta { engine, width, height, codec } => Message::Text(
                        json!({
                            "type": "meta",
                            "engine": engine,
                            "width": width,
                            "height": height,
                            "codec": codec,
                        })
                        .to_string()
                        .into(),
                    ),
                    ScreenChunk::Error(error) => Message::Text(
                        json!({ "type": "error", "error": error }).to_string().into(),
                    ),
                    ScreenChunk::H264 { is_keyframe, data } => {
                        let mut buf = Vec::with_capacity(1 + data.len());
                        buf.push(if is_keyframe { 0x01 } else { 0x02 });
                        buf.extend_from_slice(&data);
                        Message::Binary(buf.into())
                    }
                    ScreenChunk::Png(data) => {
                        let mut buf = Vec::with_capacity(1 + data.len());
                        buf.push(0x03);
                        buf.extend_from_slice(&data);
                        Message::Binary(buf.into())
                    }
                };
                if socket.send(msg).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_loopback_hosts_and_bind_auth() {
        for h in ["127.0.0.1", "127.0.0.2", "localhost", "::1", "[::1]"] {
            assert!(is_loopback_host(h), "{h} should be loopback");
            assert!(validate_bind_auth(h, false).is_ok());
            assert!(!allowed_hosts_for_bind(h, 8095).is_empty());
        }
        for h in ["0.0.0.0", "::", "192.0.2.10", "198.51.100.2"] {
            assert!(!is_loopback_host(h), "{h} should not be loopback");
            assert!(validate_bind_auth(h, false).is_err());
            assert!(validate_bind_auth(h, true).is_ok());
            assert!(allowed_hosts_for_bind(h, 8095).is_empty());
        }
    }
}
