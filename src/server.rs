//! Axum HTTP, WebSocket HID (`/ws/hid`), and SSE telemetry (`/api/telemetry/stream`) server.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio_stream::wrappers::BroadcastStream;

use crate::adb::AdbInspector;
use crate::aoa::HidBackend;
use crate::error::{AppError, Result};
use crate::telemetry::TelemetryStore;

const INDEX_HTML: &str = include_str!("../static/index.html");

/// Shared server state across HTTP, WebSocket, and SSE handlers.
pub struct AppState {
    pub bridge: Arc<dyn HidBackend>,
    pub adb: Arc<AdbInspector>,
    pub telemetry: Arc<TelemetryStore>,
    pub token: Option<String>,
}

#[derive(Deserialize)]
pub struct TelemetryQuery {
    pub since_id: Option<u64>,
    pub category: Option<String>,
    pub level: Option<String>,
    pub limit: Option<usize>,
    pub download: Option<u8>,
    pub token: Option<String>,
}

#[derive(Deserialize)]
pub struct AuthQuery {
    pub token: Option<String>,
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
pub struct AdbTapPayload {
    pub x: i32,
    pub y: i32,
}

#[derive(Deserialize)]
pub struct AdbSwipePayload {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    #[serde(default = "default_swipe_dur")]
    pub duration_ms: i32,
}

fn default_swipe_dur() -> i32 {
    300
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

fn check_auth(state: &AppState, headers: &HeaderMap, query_token: Option<&str>) -> Result<()> {
    let Some(ref expected) = state.token else {
        return Ok(());
    };
    if let Some(hdr) = headers.get("x-auth-token").and_then(|v| v.to_str().ok()) {
        if bool::from(hdr.as_bytes().ct_eq(expected.as_bytes())) {
            return Ok(());
        }
    }
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(bearer) = auth.strip_prefix("Bearer ") {
            if bool::from(bearer.trim().as_bytes().ct_eq(expected.as_bytes())) {
                return Ok(());
            }
        }
    }
    if let Some(qtok) = query_token {
        if bool::from(qtok.as_bytes().ct_eq(expected.as_bytes())) {
            return Ok(());
        }
    }
    Err(AppError::Unauthorized("Invalid or missing token".into()))
}

/// Build the full Axum [`Router`] for HTTP, WebSocket HID, and SSE telemetry.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(get_status))
        .route("/api/specs", get(get_specs))
        .route("/api/telemetry", get(get_telemetry))
        .route("/api/telemetry/clear", post(post_telemetry_clear))
        .route("/api/telemetry/stream", get(stream_telemetry))
        .route("/api/screen.png", get(get_screen_png))
        .route("/api/reconnect", post(post_reconnect))
        .route("/api/mouse", post(post_mouse))
        .route("/api/swipe_up", post(post_swipe_up))
        .route("/api/key", post(post_key))
        .route("/api/text", post(post_text))
        .route("/api/unlock", post(post_unlock))
        .route("/api/adb/tap", post(post_adb_tap))
        .route("/api/adb/swipe", post(post_adb_swipe))
        .route("/ws/hid", get(ws_hid))
        .with_state(state)
}

async fn index() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], Html(INDEX_HTML))
}

async fn get_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    let b = state.bridge.clone();
    let usb_status = tokio::task::spawn_blocking(move || b.status(false))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let adb_devs = state.adb.list_devices().await;
    let adb_serial = state.adb.select_authorized_serial(&adb_devs);
    let adb_lines: Vec<String> =
        adb_devs.iter().map(|d| format!("{}\t{}", d.serial, d.state)).collect();
    Ok(Json(json!({
        "ok": true,
        "auth_required": state.token.is_some(),
        "unlock_preset_configured": state.bridge.unlock_preset_configured(),
        "auto_unlock": state.bridge.auto_unlock_enabled(),
        "connected": usb_status.connected,
        "device": usb_status.device,
        "error": usb_status.error,
        "adb_devices": adb_lines,
        "adb_parsed": adb_devs,
        "adb_authorized": adb_serial.is_some(),
        "adb_serial": adb_serial,
        "telemetry": state.telemetry.metrics(),
    })))
}

async fn get_specs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    let b = state.bridge.clone();
    let usb_specs = tokio::task::spawn_blocking(move || match b.usb_specs() {
        Some(s) => Some(s),
        None => b.status(false).device,
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;

    let adb_devs = state.adb.list_devices().await;
    let adb_serial = state.adb.select_authorized_serial(&adb_devs);
    let android_specs = if let Some(ref serial) = adb_serial {
        Some(state.adb.get_specs(serial).await)
    } else {
        None
    };
    let scrcpy_cmd = adb_serial.as_ref().map(|s| format!("scrcpy -s {s}"));

    Ok(Json(json!({
        "ok": true,
        "usb": usb_specs,
        "adb_authorized": adb_serial.is_some(),
        "adb_serial": adb_serial,
        "android": android_specs,
        "scrcpy_cmd": scrcpy_cmd,
    })))
}

async fn get_telemetry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<TelemetryQuery>,
) -> Result<Response> {
    check_auth(&state, &headers, q.token.as_deref())?;
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

async fn post_telemetry_clear(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    state.telemetry.clear();
    state.telemetry.log("info", "system", "Telemetry log buffer cleared", None);
    Ok(Json(json!({ "ok": true })))
}

async fn stream_telemetry(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
) -> Result<Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>>> {
    check_auth(&state, &headers, q.token.as_deref())?;
    let rx = state.telemetry.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| async move {
        let ev = msg.ok()?;
        let data = serde_json::to_string(&ev).ok()?;
        Some(Ok(Event::default().id(ev.id.to_string()).event("telemetry").data(data)))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn get_screen_png(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response> {
    check_auth(&state, &headers, q.get("token").map(String::as_str))?;
    let devs = state.adb.list_devices().await;
    let Some(serial) = state.adb.select_authorized_serial(&devs) else {
        return Err(AppError::ServiceUnavailable("No authorized ADB device".into()));
    };
    let Some(png) = state.adb.capture_png(&serial).await else {
        return Err(AppError::Usb("Failed to capture device screen".into()));
    };
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")],
        png,
    )
        .into_response())
}

async fn post_reconnect(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    let b = state.bridge.clone();
    let st = tokio::task::spawn_blocking(move || b.status(true))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;
    let adb_devs = state.adb.list_devices().await;
    let adb_serial = state.adb.select_authorized_serial(&adb_devs);
    let adb_lines: Vec<String> =
        adb_devs.iter().map(|d| format!("{}\t{}", d.serial, d.state)).collect();
    Ok(Json(json!({
        "ok": true,
        "connected": st.connected,
        "device": st.device,
        "error": st.error,
        "adb_devices": adb_lines,
        "adb_authorized": adb_serial.is_some(),
        "adb_serial": adb_serial,
    })))
}

async fn post_mouse(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
    Json(payload): Json<MousePayload>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    let b = state.bridge.clone();
    let vscroll = payload.vscroll.or(payload.wheel).unwrap_or(0);
    tokio::task::spawn_blocking(move || {
        b.send_mouse(
            payload.dx,
            payload.dy,
            payload.buttons,
            vscroll,
            payload.hscroll,
            payload.click.as_deref(),
        )
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))??;
    Ok(Json(json!({ "ok": true })))
}

async fn post_swipe_up(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    let b = state.bridge.clone();
    tokio::task::spawn_blocking(move || b.swipe_up())
        .await
        .map_err(|e| AppError::Internal(e.to_string()))??;
    Ok(Json(json!({ "ok": true })))
}

async fn post_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
    Json(payload): Json<KeyPayload>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    let b = state.bridge.clone();
    tokio::task::spawn_blocking(move || {
        b.send_key(payload.mod_mask, payload.code, payload.shortcut.as_deref())
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))??;
    Ok(Json(json!({ "ok": true })))
}

async fn post_text(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
    Json(payload): Json<TextPayload>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    let b = state.bridge.clone();
    let sent = tokio::task::spawn_blocking(move || b.send_text(&payload.text, payload.enter))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))??;
    Ok(Json(json!({ "ok": true, "sent": sent })))
}

async fn post_unlock(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
    Json(payload): Json<UnlockPayload>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    if !matches!(payload.wake_method.as_str(), "swipe" | "space" | "none") {
        return Err(AppError::BadRequest("Invalid wake_method".into()));
    }
    if payload.use_preset {
        if !state.bridge.unlock_preset_configured() {
            return Err(AppError::BadRequest("No unlock preset configured".into()));
        }
    } else {
        let len = payload.secret.as_ref().map_or(0, String::len);
        if len == 0 || len > 128 {
            return Err(AppError::BadRequest("secret must be 1..128 characters".into()));
        }
    }
    let b = state.bridge.clone();
    let sent = tokio::task::spawn_blocking(move || {
        b.unlock_with_secret(
            payload.secret.as_deref(),
            &payload.wake_method,
            payload.enter,
            payload.delay_ms,
            payload.use_preset,
        )
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))??;
    Ok(Json(json!({ "ok": true, "sent": sent })))
}

async fn post_adb_tap(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
    Json(payload): Json<AdbTapPayload>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    let devs = state.adb.list_devices().await;
    let Some(serial) = state.adb.select_authorized_serial(&devs) else {
        return Err(AppError::ServiceUnavailable("No authorized ADB device".into()));
    };
    let ok = state.adb.tap(&serial, payload.x, payload.y).await.map_err(AppError::BadRequest)?;
    Ok(Json(json!({ "ok": ok })))
}

async fn post_adb_swipe(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
    Json(payload): Json<AdbSwipePayload>,
) -> Result<impl IntoResponse> {
    check_auth(&state, &headers, q.token.as_deref())?;
    let devs = state.adb.list_devices().await;
    let Some(serial) = state.adb.select_authorized_serial(&devs) else {
        return Err(AppError::ServiceUnavailable("No authorized ADB device".into()));
    };
    let ok = state
        .adb
        .swipe(&serial, payload.x1, payload.y1, payload.x2, payload.y2, payload.duration_ms)
        .await
        .map_err(AppError::BadRequest)?;
    Ok(Json(json!({ "ok": ok })))
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

async fn ws_hid(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<AuthQuery>,
) -> Result<Response> {
    check_auth(&state, &headers, q.token.as_deref())?;
    Ok(ws.on_upgrade(move |socket| handle_ws_hid(socket, state)))
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

    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(txt) => {
                let parsed: std::result::Result<WsHidCmd, _> = serde_json::from_str(&txt);
                match parsed {
                    Ok(WsHidCmd::Ping) => {
                        let _ =
                            socket.send(Message::Text(r#"{"ok":true,"type":"pong"}"#.into())).await;
                    }
                    Ok(WsHidCmd::Mouse { dx, dy, buttons, vscroll, hscroll, click }) => {
                        let b = state.bridge.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            b.send_mouse(dx, dy, buttons, vscroll, hscroll, click.as_deref())
                        })
                        .await;
                        if let Ok(Err(err)) = res {
                            let _ = socket
                                .send(Message::Text(
                                    json!({"ok":false,"error":err.to_string()}).to_string().into(),
                                ))
                                .await;
                        }
                    }
                    Ok(WsHidCmd::Key { mod_mask, code, shortcut }) => {
                        let b = state.bridge.clone();
                        let res = tokio::task::spawn_blocking(move || {
                            b.send_key(mod_mask, code, shortcut.as_deref())
                        })
                        .await;
                        if let Ok(Err(err)) = res {
                            let _ = socket
                                .send(Message::Text(
                                    json!({"ok":false,"error":err.to_string()}).to_string().into(),
                                ))
                                .await;
                        }
                    }
                    Ok(WsHidCmd::SwipeUp) => {
                        let b = state.bridge.clone();
                        let _ = tokio::task::spawn_blocking(move || b.swipe_up()).await;
                    }
                    Ok(WsHidCmd::Text { text, enter }) => {
                        let b = state.bridge.clone();
                        let _ =
                            tokio::task::spawn_blocking(move || b.send_text(&text, enter)).await;
                    }
                    Err(err) => {
                        let _ = socket
                            .send(Message::Text(
                                json!({"ok":false,"error":err.to_string()}).to_string().into(),
                            ))
                            .await;
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}
