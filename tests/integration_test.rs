use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Body;
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
use android_web_otg::server::{allowed_hosts_for_bind, build_router, AppState};
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

fn make_app(
    token: Option<&str>,
    preset: Option<&str>,
) -> (axum::Router, Arc<TelemetryStore>, Arc<MockAoaBridge>) {
    let tel = Arc::new(TelemetryStore::new(200, None).unwrap());
    let mock = Arc::new(MockAoaBridge::new(tel.clone(), preset));
    let bridge: Arc<dyn HidBackend> = mock.clone();
    let (_tx, shutdown_rx) = watch::channel(false);
    let state = Arc::new(AppState {
        bridge,
        telemetry: tel.clone(),
        token: token.map(|s| Zeroizing::new(s.to_string())),
        sessions: Mutex::new(VecDeque::new()),
        allowed_hosts: allowed_hosts_for_bind("127.0.0.1", 8095),
        shutdown_rx,
    });
    (build_router(state), tel, mock)
}

async fn read_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn ui_specs_unlock_validation_and_secret_redaction_work() {
    let secret_preset = "TOP_SECRET_PRESET_9944";
    let typed_secret = "MySecretPin42!";
    let (app, tel, mock) = make_app(None, Some(secret_preset));

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
    assert!(html.contains("specsModal"));
    assert!(html.contains("unlockSecret"));
    assert!(html.contains("logBox"));
    assert!(!html.contains("adbListBox"));

    // GET /api/status exposes preset bool only, never preset value
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
    let body_txt = serde_json::to_string(&read_json(res).await).unwrap();
    assert!(body_txt.contains("\"unlock_preset_configured\":true"));
    assert!(!body_txt.contains(secret_preset));

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
    let (app, _, _) = make_app(Some("secret-token-abc"), None);

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
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth")
                .header(header::HOST, "127.0.0.1:8095")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"token":"secret-token-abc"}"#))
                .unwrap(),
        )
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
