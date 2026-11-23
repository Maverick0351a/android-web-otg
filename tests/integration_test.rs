use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::{header, Request, StatusCode};
use futures_util::future::BoxFuture;
use http_body_util::BodyExt;
use serde_json::Value;
use tokio::sync::watch;
use tower::ServiceExt;
use zeroize::Zeroizing;

use android_web_otg::aoa::{BridgeStatus, HidBackend, UsbInterfaceSpec, UsbSpecs};
use android_web_otg::error::{AppError, Result};
use android_web_otg::hid::validate_ascii_secret;
use android_web_otg::mirror::{
    MirrorState, MirrorStatus, MirrorStreamMode, ScreenChunk, ScreenMirrorBackend,
};
use android_web_otg::server::{
    allowed_hosts_for_bind, build_router, AppState, AuthFailureTracker, Clock, ManualClock,
    SessionTable, SystemClock,
};
use android_web_otg::telemetry::TelemetryStore;

struct MockAoaBridge {
    telemetry: Arc<TelemetryStore>,
    preset: Option<String>,
    fail_usb: Mutex<bool>,
}

impl MockAoaBridge {
    fn new(telemetry: Arc<TelemetryStore>, preset: Option<&str>) -> Self {
        Self { telemetry, preset: preset.map(str::to_string), fail_usb: Mutex::new(false) }
    }

    fn mock_specs() -> UsbSpecs {
        UsbSpecs {
            vid: "0e8d".to_string(),
            pid: "2008".to_string(),
            product: "Mock Android Phone".to_string(),
            manufacturer: "MockVendor".to_string(),
            serial: "MOCK123456".to_string(),
            bus: "1".to_string(),
            address: 5,
            aoa_version: 2,
            usb_speed: "High Speed (480 Mbps)".to_string(),
            usb_config: "mtp".to_string(),
            max_power_ma: Some(500),
            interfaces: vec![UsbInterfaceSpec {
                number: 0,
                class_hex: "0x06".to_string(),
                class_name: "Imaging (MTP/PTP)".to_string(),
                subclass: 1,
                protocol: 1,
                name: "MTP".to_string(),
            }],
        }
    }
}

impl HidBackend for MockAoaBridge {
    fn status_snapshot(&self) -> BridgeStatus {
        BridgeStatus { connected: true, device: Some(Self::mock_specs()), error: None }
    }

    fn usb_specs(&self) -> Option<UsbSpecs> {
        Some(Self::mock_specs())
    }

    fn reconnect(&self) -> BoxFuture<'_, BridgeStatus> {
        Box::pin(async move { self.status_snapshot() })
    }

    fn send_mouse(
        &self,
        _dx: i32,
        _dy: i32,
        _buttons: Option<u8>,
        _vscroll: i32,
        _hscroll: i32,
        _click: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if *self.fail_usb.lock().unwrap() {
                return Err(AppError::Usb("endpoint stalled".into()));
            }
            self.telemetry.record_hid("mouse", 0.5);
            Ok(())
        })
    }

    fn swipe_up(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if *self.fail_usb.lock().unwrap() {
                return Err(AppError::Usb("No Android USB device detected".into()));
            }
            self.telemetry.record_hid("mouse", 0.4);
            self.telemetry.log("info", "hid", "Macro executed: Swipe Up (unlock)", None);
            Ok(())
        })
    }

    fn send_key(
        &self,
        _mod_mask: u8,
        _code: u8,
        shortcut: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.telemetry.record_hid("key", 0.3);
            self.telemetry.log(
                "info",
                "hid",
                format!("Key event: {}", shortcut.as_deref().unwrap_or("raw")),
                None,
            );
            Ok(())
        })
    }

    fn send_text(&self, text: String, press_enter: bool) -> BoxFuture<'_, Result<usize>> {
        Box::pin(async move {
            let count = text.chars().count();
            if count > 512 {
                return Err(AppError::BadRequest("Text length exceeds 512 character limit".into()));
            }
            self.telemetry.record_hid("key", 0.3);
            self.telemetry.log(
                "info",
                "hid",
                format!("Typed text ({count} chars, enter={press_enter})"),
                None,
            );
            Ok(count)
        })
    }

    fn unlock_with_secret(
        &self,
        secret: Option<Zeroizing<String>>,
        wake_method: String,
        press_enter: bool,
        _delay_ms: u64,
        use_preset: bool,
    ) -> BoxFuture<'_, Result<usize>> {
        Box::pin(async move {
            let resolved = if use_preset {
                self.preset.as_deref().ok_or_else(|| AppError::BadRequest("No preset".into()))?
            } else {
                secret
                    .as_deref()
                    .map(String::as_str)
                    .ok_or_else(|| AppError::BadRequest("Missing secret".into()))?
            };
            if let Err(ch) = validate_ascii_secret(resolved) {
                return Err(AppError::BadRequest(format!("Non-ASCII char '{ch}'")));
            }
            let len = resolved.chars().count();
            self.telemetry.record_hid("key", 0.6);
            self.telemetry.log(
                "info",
                "hid",
                format!("Unlock sequence sent ({len} chars, wake={wake_method}, enter={press_enter}, preset={use_preset})"),
                None,
            );
            Ok(len)
        })
    }

    fn unlock_preset_configured(&self) -> bool {
        self.preset.is_some()
    }

    fn auto_unlock_enabled(&self) -> bool {
        false
    }
}

type SwipeRecord = (u32, u32, u32, u32, u32);

struct MockMirrorBackend {
    taps: Mutex<Vec<(u32, u32)>>,
    swipes: Mutex<Vec<SwipeRecord>>,
}

impl MockMirrorBackend {
    fn new() -> Self {
        Self { taps: Mutex::new(Vec::new()), swipes: Mutex::new(Vec::new()) }
    }
}

impl ScreenMirrorBackend for MockMirrorBackend {
    fn status(&self, _preferred_serial: Option<String>) -> BoxFuture<'_, MirrorStatus> {
        Box::pin(async {
            MirrorStatus {
                adb_installed: true,
                state: MirrorState::Ready,
                serial: Some("MOCK123456".into()),
                screen_width: Some(1080),
                screen_height: Some(2400),
                hint: None,
            }
        })
    }

    fn capture_png(&self, _preferred_serial: Option<String>) -> BoxFuture<'_, Result<Vec<u8>>> {
        Box::pin(async {
            Ok(vec![
                0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H',
                b'D', b'R',
            ])
        })
    }

    fn tap(&self, _preferred_serial: Option<String>, x: u32, y: u32) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.taps.lock().unwrap().push((x, y));
            Ok(())
        })
    }

    fn swipe(
        &self,
        _preferred_serial: Option<String>,
        x1: u32,
        y1: u32,
        x2: u32,
        y2: u32,
        duration_ms: u32,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.swipes.lock().unwrap().push((x1, y1, x2, y2, duration_ms));
            Ok(())
        })
    }

    fn spawn_stream(
        &self,
        _preferred_serial: Option<String>,
        _mode: MirrorStreamMode,
        _shutdown_rx: watch::Receiver<bool>,
    ) -> BoxFuture<'_, Result<tokio::sync::mpsc::Receiver<ScreenChunk>>> {
        Box::pin(async {
            let (_tx, rx) = tokio::sync::mpsc::channel(4);
            Ok(rx)
        })
    }
}

fn make_app(
    token: Option<&str>,
    preset: Option<&str>,
) -> (axum::Router, Arc<TelemetryStore>, Arc<MockAoaBridge>, Arc<MockMirrorBackend>) {
    let tel = Arc::new(TelemetryStore::new(200, None).unwrap());
    let mock = Arc::new(MockAoaBridge::new(tel.clone(), preset));
    let bridge: Arc<dyn HidBackend> = mock.clone();
    let mock_mirror = Arc::new(MockMirrorBackend::new());
    let mirror: Arc<dyn ScreenMirrorBackend> = mock_mirror.clone();
    let (_tx, shutdown_rx) = watch::channel(false);
    let state = Arc::new(AppState {
        bridge,
        mirror,
        telemetry: tel.clone(),
        token: token.map(|s| Zeroizing::new(s.to_string())),
        sessions: Mutex::new(SessionTable::default()),
        auth_failures: Mutex::new(AuthFailureTracker::default()),
        clock: Arc::new(SystemClock),
        allowed_hosts: allowed_hosts_for_bind("127.0.0.1", 8095),
        shutdown_rx,
    });
    (build_router(state), tel, mock, mock_mirror)
}

fn make_app_with_clock(token: Option<&str>, clock: Arc<ManualClock>) -> axum::Router {
    let tel = Arc::new(TelemetryStore::new(200, None).unwrap());
    let mock = Arc::new(MockAoaBridge::new(tel.clone(), None));
    let bridge: Arc<dyn HidBackend> = mock;
    let mock_mirror = Arc::new(MockMirrorBackend::new());
    let mirror: Arc<dyn ScreenMirrorBackend> = mock_mirror;
    let (_tx, shutdown_rx) = watch::channel(false);
    let state_clock: Arc<dyn Clock> = clock;
    let state = Arc::new(AppState {
        bridge,
        mirror,
        telemetry: tel,
        token: token.map(|s| Zeroizing::new(s.to_string())),
        sessions: Mutex::new(SessionTable::default()),
        auth_failures: Mutex::new(AuthFailureTracker::default()),
        clock: state_clock,
        allowed_hosts: allowed_hosts_for_bind("127.0.0.1", 8095),
        shutdown_rx,
    });
    build_router(state)
}

const TEST_API_TOKEN: &str = "test-token-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn with_peer(mut req: Request<Body>, ip: IpAddr) -> Request<Body> {
    req.extensions_mut().insert(ConnectInfo(SocketAddr::new(ip, 41234)));
    req
}

fn loopback() -> IpAddr {
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

async fn read_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn ui_specs_unlock_validation_and_secret_redaction_work() {
    let secret_preset = "TOP_SECRET_PRESET_9944";
    let typed_secret = "MySecretPin42!";
    let (app, tel, mock, mock_mirror) = make_app(None, Some(secret_preset));

    // GET / includes security headers and lean UI (without removed ADB elements)
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::HOST, "127.0.0.1:8095")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    assert_eq!(res.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
    let html_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&html_bytes);
    assert!(html.contains("phone-chassis"));
    assert!(html.contains("mirrorCanvas"));
    assert!(html.contains("btnMirror"));
    assert!(html.contains("avg/last/max"));
    assert!(html.contains("specsModal"));
    assert!(html.contains("unlockSecret"));
    assert!(html.contains("logBox"));
    assert!(!html.contains("adbListBox"));

    // GET /api/status exposes preset bool + mirror status, never preset value
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header(header::HOST, "127.0.0.1:8095")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let status_json = read_json(res).await;
    assert_eq!(status_json["mirror"]["state"], "ready");
    let body_txt = serde_json::to_string(&status_json).unwrap();
    assert!(body_txt.contains("\"unlock_preset_configured\":true"));
    assert!(!body_txt.contains(secret_preset));

    // Mirror status, PNG snapshot, and tap/swipe endpoints work cleanly
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/mirror/status")
                .header(header::HOST, "127.0.0.1:8095")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(read_json(res).await["screen_width"], 1080);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/screen/frame")
                .header(header::HOST, "127.0.0.1:8095")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers().get(header::CONTENT_TYPE).unwrap(), "image/png");
    let png_bytes = res.into_body().collect().await.unwrap().to_bytes();
    assert!(png_bytes.starts_with(&[0x89, b'P', b'N', b'G']));

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mirror/tap")
                .header(header::HOST, "127.0.0.1:8095")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"x":540,"y":1200}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(mock_mirror.taps.lock().unwrap().as_slice(), &[(540, 1200)]);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mirror/swipe")
                .header(header::HOST, "127.0.0.1:8095")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"x1":540,"y1":1800,"x2":540,"y2":600,"duration_ms":300}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(mock_mirror.swipes.lock().unwrap().as_slice(), &[(540, 1800, 540, 600, 300)]);

    // GET /api/specs returns pure USB descriptors
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/specs")
                .header(header::HOST, "127.0.0.1:8095")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let specs_json = read_json(res).await;
    assert_eq!(specs_json["usb"]["product"], "Mock Android Phone");
    assert!(specs_json.get("adb_authorized").is_none());

    // Non-ASCII secret rejected cleanly with 400
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/unlock")
                .header(header::HOST, "127.0.0.1:8095")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"secret":"Pass€","wake_method":"swipe"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Oversized text rejected cleanly with 400
    let huge_text = "a".repeat(600);
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/text")
                .header(header::HOST, "127.0.0.1:8095")
                .header("Content-Type", "application/json")
                .body(Body::from(format!(r#"{{"text":"{huge_text}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // Valid explicit & preset unlocks never leak secrets to telemetry JSON/JSONL
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/unlock")
                .header(header::HOST, "127.0.0.1:8095")
                .header("Content-Type", "application/json")
                .body(Body::from(format!(r#"{{"secret":"{typed_secret}","wake_method":"swipe"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/unlock")
                .header(header::HOST, "127.0.0.1:8095")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"use_preset":true,"wake_method":"space"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let ndjson = tel.export_jsonl();
    assert!(!ndjson.contains(typed_secret));
    assert!(!ndjson.contains(secret_preset));

    // USB error returns 503 Service Unavailable (not 200 OK)
    *mock.fail_usb.lock().unwrap() = true;
    let res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/swipe_up")
                .header(header::HOST, "127.0.0.1:8095")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn origin_host_guard_and_cookie_header_auth_enforce_security() {
    let (app, _, _, _) = make_app(Some("secret-token-abc"), None);

    // Wrong Host blocked (DNS rebinding protection)
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header(header::HOST, "attacker.example:8095")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Cross-origin Origin rejected on POST and /ws/hid (anti-CSWSH / anti-CSRF)
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/swipe_up")
                .header(header::HOST, "127.0.0.1:8095")
                .header(header::ORIGIN, "https://evil.example")
                .header("X-Auth-Token", "secret-token-abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ws/hid")
                .header(header::HOST, "127.0.0.1:8095")
                .header("X-Auth-Token", "secret-token-abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/ws/screen")
                .header(header::HOST, "127.0.0.1:8095")
                .header("X-Auth-Token", "secret-token-abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Query-string ?token= is explicitly rejected
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status?token=secret-token-abc")
                .header(header::HOST, "127.0.0.1:8095")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // POST /api/auth issues HttpOnly SameSite=Strict session cookie
    let res = app
        .clone()
        .oneshot(with_peer(
            Request::builder()
                .method("POST")
                .uri("/api/auth")
                .header(header::HOST, "127.0.0.1:8095")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"token":"secret-token-abc"}"#))
                .unwrap(),
            loopback(),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let set_cookie = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
    let cookie_kv = set_cookie.split(';').next().unwrap();

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header(header::HOST, "127.0.0.1:8095")
                .header(header::COOKIE, cookie_kv)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Header auth (Bearer & X-Auth-Token) accepted
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/telemetry")
                .header(header::HOST, "127.0.0.1:8095")
                .header("Authorization", "Bearer secret-token-abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

fn auth_request(token: &str, peer: IpAddr) -> Request<Body> {
    with_peer(
        Request::builder()
            .method("POST")
            .uri("/api/auth")
            .header(header::HOST, "127.0.0.1:8095")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"token":"{token}"}}"#)))
            .unwrap(),
        peer,
    )
}

async fn post_auth(
    app: &axum::Router,
    token: &str,
    peer: IpAddr,
    extra: &[(&str, &str)],
) -> axum::response::Response {
    let mut req = auth_request(token, peer);
    for (name, value) in extra {
        req.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
    }
    app.clone().oneshot(req).await.unwrap()
}

#[tokio::test]
async fn repeated_failures_from_same_ip_are_rate_limited_other_ips_and_success_are_not() {
    let (app, _, _, _) = make_app(Some(TEST_API_TOKEN), None);
    let attacker = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));
    let other = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 11));

    for attempt in 1..=5 {
        let res = post_auth(&app, "wrong-token", attacker, &[]).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "attempt {attempt} should still be 401");
    }

    // X-Forwarded-For must not select a different bucket than ConnectInfo.
    let res = post_auth(
        &app,
        "wrong-token",
        attacker,
        &[("x-forwarded-for", "198.51.100.9"), ("x-forwarded-proto", "https")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry = res.headers().get(header::RETRY_AFTER).expect("Retry-After").to_str().unwrap();
    assert_eq!(retry, "1", "first lockout uses a 1s exponential backoff base");

    // A different peer is not locked, even if it spoofs the attacker's address.
    let res = post_auth(&app, "wrong-token", other, &[("x-forwarded-for", "203.0.113.10")]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // A correct token clears the lockout for that IP.
    let res = post_auth(&app, TEST_API_TOKEN, attacker, &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = post_auth(&app, "wrong-token", attacker, &[]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_cookie_includes_max_age_httponly_and_samesite() {
    let (app, _, _, _) = make_app(Some(TEST_API_TOKEN), None);
    let res = post_auth(&app, TEST_API_TOKEN, loopback(), &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Max-Age=43200"), "{cookie}");
    assert!(!cookie.split(';').any(|part| part.trim().eq_ignore_ascii_case("secure")), "{cookie}");
}

#[tokio::test]
async fn auth_cookie_secure_flag_follows_https_origin_or_loopback_forwarded_proto() {
    let (app, _, _, _) = make_app(Some(TEST_API_TOKEN), None);

    let res = post_auth(
        &app,
        TEST_API_TOKEN,
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 20)),
        &[("origin", "https://127.0.0.1:8095")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(cookie.split(';').any(|part| part.trim() == "Secure"), "{cookie}");

    let res = post_auth(&app, TEST_API_TOKEN, loopback(), &[("x-forwarded-proto", "https")]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(cookie.split(';').any(|part| part.trim() == "Secure"), "{cookie}");

    let res = post_auth(
        &app,
        TEST_API_TOKEN,
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 21)),
        &[("x-forwarded-proto", "https")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let cookie = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(
        !cookie.split(';').any(|part| part.trim().eq_ignore_ascii_case("secure")),
        "non-loopback X-Forwarded-Proto must be ignored: {cookie}"
    );
}

#[tokio::test]
async fn logout_removes_the_session_and_clears_the_cookie() {
    let (app, _, _, _) = make_app(Some(TEST_API_TOKEN), None);
    let res = post_auth(&app, TEST_API_TOKEN, loopback(), &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    let set_cookie = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    let cookie_kv = set_cookie.split(';').next().unwrap().to_string();

    let res = app
        .clone()
        .oneshot(with_peer(
            Request::builder()
                .method("POST")
                .uri("/api/logout")
                .header(header::HOST, "127.0.0.1:8095")
                .header(header::COOKIE, &cookie_kv)
                .body(Body::empty())
                .unwrap(),
            loopback(),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let cleared = res.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    assert!(cleared.contains("HttpOnly"), "{cleared}");
    assert!(cleared.contains("SameSite=Strict"), "{cleared}");

    let res = app
        .clone()
        .oneshot(with_peer(
            Request::builder()
                .uri("/api/status")
                .header(header::HOST, "127.0.0.1:8095")
                .header(header::COOKIE, &cookie_kv)
                .body(Body::empty())
                .unwrap(),
            loopback(),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let html_res = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::HOST, "127.0.0.1:8095")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html_bytes = html_res.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&html_bytes);
    assert!(html.contains("btnLogout"));
    assert!(html.contains("/api/logout"));
}

async fn login_cookie(app: &axum::Router, peer: IpAddr) -> String {
    let res = post_auth(app, TEST_API_TOKEN, peer, &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    res.headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn status_with(app: &axum::Router, cookie: &str, peer: IpAddr) -> StatusCode {
    app.clone()
        .oneshot(with_peer(
            Request::builder()
                .uri("/api/status")
                .header(header::HOST, "127.0.0.1:8095")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
            peer,
        ))
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn sessions_expire_after_idle_timeout_and_absolute_lifetime() {
    let peer = loopback();
    let idle_clock = Arc::new(ManualClock::new());
    let idle_app = make_app_with_clock(Some(TEST_API_TOKEN), idle_clock.clone());
    let cookie_a = login_cookie(&idle_app, peer).await;
    idle_clock.advance(std::time::Duration::from_secs(20 * 60));
    let cookie_b = login_cookie(&idle_app, peer).await;
    idle_clock.advance(std::time::Duration::from_secs(20 * 60));
    assert_eq!(status_with(&idle_app, &cookie_a, peer).await, StatusCode::UNAUTHORIZED);
    assert_eq!(status_with(&idle_app, &cookie_b, peer).await, StatusCode::OK);

    let absolute_clock = Arc::new(ManualClock::new());
    let absolute_app = make_app_with_clock(Some(TEST_API_TOKEN), absolute_clock.clone());
    let cookie = login_cookie(&absolute_app, peer).await;
    for step in 1..=35 {
        absolute_clock.advance(std::time::Duration::from_secs(20 * 60));
        assert_eq!(
            status_with(&absolute_app, &cookie, peer).await,
            StatusCode::OK,
            "session should stay live at 20-minute refresh {step}"
        );
    }
    absolute_clock.advance(std::time::Duration::from_secs(20 * 60));
    assert_eq!(status_with(&absolute_app, &cookie, peer).await, StatusCode::UNAUTHORIZED);

    let cleared = absolute_app
        .oneshot(with_peer(
            Request::builder()
                .method("POST")
                .uri("/api/logout")
                .header(header::HOST, "127.0.0.1:8095")
                .header(header::COOKIE, &cookie)
                .body(Body::empty())
                .unwrap(),
            peer,
        ))
        .await
        .unwrap();
    assert_eq!(cleared.status(), StatusCode::OK);
    let clear_cookie = cleared.headers().get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(clear_cookie.contains("Max-Age=0"), "{clear_cookie}");
}

#[test]
fn udev_microsoft_vid_is_limited_to_surface_duo_pids() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/deploy/51-android.rules");
    let text = std::fs::read_to_string(path).unwrap();
    let mtp = r#"SUBSYSTEM=="usb", ENV{DEVTYPE}=="usb_device", ENV{ID_MTP_DEVICE}=="1", MODE="0660", GROUP="plugdev", TAG+="uaccess""#;
    assert!(text.lines().any(|line| line.trim() == mtp), "MTP fallback rule must stay unchanged");

    let active: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#') && line.contains("045e"))
        .collect();
    assert_eq!(active.len(), 4, "expected four Surface Duo PID rules, got {active:?}");
    for pid in ["0c26", "0c2e", "0c2f", "0c30"] {
        assert!(
            active.iter().any(|line| {
                line.contains(r#"ATTR{idVendor}=="045e""#)
                    && line.contains(&format!(r#"ATTR{{idProduct}}=="{pid}""#))
            }),
            "missing Surface Duo PID {pid}: {active:?}"
        );
    }
    assert!(
        text.lines().any(|line| line.trim_start().starts_with('#') && line.contains("PID")),
        "unverified Microsoft PIDs should remain a commented template"
    );
}
