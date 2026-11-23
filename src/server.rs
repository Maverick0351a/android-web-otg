//! Axum HTTP, WebSocket HID (`/ws/hid`), and SSE telemetry (`/api/telemetry/stream`) server.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, Request, State};
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
const SESSION_COOKIE_MAX_AGE_SECS: u64 = 12 * 60 * 60;
const SESSION_ABSOLUTE_TTL: Duration = Duration::from_secs(SESSION_COOKIE_MAX_AGE_SECS);
const SESSION_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
const SESSION_WATCH_INTERVAL: Duration = Duration::from_secs(5);
const AUTH_FREE_FAILURES: u32 = 5;
const AUTH_BACKOFF_BASE_SECS: u64 = 1;
const AUTH_BACKOFF_MAX_SECS: u64 = 15 * 60;
const MAX_TRACKED_AUTH_IPS: usize = 1024;

/// Monotonic clock for session expiry and per-IP auth lockout.
pub trait Clock: Send + Sync {
    /// Current monotonic instant.
    fn now(&self) -> Instant;
}

/// Production clock backed by [`Instant::now`].
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Clock whose instant moves only when [`ManualClock::advance`] is called.
#[derive(Clone, Debug)]
pub struct ManualClock {
    origin: Instant,
    offset_ms: Arc<Mutex<u64>>,
}

impl ManualClock {
    /// Start at [`Instant::now`] with a zero offset.
    #[must_use]
    pub fn new() -> Self {
        Self { origin: Instant::now(), offset_ms: Arc::new(Mutex::new(0)) }
    }

    /// Move [`Clock::now`] forward. Shared by every clone of this clock.
    pub fn advance(&self, by: Duration) {
        let mut offset = self.offset_ms.lock().expect("clock mutex poisoned");
        let step = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        *offset = offset.saturating_add(step);
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        let offset = *self.offset_ms.lock().expect("clock mutex poisoned");
        self.origin + Duration::from_millis(offset)
    }
}

struct SessionRecord {
    token: String,
    created: Instant,
    last_seen: Instant,
}

/// In-memory session table. Expired records are removed on the next authenticated request.
#[derive(Default)]
pub struct SessionTable {
    records: VecDeque<SessionRecord>,
}

#[derive(Default)]
struct AuthFailure {
    failures: u32,
    locked_until: Option<Instant>,
}

enum AuthDecision {
    Allow,
    Reject,
    Lockout { retry_after_secs: u64 },
}

/// Bounded per-IP failure state for `POST /api/auth`. The key is the TCP peer, never a header.
#[derive(Default)]
pub struct AuthFailureTracker {
    by_ip: HashMap<IpAddr, AuthFailure>,
    order: VecDeque<IpAddr>,
}

impl AuthFailureTracker {
    fn observe(&mut self, ip: IpAddr, now: Instant, success: bool) -> AuthDecision {
        if success {
            self.remove(ip);
            return AuthDecision::Allow;
        }
        self.touch(ip);
        let entry = self.by_ip.entry(ip).or_default();
        if let Some(until) = entry.locked_until {
            if now < until {
                return AuthDecision::Lockout { retry_after_secs: retry_after_secs(until, now) };
            }
        }
        entry.failures = entry.failures.saturating_add(1);
        if entry.failures > AUTH_FREE_FAILURES {
            let until = now + Duration::from_secs(backoff_secs(entry.failures));
            entry.locked_until = Some(until);
            return AuthDecision::Lockout { retry_after_secs: retry_after_secs(until, now) };
        }
        AuthDecision::Reject
    }

    fn remove(&mut self, ip: IpAddr) {
        self.by_ip.remove(&ip);
        self.order.retain(|existing| *existing != ip);
    }

    fn touch(&mut self, ip: IpAddr) {
        if let Some(pos) = self.order.iter().position(|existing| *existing == ip) {
            self.order.remove(pos);
        }
        self.order.push_back(ip);
        while self.order.len() > MAX_TRACKED_AUTH_IPS {
            if let Some(old) = self.order.pop_front() {
                self.by_ip.remove(&old);
            }
        }
    }
}

fn backoff_secs(failures: u32) -> u64 {
    let exp = failures.saturating_sub(AUTH_FREE_FAILURES + 1).min(16);
    AUTH_BACKOFF_BASE_SECS.saturating_mul(1_u64 << exp).min(AUTH_BACKOFF_MAX_SECS)
}

fn retry_after_secs(until: Instant, now: Instant) -> u64 {
    let remaining = until.saturating_duration_since(now);
    let mut secs = remaining.as_secs();
    if remaining.subsec_nanos() > 0 {
        secs = secs.saturating_add(1);
    }
    secs.max(1)
}

/// Shared server state across HTTP, WebSocket, and SSE handlers.
pub struct AppState {
    pub bridge: Arc<dyn HidBackend>,
    pub mirror: Arc<dyn ScreenMirrorBackend>,
    pub telemetry: Arc<TelemetryStore>,
    pub token: Option<Zeroizing<String>>,
    pub sessions: Mutex<SessionTable>,
    pub auth_failures: Mutex<AuthFailureTracker>,
    pub clock: Arc<dyn Clock>,
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
    if presented_long_lived_token(expected, headers) {
        return Ok(());
    }
    if let Some(cookie_val) = extract_cookie(headers, SESSION_COOKIE_NAME) {
        let now = state.clock.now();
        let mut sessions = state.sessions.lock().expect("sessions mutex poisoned");
        if sessions.authenticate(cookie_val, now) {
            return Ok(());
        }
    }
    Err(AppError::Unauthorized("Invalid or missing token".into()))
}

fn session_is_live(session: &SessionRecord, now: Instant) -> bool {
    now.saturating_duration_since(session.created) < SESSION_ABSOLUTE_TTL
        && now.saturating_duration_since(session.last_seen) < SESSION_IDLE_TTL
}

impl SessionTable {
    fn authenticate(&mut self, cookie: &str, now: Instant) -> bool {
        let Some(index) = self.records.iter().position(|session| ct_eq_str(cookie, &session.token))
        else {
            return false;
        };
        if !session_is_live(&self.records[index], now) {
            self.records.remove(index);
            return false;
        }
        self.records[index].last_seen = now;
        true
    }

    fn insert(&mut self, record: SessionRecord) {
        if self.records.len() >= MAX_SESSIONS {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    fn remove_token(&mut self, cookie: &str) {
        self.records.retain(|session| !ct_eq_str(cookie, &session.token));
    }

    /// True when `cookie` matches a session that is still inside both lifetimes.
    /// Does not extend the idle timer.
    fn contains_live(&mut self, cookie: &str, now: Instant) -> bool {
        let Some(index) = self.records.iter().position(|session| ct_eq_str(cookie, &session.token))
        else {
            return false;
        };
        if session_is_live(&self.records[index], now) {
            return true;
        }
        self.records.remove(index);
        false
    }
}

fn presented_long_lived_token(expected: &str, headers: &HeaderMap) -> bool {
    if let Some(hdr) = headers.get("x-auth-token").and_then(|value| value.to_str().ok()) {
        if ct_eq_str(hdr, expected) {
            return true;
        }
    }
    if let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()) {
        if let Some(bearer) = auth.strip_prefix("Bearer ") {
            return ct_eq_str(bearer.trim(), expected);
        }
    }
    false
}

/// Session cookie a long-lived socket must keep re-checking.
///
/// A matching `X-Auth-Token` or bearer token is not a session, so those
/// connections are not tied to idle or absolute expiry. When no API token is
/// configured there is nothing to expire.
fn session_binding(token: Option<&str>, headers: &HeaderMap) -> Option<String> {
    let expected = token?;
    if presented_long_lived_token(expected, headers) {
        return None;
    }
    extract_cookie(headers, SESSION_COOKIE_NAME).map(str::to_string)
}

fn session_still_current(state: &AppState, cookie: Option<&str>) -> bool {
    let Some(cookie) = cookie else {
        return true;
    };
    let now = state.clock.now();
    state.sessions.lock().expect("sessions mutex poisoned").contains_live(cookie, now)
}

fn note_session_activity(state: &AppState, cookie: Option<&str>) -> bool {
    let Some(cookie) = cookie else {
        return true;
    };
    let now = state.clock.now();
    state.sessions.lock().expect("sessions mutex poisoned").authenticate(cookie, now)
}

fn request_is_https(headers: &HeaderMap, peer: IpAddr) -> bool {
    let origin_https =
        headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()).is_some_and(|origin| {
            origin.trim().get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        });
    if origin_https {
        return true;
    }
    let forwarded_https = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"));
    forwarded_https && peer.is_loopback()
}

fn session_cookie(value: &str, max_age: u64, secure: bool) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE_NAME}={value}; HttpOnly; SameSite=Strict; Path=/; Max-Age={max_age}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

fn too_many_auth_attempts(retry_after_secs: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [
            (header::RETRY_AFTER, retry_after_secs.to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        Json(json!({
            "ok": false,
            "error": "Too many authentication attempts",
            "retry_after": retry_after_secs,
        })),
    )
        .into_response()
}

async fn security_middleware(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    let is_public = (path == "/" && method == Method::GET)
        || (path == "/api/auth" && method == Method::POST)
        || (path == "/api/logout" && method == Method::POST);

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
        .route("/api/logout", post(post_logout))
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
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<AuthPayload>,
) -> Result<Response> {
    let Some(ref expected) = state.token else {
        return Ok(Json(json!({ "ok": true, "auth_required": false })).into_response());
    };
    let peer_ip = peer.ip();
    let now = state.clock.now();
    let accepted = ct_eq_str(payload.token.trim(), expected);
    let decision = state
        .auth_failures
        .lock()
        .expect("auth failure mutex poisoned")
        .observe(peer_ip, now, accepted);
    match decision {
        AuthDecision::Lockout { retry_after_secs } => {
            return Ok(too_many_auth_attempts(retry_after_secs));
        }
        AuthDecision::Reject => return Err(AppError::Unauthorized("Invalid API token".into())),
        AuthDecision::Allow => {}
    }
    let session = generate_session_token()?;
    state.sessions.lock().expect("sessions mutex poisoned").insert(SessionRecord {
        token: session.clone(),
        created: now,
        last_seen: now,
    });
    let cookie =
        session_cookie(&session, SESSION_COOKIE_MAX_AGE_SECS, request_is_https(&headers, peer_ip));
    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie), (header::CACHE_CONTROL, "no-store".to_string())],
        Json(json!({ "ok": true })),
    )
        .into_response())
}

async fn post_logout(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(cookie_val) = extract_cookie(&headers, SESSION_COOKIE_NAME) {
        state.sessions.lock().expect("sessions mutex poisoned").remove_token(cookie_val);
    }
    let cookie = session_cookie("", 0, request_is_https(&headers, peer.ip()));
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie), (header::CACHE_CONTROL, "no-store".to_string())],
        Json(json!({ "ok": true })),
    )
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
    app: Arc<AppState>,
    session: Option<String>,
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

    let init = SseState {
        backlog,
        last_sent_id: last_id,
        rx,
        shutdown_rx: state.shutdown_rx.clone(),
        session: session_binding(state.token.as_deref().map(String::as_str), &headers),
        app: state,
    };

    let stream = stream::unfold(init, |mut st| async move {
        if *st.shutdown_rx.borrow() || !session_still_current(&st.app, st.session.as_deref()) {
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
                () = tokio::time::sleep(SESSION_WATCH_INTERVAL) => {
                    if !session_still_current(&st.app, st.session.as_deref()) {
                        return None;
                    }
                    continue;
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

async fn ws_hid(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let session = session_binding(state.token.as_deref().map(String::as_str), &headers);
    ws.max_message_size(MAX_WS_MSG_BYTES)
        .on_upgrade(move |socket| handle_ws_hid(socket, state, session))
}

async fn send_ws_err(socket: &mut WebSocket, err: &AppError) {
    let _ = socket
        .send(Message::Text(json!({ "ok": false, "error": err.to_string() }).to_string().into()))
        .await;
}

async fn handle_ws_hid(mut socket: WebSocket, state: Arc<AppState>, session: Option<String>) {
    let active = state.telemetry.ws_connected();
    state.telemetry.log(
        "ok",
        "http",
        format!("WebSocket HID channel live (active={active})"),
        None,
    );
    let _guard = WsGuard(state.telemetry.clone());
    let mut shutdown_rx = state.shutdown_rx.clone();
    let mut session_watch = tokio::time::interval(SESSION_WATCH_INTERVAL);
    session_watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if *shutdown_rx.borrow() || !session_still_current(&state, session.as_deref()) {
            let _ = socket.close().await;
            break;
        }
        let next_msg = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                let _ = socket.close().await;
                break;
            }
            _ = session_watch.tick() => continue,
            incoming = socket.recv() => incoming,
        };
        let Some(Ok(msg)) = next_msg else {
            break;
        };
        if !note_session_activity(&state, session.as_deref()) {
            let _ = socket.close().await;
            break;
        }
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
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let mode = MirrorStreamMode::from_query(q.mode.as_deref());
    let session = session_binding(state.token.as_deref().map(String::as_str), &headers);
    ws.max_message_size(MAX_WS_MSG_BYTES)
        .on_upgrade(move |socket| handle_ws_screen(socket, state, mode, session))
}

async fn handle_ws_screen(
    mut socket: WebSocket,
    state: Arc<AppState>,
    mode: MirrorStreamMode,
    session: Option<String>,
) {
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
    let mut session_watch = tokio::time::interval(SESSION_WATCH_INTERVAL);
    session_watch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if *shutdown_rx.borrow() || !session_still_current(&state, session.as_deref()) {
            let _ = socket.close().await;
            break;
        }
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => {
                let _ = socket.close().await;
                break;
            }
            _ = session_watch.tick() => continue,
            maybe_chunk = rx.recv() => {
                if !session_still_current(&state, session.as_deref()) {
                    let _ = socket.close().await;
                    break;
                }
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
                    Some(Ok(_)) => {
                        if !note_session_activity(&state, session.as_deref()) {
                            let _ = socket.close().await;
                            break;
                        }
                    }
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

    #[test]
    fn auth_backoff_grows_exponentially_and_the_map_stays_bounded() {
        use std::net::Ipv4Addr;

        let mut tracker = AuthFailureTracker::default();
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8));
        let start = Instant::now();
        for _ in 0..AUTH_FREE_FAILURES {
            assert!(matches!(tracker.observe(ip, start, false), AuthDecision::Reject));
        }
        match tracker.observe(ip, start, false) {
            AuthDecision::Lockout { retry_after_secs } => assert_eq!(retry_after_secs, 1),
            AuthDecision::Allow | AuthDecision::Reject => panic!("expected the first lockout"),
        }
        let during = start + Duration::from_millis(500);
        match tracker.observe(ip, during, false) {
            AuthDecision::Lockout { retry_after_secs } => assert_eq!(retry_after_secs, 1),
            AuthDecision::Allow | AuthDecision::Reject => panic!("lockout should hold"),
        }
        let after = start + Duration::from_secs(1);
        match tracker.observe(ip, after, false) {
            AuthDecision::Lockout { retry_after_secs } => assert_eq!(retry_after_secs, 2),
            AuthDecision::Allow | AuthDecision::Reject => panic!("backoff should double"),
        }
        assert!(matches!(tracker.observe(ip, after, true), AuthDecision::Allow));
        assert!(matches!(tracker.observe(ip, after, false), AuthDecision::Reject));
        assert_eq!(backoff_secs(AUTH_FREE_FAILURES + 1 + 20), AUTH_BACKOFF_MAX_SECS);

        let mut bounded = AuthFailureTracker::default();
        for i in 0..(MAX_TRACKED_AUTH_IPS + 1) {
            let addr =
                IpAddr::V4(Ipv4Addr::from(0x0A00_0000_u32.wrapping_add(u32::try_from(i).unwrap())));
            let _ = bounded.observe(addr, start, false);
        }
        assert!(bounded.by_ip.len() <= MAX_TRACKED_AUTH_IPS);
        assert!(bounded.order.len() <= MAX_TRACKED_AUTH_IPS);
        let first = IpAddr::V4(Ipv4Addr::from(0x0A00_0000_u32));
        assert!(!bounded.by_ip.contains_key(&first));
    }

    #[test]
    fn session_expires_on_idle_and_absolute_lifetime() {
        let created = Instant::now();
        let fresh = SessionRecord { token: "t".into(), created, last_seen: created };
        assert!(session_is_live(&fresh, created + Duration::from_secs(29 * 60)));
        assert!(!session_is_live(&fresh, created + Duration::from_secs(30 * 60)));
        let active = SessionRecord {
            token: "t".into(),
            created,
            last_seen: created + Duration::from_secs(11 * 60 * 60 + 50 * 60),
        };
        assert!(session_is_live(&active, active.last_seen));
        assert!(!session_is_live(&active, created + SESSION_ABSOLUTE_TTL));
    }

    #[test]
    fn bearer_sockets_are_not_session_bound_and_cookie_sockets_are() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, HeaderValue::from_static("otg_session=abc"));
        assert_eq!(session_binding(None, &headers), None);
        let token = "test-token-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        headers.insert("x-auth-token", HeaderValue::from_static(token));
        assert_eq!(session_binding(Some(token), &headers), None);
        headers.remove("x-auth-token");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static(concat!(
                "Bearer ",
                "test-token-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )),
        );
        assert_eq!(session_binding(Some(token), &headers), None);
        headers.remove(header::AUTHORIZATION);
        assert_eq!(session_binding(Some(token), &headers).as_deref(), Some("abc"));

        let created = Instant::now();
        let mut table = SessionTable::default();
        table.insert(SessionRecord { token: "abc".into(), created, last_seen: created });
        assert!(table.contains_live("abc", created + Duration::from_secs(29 * 60)));
        assert!(!table.contains_live("abc", created + Duration::from_secs(30 * 60)));
        table.insert(SessionRecord { token: "abc".into(), created, last_seen: created });
        table.remove_token("abc");
        assert!(!table.contains_live("abc", created));
    }
}
