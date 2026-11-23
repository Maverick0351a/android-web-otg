use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use android_web_otg::adb::AdbInspector;
use android_web_otg::aoa::{BridgeStatus, HidBackend, UsbInterfaceSpec, UsbSpecs};
use android_web_otg::error::{AppError, Result};
use android_web_otg::server::{build_router, AppState};
use android_web_otg::telemetry::TelemetryStore;

struct MockAoaBridge {
    telemetry: Arc<TelemetryStore>,
    preset: Option<String>,
    calls: Mutex<Vec<String>>,
}

impl MockAoaBridge {
    fn new(telemetry: Arc<TelemetryStore>, preset: Option<&str>) -> Self {
        Self { telemetry, preset: preset.map(str::to_string), calls: Mutex::new(Vec::new()) }
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
    fn status(&self, _reconnect: bool) -> BridgeStatus {
        BridgeStatus { connected: true, device: Some(Self::mock_specs()), error: None }
    }

    fn usb_specs(&self) -> Option<UsbSpecs> {
        Some(Self::mock_specs())
    }

    fn send_mouse(
        &self,
        dx: i32,
        dy: i32,
        _buttons: Option<u8>,
        _vscroll: i32,
        _hscroll: i32,
        _click: Option<&str>,
    ) -> Result<()> {
        self.telemetry.record_hid("mouse", 0.5);
        self.calls.lock().unwrap().push(format!("mouse:{dx}:{dy}"));
        Ok(())
    }

    fn swipe_up(&self) -> Result<()> {
        self.telemetry.record_hid("mouse", 0.4);
        self.telemetry.log("info", "hid", "Macro executed: Swipe Up (unlock)", None);
        Ok(())
    }

    fn send_key(&self, _mod_mask: u8, _code: u8, shortcut: Option<&str>) -> Result<()> {
        self.telemetry.record_hid("key", 0.3);
        self.telemetry.log(
            "info",
            "hid",
            format!("Key event: {}", shortcut.unwrap_or("raw")),
            None,
        );
        Ok(())
    }

    fn send_text(&self, text: &str, press_enter: bool) -> Result<usize> {
        self.telemetry.record_hid("key", 0.3);
        let len = text.chars().count();
        self.telemetry.log(
            "info",
            "hid",
            format!("Typed text ({len} chars, enter={press_enter})"),
            None,
        );
        Ok(len)
    }

    fn unlock_with_secret(
        &self,
        secret: Option<&str>,
        wake_method: &str,
        press_enter: bool,
        _delay_ms: u64,
        use_preset: bool,
    ) -> Result<usize> {
        let resolved = if use_preset {
            self.preset.as_deref().ok_or_else(|| AppError::BadRequest("No preset".into()))?
        } else {
            secret.ok_or_else(|| AppError::BadRequest("Missing secret".into()))?
        };
        let len = resolved.chars().count();
        self.telemetry.record_hid("key", 0.6);
        self.telemetry.log(
            "info",
            "hid",
            format!("Unlock sequence sent ({len} chars, wake={wake_method}, enter={press_enter}, preset={use_preset})"),
            None,
        );
        Ok(len)
    }

    fn unlock_preset_configured(&self) -> bool {
        self.preset.is_some()
    }

    fn auto_unlock_enabled(&self) -> bool {
        false
    }
}

fn make_app(token: Option<&str>, preset: Option<&str>) -> (axum::Router, Arc<TelemetryStore>) {
    let tel = Arc::new(TelemetryStore::new(200, None));
    let bridge: Arc<dyn HidBackend> = Arc::new(MockAoaBridge::new(tel.clone(), preset));
    let adb = Arc::new(AdbInspector::new(tel.clone(), false, None));
    let state = Arc::new(AppState {
        bridge,
        adb,
        telemetry: tel.clone(),
        token: token.map(str::to_string),
    });
    (build_router(state), tel)
}

async fn read_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn ui_status_specs_and_unlock_secret_redaction_work() {
    let secret_preset = "TOP_SECRET_PRESET_9944";
    let typed_secret = "MySecretPin42!";
    let (app, tel) = make_app(None, Some(secret_preset));

    // GET /
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8_lossy(&html_bytes);
    assert!(html.contains("phone-chassis"));
    assert!(html.contains("specsModal"));
    assert!(html.contains("unlockSecret"));
    assert!(html.contains("logBox"));

    // GET /api/status must show preset bool but never expose preset value
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/api/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body_txt = serde_json::to_string(&read_json(res).await).unwrap();
    assert!(body_txt.contains("\"unlock_preset_configured\":true"));
    assert!(!body_txt.contains(secret_preset));

    // GET /api/specs returns USB specs pre-ADB
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/api/specs").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let specs_json = read_json(res).await;
    assert_eq!(specs_json["usb"]["product"], "Mock Android Phone");
    assert_eq!(specs_json["adb_authorized"], false);

    // POST /api/unlock rejects empty secret with 400
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/unlock")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"secret":"","wake_method":"swipe"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // POST /api/unlock with explicit secret
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/unlock")
                .header("Content-Type", "application/json")
                .body(Body::from(format!(r#"{{"secret":"{typed_secret}","wake_method":"swipe"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let u1 = read_json(res).await;
    assert_eq!(u1["ok"], true);
    assert_eq!(u1["sent"], typed_secret.len());

    // POST /api/unlock with server preset
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/unlock")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"use_preset":true,"wake_method":"space"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let u2 = read_json(res).await;
    assert_eq!(u2["sent"], secret_preset.len());

    // Telemetry JSON and JSONL download must NEVER contain either secret
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/api/telemetry").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let tel_str = serde_json::to_string(&read_json(res).await).unwrap();
    assert!(!tel_str.contains(typed_secret));
    assert!(!tel_str.contains(secret_preset));

    let res = app
        .clone()
        .oneshot(Request::builder().uri("/api/telemetry?download=1").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ndjson_bytes = res.into_body().collect().await.unwrap().to_bytes();
    let ndjson = String::from_utf8_lossy(&ndjson_bytes);
    assert!(!ndjson.contains(typed_secret));
    assert!(!ndjson.contains(secret_preset));
    assert!(!tel.export_jsonl().contains(typed_secret));

    // POST /api/telemetry/clear clears event log cleanly
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/telemetry/clear")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(tel.snapshot(0, None, None, 50).events.len(), 1);
}

#[tokio::test]
async fn token_authentication_enforces_401_and_accepts_headers() {
    let (app, _) = make_app(Some("secret-token-abc"), None);

    // Missing token -> 401
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/api/status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let j = read_json(res).await;
    assert_eq!(j["auth_required"], true);

    // Valid X-Auth-Token -> 200
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .header("X-Auth-Token", "secret-token-abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Valid Authorization Bearer -> 200
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/telemetry")
                .header("Authorization", "Bearer secret-token-abc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}
