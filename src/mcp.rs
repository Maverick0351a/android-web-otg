//! Built-in pure-Rust stdio MCP (Model Context Protocol) server (`android-web-otg --mcp`).
//! Speaks newline-delimited JSON-RPC 2.0 over stdin/stdout with zero extra dependencies.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::aoa::HidBackend;
use crate::mirror::ScreenMirrorBackend;
use crate::telemetry::TelemetryStore;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Encode arbitrary bytes as standard RFC 4648 Base64 (used for inline PNG screenshots in MCP).
#[must_use]
pub fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 0x3F) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 0x3F) as usize] as char } else { '=' });
    }
    out
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "otg_status",
            "description": "Inspect USB AOAv2 OTG connection state, ADB mirror state, and local telemetry metrics.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "otg_specs",
            "description": "Read raw pre-ADB USB device descriptors (VID:PID, AOAv2 version, speed, power, interfaces).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "otg_mouse",
            "description": "Inject AOAv2 HID relative mouse movement, scroll, or click (works before ADB is enabled).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dx": { "type": "integer", "minimum": -127, "maximum": 127 },
                    "dy": { "type": "integer", "minimum": -127, "maximum": 127 },
                    "buttons": { "type": "integer", "minimum": 0, "maximum": 7 },
                    "vscroll": { "type": "integer", "minimum": -127, "maximum": 127 },
                    "hscroll": { "type": "integer", "minimum": -127, "maximum": 127 },
                    "click": { "type": "string", "enum": ["left", "right", "middle"] }
                }
            }
        },
        {
            "name": "otg_swipe_up",
            "description": "Send an upward drag-swipe gesture over AOAv2 HID to open the lockscreen PIN prompt or app drawer.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "otg_key",
            "description": "Send an AOAv2 HID shortcut (wake, home, back, enter, tab, settings, talkback_next, talkback_prev, talkback_activate, etc.) or raw modifier+scancode.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "shortcut": { "type": "string" },
                    "mod": { "type": "integer", "minimum": 0, "maximum": 255 },
                    "code": { "type": "integer", "minimum": 0, "maximum": 255 }
                }
            }
        },
        {
            "name": "otg_text",
            "description": "Type ASCII text over AOAv2 HID keyboard (for non-secret input such as search queries, Wi-Fi SSIDs, or package names).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "maxLength": 512 },
                    "enter": { "type": "boolean" }
                },
                "required": ["text"]
            }
        },
        {
            "name": "otg_screen_capture",
            "description": "Capture the current Android phone screen as an inline PNG image via ADB (requires USB Debugging).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "otg_tap",
            "description": "Tap absolute phone screen pixel coordinates (x, y) via ADB input tap.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x": { "type": "integer", "minimum": 0 },
                    "y": { "type": "integer", "minimum": 0 }
                },
                "required": ["x", "y"]
            }
        },
        {
            "name": "otg_swipe",
            "description": "Swipe between two phone pixel coordinates via ADB input swipe.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "x1": { "type": "integer", "minimum": 0 },
                    "y1": { "type": "integer", "minimum": 0 },
                    "x2": { "type": "integer", "minimum": 0 },
                    "y2": { "type": "integer", "minimum": 0 },
                    "duration_ms": { "type": "integer", "minimum": 50, "maximum": 3000 }
                },
                "required": ["x1", "y1", "x2", "y2"]
            }
        }
    ])
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_err(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_text(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }] })
}

fn tool_err(text: String) -> Value {
    json!({ "isError": true, "content": [{ "type": "text", "text": text }] })
}

async fn dispatch_tool(
    name: &str,
    args: &Value,
    bridge: &Arc<dyn HidBackend>,
    mirror: &Arc<dyn ScreenMirrorBackend>,
    telemetry: &Arc<TelemetryStore>,
) -> Value {
    let preferred_serial =
        bridge.status_snapshot().device.and_then(|d| (!d.serial.is_empty()).then_some(d.serial));

    match name {
        "otg_status" => {
            let st = bridge.status_snapshot();
            let mir = mirror.status(preferred_serial).await;
            let payload = json!({
                "usb": st,
                "mirror": mir,
                "telemetry": telemetry.metrics(),
            });
            tool_text(serde_json::to_string_pretty(&payload).unwrap_or_default())
        }
        "otg_specs" => {
            let st = bridge.status_snapshot();
            let payload = json!({
                "connected": st.connected,
                "usb": st.device,
                "error": st.error,
            });
            tool_text(serde_json::to_string_pretty(&payload).unwrap_or_default())
        }
        "otg_mouse" => {
            let dx = args.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32;
            let dy = args.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32;
            let buttons = args.get("buttons").and_then(Value::as_u64).map(|b| b as u8);
            let vscroll = args.get("vscroll").and_then(Value::as_i64).unwrap_or(0) as i32;
            let hscroll = args.get("hscroll").and_then(Value::as_i64).unwrap_or(0) as i32;
            let click = args.get("click").and_then(Value::as_str).map(str::to_string);
            match bridge.send_mouse(dx, dy, buttons, vscroll, hscroll, click).await {
                Ok(()) => tool_text("Sent mouse HID report.".into()),
                Err(e) => tool_err(e.to_string()),
            }
        }
        "otg_swipe_up" => match bridge.swipe_up().await {
            Ok(()) => tool_text("Sent AOAv2 HID Swipe Up gesture.".into()),
            Err(e) => tool_err(e.to_string()),
        },
        "otg_key" => {
            let mod_mask = args.get("mod").and_then(Value::as_u64).unwrap_or(0) as u8;
            let code = args.get("code").and_then(Value::as_u64).unwrap_or(0) as u8;
            let shortcut = args.get("shortcut").and_then(Value::as_str).map(str::to_string);
            match bridge.send_key(mod_mask, code, shortcut).await {
                Ok(()) => tool_text("Sent keyboard HID event.".into()),
                Err(e) => tool_err(e.to_string()),
            }
        }
        "otg_text" => {
            let Some(text) = args.get("text").and_then(Value::as_str) else {
                return tool_err("Missing required string parameter: text".into());
            };
            let enter = args.get("enter").and_then(Value::as_bool).unwrap_or(false);
            match bridge.send_text(text.to_string(), enter).await {
                Ok(n) => tool_text(format!("Typed {n} ASCII characters over HID.")),
                Err(e) => tool_err(e.to_string()),
            }
        }
        "otg_screen_capture" => {
            let st = mirror.status(preferred_serial.clone()).await;
            match mirror.capture_png(preferred_serial).await {
                Ok(png) => {
                    let b64 = encode_base64(&png);
                    let dim = match (st.screen_width, st.screen_height) {
                        (Some(w), Some(h)) => format!("{w}x{h}"),
                        _ => "unknown".into(),
                    };
                    json!({
                        "content": [
                            { "type": "image", "data": b64, "mimeType": "image/png" },
                            { "type": "text", "text": format!("Captured {} bytes (device={}, resolution={dim})", png.len(), st.serial.as_deref().unwrap_or("default")) }
                        ]
                    })
                }
                Err(e) => tool_err(e.to_string()),
            }
        }
        "otg_tap" => {
            let Some(x) = args.get("x").and_then(Value::as_u64) else {
                return tool_err("Missing integer parameter: x".into());
            };
            let Some(y) = args.get("y").and_then(Value::as_u64) else {
                return tool_err("Missing integer parameter: y".into());
            };
            match mirror.tap(preferred_serial, x as u32, y as u32).await {
                Ok(()) => tool_text(format!("Tapped ({x}, {y}).")),
                Err(e) => tool_err(e.to_string()),
            }
        }
        "otg_swipe" => {
            let Some(x1) = args.get("x1").and_then(Value::as_u64) else {
                return tool_err("Missing integer parameter: x1".into());
            };
            let Some(y1) = args.get("y1").and_then(Value::as_u64) else {
                return tool_err("Missing integer parameter: y1".into());
            };
            let Some(x2) = args.get("x2").and_then(Value::as_u64) else {
                return tool_err("Missing integer parameter: x2".into());
            };
            let Some(y2) = args.get("y2").and_then(Value::as_u64) else {
                return tool_err("Missing integer parameter: y2".into());
            };
            let duration_ms = args.get("duration_ms").and_then(Value::as_u64).unwrap_or(280) as u32;
            match mirror
                .swipe(preferred_serial, x1 as u32, y1 as u32, x2 as u32, y2 as u32, duration_ms)
                .await
            {
                Ok(()) => {
                    tool_text(format!("Swiped ({x1},{y1}) -> ({x2},{y2}) in {duration_ms}ms."))
                }
                Err(e) => tool_err(e.to_string()),
            }
        }
        other => tool_err(format!("Unknown tool: {other}")),
    }
}

/// Process a single JSON-RPC 2.0 request line; returns `None` for notifications.
pub async fn handle_rpc_line(
    line: &str,
    bridge: &Arc<dyn HidBackend>,
    mirror: &Arc<dyn ScreenMirrorBackend>,
    telemetry: &Arc<TelemetryStore>,
) -> Option<Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let req: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(err) => {
            return Some(rpc_err(Value::Null, -32700, &format!("Parse error: {err}")));
        }
    };
    let Some(method) = req.get("method").and_then(Value::as_str) else {
        return req.get("id").cloned().map(|id| rpc_err(id, -32600, "Invalid Request"));
    };
    let id = req.get("id").cloned();

    // JSON-RPC notifications have no `id` and expect no reply.
    let req_id = id?;

    match method {
        "initialize" => Some(rpc_ok(
            req_id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "android-web-otg",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )),
        "ping" => Some(rpc_ok(req_id, json!({}))),
        "tools/list" => Some(rpc_ok(req_id, json!({ "tools": tool_definitions() }))),
        "tools/call" => {
            let params = req.get("params").unwrap_or(&Value::Null);
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(rpc_err(req_id, -32602, "Missing params.name"));
            };
            let args = params.get("arguments").unwrap_or(&Value::Null);
            let res = dispatch_tool(name, args, bridge, mirror, telemetry).await;
            Some(rpc_ok(req_id, res))
        }
        other => Some(rpc_err(req_id, -32601, &format!("Method not found: {other}"))),
    }
}

/// Run the stdio MCP loop until EOF on stdin or shutdown signal. Logs go strictly to stderr.
pub async fn run_stdio_mcp(
    bridge: Arc<dyn HidBackend>,
    mirror: Arc<dyn ScreenMirrorBackend>,
    telemetry: Arc<TelemetryStore>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> std::io::Result<()> {
    eprintln!(
        "android-web-otg v{} stdio MCP server ready (JSON-RPC 2.0 on stdin/stdout)",
        env!("CARGO_PKG_VERSION")
    );
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        let next_line = tokio::select! {
            biased;
            _ = shutdown_rx.changed() => break,
            line = reader.next_line() => line?,
        };
        let Some(line) = next_line else {
            break;
        };
        if let Some(resp) = handle_rpc_line(&line, &bridge, &mirror, &telemetry).await {
            let serialized = serde_json::to_string(&resp)?;
            stdout.write_all(serialized.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::BoxFuture;
    use tokio::sync::mpsc;

    use crate::aoa::{BridgeStatus, UsbSpecs};
    use crate::error::Result;
    use crate::mirror::{MirrorState, MirrorStatus, MirrorStreamMode, ScreenChunk};
    use zeroize::Zeroizing;

    struct DummyHid;
    impl HidBackend for DummyHid {
        fn status_snapshot(&self) -> BridgeStatus {
            BridgeStatus {
                connected: true,
                device: Some(UsbSpecs {
                    vid: "0e8d".into(),
                    pid: "2008".into(),
                    product: "Dummy Phone".into(),
                    manufacturer: "Dummy".into(),
                    serial: "SER123".into(),
                    bus: "1".into(),
                    address: 2,
                    aoa_version: 2,
                    usb_speed: "High Speed (480 Mbps)".into(),
                    usb_config: "mtp".into(),
                    max_power_ma: Some(500),
                    interfaces: vec![],
                }),
                error: None,
            }
        }
        fn usb_specs(&self) -> Option<UsbSpecs> {
            self.status_snapshot().device
        }
        fn reconnect(&self) -> BoxFuture<'_, BridgeStatus> {
            Box::pin(async { self.status_snapshot() })
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
            Box::pin(async { Ok(()) })
        }
        fn swipe_up(&self) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn send_key(
            &self,
            _mod_mask: u8,
            _code: u8,
            _shortcut: Option<String>,
        ) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn send_text(&self, text: String, _press_enter: bool) -> BoxFuture<'_, Result<usize>> {
            Box::pin(async move { Ok(text.chars().count()) })
        }
        fn unlock_with_secret(
            &self,
            _secret: Option<Zeroizing<String>>,
            _wake_method: String,
            _press_enter: bool,
            _delay_ms: u64,
            _use_preset: bool,
        ) -> BoxFuture<'_, Result<usize>> {
            Box::pin(async { Ok(0) })
        }
        fn unlock_preset_configured(&self) -> bool {
            false
        }
        fn auto_unlock_enabled(&self) -> bool {
            false
        }
    }

    struct DummyMirror;
    impl ScreenMirrorBackend for DummyMirror {
        fn status(&self, _preferred_serial: Option<String>) -> BoxFuture<'_, MirrorStatus> {
            Box::pin(async {
                MirrorStatus {
                    adb_installed: true,
                    state: MirrorState::Ready,
                    serial: Some("SER123".into()),
                    screen_width: Some(1080),
                    screen_height: Some(2400),
                    hint: None,
                }
            })
        }
        fn capture_png(&self, _preferred_serial: Option<String>) -> BoxFuture<'_, Result<Vec<u8>>> {
            Box::pin(async { Ok(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) })
        }
        fn tap(&self, _pref: Option<String>, _x: u32, _y: u32) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn swipe(
            &self,
            _pref: Option<String>,
            _x1: u32,
            _y1: u32,
            _x2: u32,
            _y2: u32,
            _d: u32,
        ) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn spawn_stream(
            &self,
            _pref: Option<String>,
            _mode: MirrorStreamMode,
            _shutdown: watch::Receiver<bool>,
        ) -> BoxFuture<'_, Result<mpsc::Receiver<ScreenChunk>>> {
            Box::pin(async {
                let (_tx, rx) = mpsc::channel(1);
                Ok(rx)
            })
        }
    }

    #[test]
    fn encodes_rfc4648_base64() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
    }

    #[tokio::test]
    async fn mcp_dispatch_handles_lifecycle_and_all_tools() {
        let tel = Arc::new(TelemetryStore::new(50, None).unwrap());
        let hid: Arc<dyn HidBackend> = Arc::new(DummyHid);
        let mir: Arc<dyn ScreenMirrorBackend> = Arc::new(DummyMirror);

        let init = handle_rpc_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &hid,
            &mir,
            &tel,
        )
        .await
        .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "android-web-otg");

        let notif = handle_rpc_line(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &hid,
            &mir,
            &tel,
        )
        .await;
        assert!(notif.is_none());

        let listed = handle_rpc_line(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            &hid,
            &mir,
            &tel,
        )
        .await
        .unwrap();
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 9);

        let shot = handle_rpc_line(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"otg_screen_capture","arguments":{}}}"#,
            &hid,
            &mir,
            &tel,
        )
        .await
        .unwrap();
        assert_eq!(shot["result"]["content"][0]["type"], "image");
        assert_eq!(shot["result"]["content"][0]["mimeType"], "image/png");

        let bad = handle_rpc_line(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"no_such_tool","arguments":{}}}"#,
            &hid,
            &mir,
            &tel,
        )
        .await
        .unwrap();
        assert_eq!(bad["result"]["isError"], true);
    }
}
